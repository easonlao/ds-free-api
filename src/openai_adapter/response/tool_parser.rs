//! 工具调用解析 —— 滑动窗口检测 `<tool_calls>...</tool_calls>`，转换为结构化 tool_calls
//!
//! 算法核心：
//! - Detecting 状态：维护固定宽度 W 的扫描缓冲区，新 chunk 到来时
//!   先追加到缓冲区，扫描 `<tool_calls>`（或回退 `<tool_call>`），未找到则释放超出 W 的安全部分
//! - CollectingXml 状态：检测到标记后收集内容直到 `</tool_calls>`
//! - Done 状态：工具调用已发出，截断后续内容（防幻觉）

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use pin_project_lite::pin_project;

use log::{debug, trace, warn};

use crate::openai_adapter::OpenAIAdapterError;
use crate::openai_adapter::types::{
    ChatCompletionsResponseChunk, ChunkChoice, Delta, FunctionCall, ToolCall,
};

static CALL_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
pub(crate) const MAX_XML_BUF_LEN: usize = 64 * 1024;

pub(crate) const TOOL_CALL_START: &str = "<|tool_calls_begin|>";
pub(crate) const TOOL_CALL_END: &str = "<|tool_calls_end|>";
pub(crate) const NEW_TOOL_CALL_START: &str = "<|tool_calls_start|>";
pub(crate) const NEW_TOOL_CALL_END: &str = "<|tool_calls_end|>";
pub(crate) const NEW_TOOL_CALL_BLOCK: &str = "<|tool_call|>";
pub(crate) const NEW_TOOL_CALL_BLOCK_END: &str = "<|tool_call_end|>";
pub(crate) const NEW_TOOL_CALL_SENTINEL: &str = "<|tool_calls_sentinel|>";
pub(crate) const PIPE_TOOL_CALLS_START: &str = "<|tool_calls|>";
pub(crate) const SECTION_START: &str = "<|tool_calls_section_begin|>";
pub(crate) const SECTION_END: &str = "<|tool_calls_section_end|>";
const W: usize = 71;

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct TagConfig {
    pub starts: Vec<String>,
    pub ends: Vec<String>,
}

impl TagConfig {
    pub fn from_config(cfg: &crate::config::ToolCallTagConfig) -> Self {
        Self {
            starts: cfg.extra_starts.clone(),
            ends: cfg.extra_ends.clone(),
        }
    }
}

/// 标签字符归一化：`｜`(U+FF5C) → `|`，block 字符(U+2581-U+2588) → `_`
fn norm_tag_char(c: char) -> char {
    match c {
        '\u{FF5C}' => '|',
        c if c >= '\u{2581}' && c <= '\u{2588}' => '_',
        _ => c,
    }
}

/// 标签字符等价判断
fn eq_tag_char(a: char, b: char) -> bool {
    a == b || norm_tag_char(a) == norm_tag_char(b)
}

/// 模糊匹配标签：在 `haystack` 中查找 `partial`，支持 `｜`↔`|`、`▁`↔`_` 等价
fn fuzzy_match_tag<'a>(haystack: &'a str, partial: &str) -> Option<(usize, &'a str)> {
    let n_chars: Vec<char> = partial.chars().collect();
    let h_chars: Vec<char> = haystack.chars().collect();

    if n_chars.is_empty() || h_chars.len() < n_chars.len() {
        return None;
    }

    for start in 0..=h_chars.len() - n_chars.len() {
        let mut matched = true;
        for j in 0..n_chars.len() {
            if !eq_tag_char(n_chars[j], h_chars[start + j]) {
                matched = false;
                break;
            }
        }
        if matched {
            let byte_pos: usize = h_chars[..start].iter().map(|c| c.len_utf8()).sum();
            let tag_len: usize = h_chars[start..start + n_chars.len()]
                .iter()
                .map(|c| c.len_utf8())
                .sum();
            return Some((byte_pos, &haystack[byte_pos..byte_pos + tag_len]));
        }
    }
    None
}

fn match_start_tag<'a>(s: &'a str, tag: &str) -> Option<(usize, &'a str)> {
    let partial = tag.trim_end_matches('>');
    if let Some(pos) = s.find(partial) {
        Some((pos, &s[pos..pos + partial.len()]))
    } else {
        fuzzy_match_tag(s, partial)
    }
}

pub(crate) fn find_start_tag_with<'a>(s: &'a str, cfg: &TagConfig) -> Option<(usize, &'a str)> {
    if let Some(m) = match_start_tag(s, TOOL_CALL_START) {
        return Some(m);
    }
    if let Some(m) = match_start_tag(s, NEW_TOOL_CALL_START) {
        return Some(m);
    }
    if let Some(m) = match_start_tag(s, NEW_TOOL_CALL_SENTINEL) {
        return Some(m);
    }
    if let Some(m) = match_start_tag(s, PIPE_TOOL_CALLS_START) {
        return Some(m);
    }
    if let Some(m) = match_start_tag(s, SECTION_START) {
        return Some(m);
    }
    if let Some(m) = match_start_tag(s, TOOL_CALL_START) {
        return Some(m);
    }
    for start in &cfg.starts {
        if let Some(m) = match_start_tag(s, start) {
            return Some(m);
        }
    }
    None
}

pub(crate) fn find_end_tag_with<'a>(
    s: &'a str,
    from: usize,
    cfg: &TagConfig,
    start_tag: Option<&str>,
) -> Option<(usize, &'a str)> {
    let search = &s[from..];
    if let Some(st) = start_tag {
        let open_tag = st.trim_end_matches('>');
        let close_tag = format!("</{}>", &open_tag[1..]);
        if let Some(pos) = search.find(&close_tag) {
            let abs = from + pos;
            return Some((abs, &s[abs..abs + close_tag.len()]));
        }
        // 模糊回退：close_tag 中可能含 ｜/▁ 变体
        let close_partial = close_tag.trim_end_matches('>');
        if let Some((pos, matched)) = fuzzy_match_tag(search, close_partial) {
            let abs = from + pos;
            return Some((abs, &s[abs..abs + matched.len()]));
        }
        // pipe 标签回退：模型有时输出 </tag|> 而非 </|tag|>
        // 即闭合标签少一个 |（<|tag|> → 期望 </|tag|>，但模型输出 </tag|>）
        if let Some(pipe_tag) = open_tag.strip_prefix("<|") {
            let alt_close = format!("</{}|>", pipe_tag);
            if let Some(pos) = search.find(&alt_close) {
                let abs = from + pos;
                return Some((abs, &s[abs..abs + alt_close.len()]));
            }
            let alt_partial = alt_close.trim_end_matches('>');
            if let Some((pos, matched)) = fuzzy_match_tag(search, alt_partial) {
                let abs = from + pos;
                return Some((abs, &s[abs..abs + matched.len()]));
            }
        }
    }

    // 无论 start_tag 是否提供，都尝试已知结束标签
    let is_section_tag = start_tag.map_or(false, |s| {
        s.starts_with(SECTION_START.trim_end_matches('>'))
        || s.starts_with(PIPE_TOOL_CALLS_START.trim_end_matches('>'))
    });
    // 两轮扫描：第一轮优先找 section 级闭合，找不到则第二轮接受子标签闭合
    for end in std::iter::once(TOOL_CALL_END)
        .chain(std::iter::once(NEW_TOOL_CALL_END))
        .chain(std::iter::once(SECTION_END))
        .chain(std::iter::once(TOOL_CALL_END))
        .chain(cfg.ends.iter().map(|s| s.as_str()))
    {
        if is_section_tag && end != SECTION_END && end != TOOL_CALL_END {
            continue;
        }
        if let Some(pos) = search.find(end) {
            let abs = from + pos;
            return Some((abs, &s[abs..abs + end.len()]));
        }
        let end_partial = end.trim_end_matches('>');
        if let Some((pos, matched)) = fuzzy_match_tag(search, end_partial) {
            let abs = from + pos;
            return Some((abs, &s[abs..abs + matched.len()]));
        }
    }
    // section 标签回退：没找到 SECTION_END → 接受 TOOL_CALL_END 或 NEW_TOOL_CALL_END
    if is_section_tag {
        for end in std::iter::once(TOOL_CALL_END).chain(std::iter::once(NEW_TOOL_CALL_END)) {
            if let Some(pos) = search.find(end) {
                let abs = from + pos;
                return Some((abs, &s[abs..abs + end.len()]));
            }
            let end_partial = end.trim_end_matches('>');
            if let Some((pos, matched)) = fuzzy_match_tag(search, end_partial) {
                let abs = from + pos;
                return Some((abs, &s[abs..abs + matched.len()]));
            }
        }
    }
    if let Some(st) = start_tag
        && let Some((pos, tag)) = match_start_tag(search, st)
    {
        return Some((from + pos, &s[from + pos..from + pos + tag.len()]));
    }
    if let Some((pos, tag)) = match_start_tag(search, TOOL_CALL_START) {
        return Some((from + pos, &s[from + pos..from + pos + tag.len()]));
    }
    if let Some((pos, tag)) = match_start_tag(search, NEW_TOOL_CALL_START) {
        return Some((from + pos, &s[from + pos..from + pos + tag.len()]));
    }
    if let Some((pos, tag)) = match_start_tag(search, NEW_TOOL_CALL_SENTINEL) {
        return Some((from + pos, &s[from + pos..from + pos + tag.len()]));
    }
    if let Some((pos, tag)) = match_start_tag(search, PIPE_TOOL_CALLS_START) {
        return Some((from + pos, &s[from + pos..from + pos + tag.len()]));
    }
    if let Some((pos, tag)) = match_start_tag(search, SECTION_START) {
        return Some((from + pos, &s[from + pos..from + pos + tag.len()]));
    }
    if let Some((pos, tag)) = match_start_tag(search, TOOL_CALL_START) {
        return Some((from + pos, &s[from + pos..from + pos + tag.len()]));
    }
    for start in &cfg.starts {
        if let Some((pos, tag)) = match_start_tag(search, start) {
            return Some((from + pos, &s[from + pos..from + pos + tag.len()]));
        }
    }
    None
}

fn is_start_tag(tag: &str, cfg: &TagConfig) -> bool {
    if !tag.starts_with('<') {
        return false;
    }
    let partial = TOOL_CALL_START.trim_end_matches('>');
    let tag_norm: String = tag.chars().map(norm_tag_char).collect();
    let partial_norm: String = partial.chars().map(norm_tag_char).collect();
    if partial_norm.starts_with(&tag_norm) || tag_norm.starts_with(&partial_norm) {
        return true;
    }
    let new_partial = NEW_TOOL_CALL_START.trim_end_matches('>');
    if tag_norm.starts_with(new_partial) || new_partial.starts_with(&tag_norm) {
        return true;
    }
    let sentinel_partial = NEW_TOOL_CALL_SENTINEL.trim_end_matches('>');
    if tag_norm.starts_with(sentinel_partial) || sentinel_partial.starts_with(&tag_norm) {
        return true;
    }
    let pipe_partial = PIPE_TOOL_CALLS_START.trim_end_matches('>');
    if tag_norm.starts_with(pipe_partial) || pipe_partial.starts_with(&tag_norm) {
        return true;
    }
    let section_partial = SECTION_START.trim_end_matches('>');
    if tag_norm.starts_with(section_partial) || section_partial.starts_with(&tag_norm) {
        return true;
    }
    let calls_begin_partial = TOOL_CALL_START.trim_end_matches('>');
    if tag_norm.starts_with(calls_begin_partial) || calls_begin_partial.starts_with(&tag_norm) {
        return true;
    }
    for start in &cfg.starts {
        let p: String = start
            .trim_end_matches('>')
            .chars()
            .map(norm_tag_char)
            .collect();
        if p.starts_with(&tag_norm) || tag_norm.starts_with(&p) {
            return true;
        }
    }
    false
}

fn next_call_id() -> String {
    let n = CALL_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("call_{:016x}", n)
}

fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn safe_truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len { return s; }
    let mut i = max_len;
    while !s.is_char_boundary(i) { i -= 1; }
    &s[..i]
}

fn is_inside_code_fence(xml: &str, tag_pos: usize) -> bool {
    xml[..tag_pos].matches("```").count() % 2 == 1
}

fn repair_invalid_backslashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some(&next)
                    if matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') =>
                {
                    out.push('\\');
                    out.push(next);
                    chars.next();
                }
                Some(&next) => {
                    out.push('\\');
                    out.push('\\');
                    out.push(next);
                    chars.next();
                }
                None => {
                    out.push('\\');
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn repair_unquoted_keys(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 32);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        if (chars[i] == '{' || chars[i] == ',') && i + 1 < len {
            out.push(chars[i]);
            i += 1;
            while i < len && chars[i].is_whitespace() {
                out.push(chars[i]);
                i += 1;
            }
            if i < len && (chars[i].is_alphabetic() || chars[i] == '_') {
                let key_start = i;
                while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                if i < len && chars[i] == ':' {
                    out.push('"');
                    out.extend(&chars[key_start..i]);
                    out.push('"');
                } else {
                    out.extend(&chars[key_start..i]);
                    continue;
                }
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn repair_trailing_commas(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        let c = chars[i];
        if escaped {
            escaped = false;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\\' {
            escaped = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_string && c == ',' {
            // 跳过逗号后空白，检查是否紧接 ] 或 }
            let mut j = i + 1;
            while j < len && chars[j].is_whitespace() {
                j += 1;
            }
            if j < len && (chars[j] == ']' || chars[j] == '}') {
                i = j; // 跳到 ]/} 位置，跳过逗号
                continue;
            }
            out.push(c);
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

fn repair_single_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            escaped = false;
            out.push(c);
            continue;
        }
        if c == '\\' {
            out.push(c);
            escaped = true;
            continue;
        }
        if c == '\'' && !in_double && !escaped {
            in_single = !in_single;
            out.push('"');
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            out.push(c);
            continue;
        }
        if c == '"' && in_single {
            // 单引号字符串内遇到双引号，转义
            out.push('\\');
            out.push('"');
            continue;
        }
        out.push(c);
    }
    out
}

fn strip_control_chars(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\n' || c == '\t' || c == '\r' || !c.is_control())
        .collect()
}

fn normalize_unicode_quotes(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{201C}' | '\u{201D}' | '\u{300C}' | '\u{300D}' => '"',
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{FF02}' => '"',
            _ => c,
        })
        .collect()
}

/// JSON 字符串值中的真实换行符 → `\n` 转义
fn escape_newlines_in_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in s.chars() {
        if escaped { escaped = false; out.push(c); continue; }
        if c == '\\' && in_string { escaped = true; out.push(c); continue; }
        if c == '"' && !escaped { in_string = !in_string; out.push(c); continue; }
        if in_string && (c == '\n' || c == '\r') { out.push_str("\\n"); continue; }
        out.push(c);
    }
    out
}

fn try_json_parse(s: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(s).ok()?;
    Some(s.to_string())
}

fn repair_json(s: &str) -> Option<String> {
    let mut current = strip_control_chars(s);
    current = normalize_unicode_quotes(&current);
    if try_json_parse(&current).is_some() {
        trace!(target: "adapter", "[tc] json_repair→try step=cleanup len={}", current.len());
        return Some(current);
    }

    current = escape_newlines_in_strings(&current);
    if try_json_parse(&current).is_some() {
        trace!(target: "adapter", "[tc] json_repair→try step=escape_newlines len={}", current.len());
        return Some(current);
    }

    current = repair_invalid_backslashes(&current);
    if try_json_parse(&current).is_some() {
        trace!(target: "adapter", "[tc] json_repair→try step=backslash len={}", current.len());
        return Some(current);
    }

    current = repair_single_quotes(&current);
    if try_json_parse(&current).is_some() {
        trace!(target: "adapter", "[tc] json_repair→try step=single_quotes len={}", current.len());
        return Some(current);
    }

    current = repair_trailing_commas(&current);
    if try_json_parse(&current).is_some() {
        trace!(target: "adapter", "[tc] json_repair→try step=trailing_commas len={}", current.len());
        return Some(current);
    }

    current = repair_unquoted_keys(&current);
    if try_json_parse(&current).is_some() {
        trace!(target: "adapter", "[tc] json_repair→try step=unquoted_keys len={}", current.len());
        return Some(current);
    }

    trace!(target: "adapter", "[tc] json_repair→all_failed sample=\"{}\"",
        safe_truncate(s, 150));
    None
}

/// 归一化 pipe 变体标签：`<|tag|>` → `<tag>`，`</|tag|>` → `</tag>`
fn normalize_xml_pipes(s: &str) -> String {
    s.replace("</|", "</").replace("<|", "<").replace("|>", ">")
}

pub fn parse_tool_calls(xml: &str) -> Option<(Vec<ToolCall>, String)> {
    parse_tool_calls_with(xml, &TagConfig::from_config(&Default::default()))
}

pub fn parse_tool_calls_with(xml: &str, cfg: &TagConfig) -> Option<(Vec<ToolCall>, String)> {
    let (start, start_tag) = find_start_tag_with(xml, cfg)?;
    // start_tag 不包含尾部 >（match_start_tag 使用 trim_end_matches('>')），
    // 需跳过 > 定位到标签后的内容
    let after_start = start + start_tag.len()
        + if xml.as_bytes().get(start + start_tag.len()) == Some(&b'>') { 1 } else { 0 };
    if is_inside_code_fence(xml, start) {
        return None;
    }

    let (end, inner_end) = match find_end_tag_with(xml, after_start, cfg, Some(start_tag)) {
        Some((pos, matched_end)) => (pos + matched_end.len(), pos),
        None => (xml.len(), xml.len()),
    };
    // normalize pipe-delimited XML tags (<| → <, |> → >) before parsing
    let inner = normalize_xml_pipes(&xml[after_start..inner_end]);

    let arr = if inner.trim().starts_with('[') {
        let arr_start = inner.find('[').unwrap();
        let arr_end = inner.rfind(']').map(|p| p + 1).unwrap_or(inner.len());
        let json_str = &inner[arr_start..arr_end];
        if json_str.trim() == "[]" {
            return None;
        }
        match serde_json::from_str::<Vec<serde_json::Value>>(json_str) {
            Ok(a) => a,
            Err(_) => {
                let repaired = repair_json(json_str).unwrap_or_default();
                let obj_str = repaired.trim_start_matches('[');
                let obj_start = obj_str.find('{')?;
                let obj_end = obj_str.rfind('}').map(|p| p + 1).unwrap_or(obj_str.len());
                serde_json::from_str(&obj_str[obj_start..obj_end])
                    .ok()
                    .filter(|v: &serde_json::Value| v.is_object())
                    .map(|v| vec![v])?
            }
        }
    } else if inner.contains(NEW_TOOL_CALL_BLOCK) || inner.contains(TOOL_CALL_START) {
        let calls = parse_new_tool_calls(&inner)?;
        let remaining = xml[..start].to_string() + &xml[end..];
        return Some((calls, remaining));
    } else if let Some(obj_start) = inner.find('{') {
        let obj_end = inner.rfind('}').map(|p| p + 1).unwrap_or(inner.len());
        let json_str = &inner[obj_start..obj_end];
        let obj = serde_json::from_str(json_str)
            .ok()
            .filter(|v: &serde_json::Value| v.is_object())
            .or_else(|| {
                let repaired = repair_json(json_str)?;
                serde_json::from_str(&repaired)
                    .ok()
                    .filter(|v: &serde_json::Value| v.is_object())
            })?;
        vec![obj]
    } else {
        return parse_invoke_calls(&inner, &xml[..start], &xml[end..]);
    };

    let mut calls = Vec::new();
    for item in arr {
        let name = item.get("name")?.as_str()?.to_string();
        let arguments = match item.get("arguments") {
            Some(v) => {
                if let Some(s) = v.as_str() {
                    serde_json::from_str::<serde_json::Value>(s)
                        .ok()
                        .and_then(|obj| serde_json::to_string(&obj).ok())
                        .unwrap_or_else(|| s.to_string())
                } else {
                    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
                }
            }
            None => "{}".into(),
        };
        calls.push(ToolCall {
            id: next_call_id(),
            ty: "function".to_string(),
            function: Some(FunctionCall { name, arguments }),
            custom: None,
            index: calls.len() as u32,
        });
    }
    if calls.is_empty() {
        return None;
    }
    let remaining = xml[..start].to_string() + &xml[end..];
    Some((calls, remaining))
}

/// 解析 `<|tool_call|>{json}<|tool_call_end|>` 单个工具调用块
fn parse_single_tool_call_block(s: &str) -> Option<ToolCall> {
    let val: serde_json::Value = serde_json::from_str(s).ok().or_else(|| {
        let repaired = repair_json(s)?;
        serde_json::from_str(&repaired).ok()
    })?;
    let name = val.get("name")?.as_str()?.to_string();
    let arguments = match val.get("arguments") {
        Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
        None => "{}".into(),
    };
    Some(ToolCall {
        id: next_call_id(),
        ty: "function".to_string(),
        function: Some(FunctionCall { name, arguments }),
        custom: None,
        index: 0,
    })
}

/// 解析 `<|tool_call|>...<|tool_call_end|>` 或 `<|tool_calls_begin|>...<|tool_calls_end|>` 块序列
fn parse_new_tool_calls(inner: &str) -> Option<Vec<ToolCall>> {
    let mut calls = Vec::new();
    let mut pos = 0;
    while pos < inner.len() {
        // 尝试匹配任一种开始标记
        let block_start = if let Some(p) = inner[pos..].find(NEW_TOOL_CALL_BLOCK) {
            Some((p, NEW_TOOL_CALL_BLOCK, NEW_TOOL_CALL_BLOCK_END))
        } else if let Some(p) = inner[pos..].find(TOOL_CALL_START) {
            Some((p, TOOL_CALL_START, TOOL_CALL_END))
        } else {
            None
        };
        let (rel, start_tag, end_tag) = block_start?;
        let content_start = pos + rel + start_tag.len();
        let remaining = &inner[content_start..];
        // end_tag 可能被外层 section 标签消耗掉（不在 inner 中），回退到字符串末尾
        let (end_pos, tail_len) = if let Some(p) = remaining.find(end_tag) {
            (p, end_tag.len())
        } else {
            (remaining.len(), 0)
        };
        let json_str = &remaining[..end_pos];

        if let Some(call) = parse_single_tool_call_block(json_str) {
            calls.push(ToolCall {
                index: calls.len() as u32,
                ..call
            });
        }
        pos = content_start + end_pos + tail_len;
    }
    if calls.is_empty() { None } else { Some(calls) }
}

fn parse_invoke_calls(inner: &str, prefix: &str, suffix: &str) -> Option<(Vec<ToolCall>, String)> {
    use std::collections::BTreeMap;
    let mut calls = Vec::new();
    let mut pos = 0;
    let lower = inner.to_lowercase();
    while let Some(invoke_start) = lower[pos..].find("<invoke ") {
        let abs_start = pos + invoke_start;
        let name_attr = &inner[abs_start..];
        let name_start = name_attr.find("name=\"")? + 6;
        let tail = &name_attr[name_start..];
        let name_end = match (tail.find('"'), tail.find('>')) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => return None,
        };
        let name = &tail[..name_end];
        let close_tag = "</invoke>";
        let rest = &lower[abs_start..];
        let close_pos = rest.find(close_tag)?;
        let invoke_body = &inner[abs_start..abs_start + close_pos + close_tag.len()];
        let mut params: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mut ppos = 0;
        let body_lower = invoke_body.to_lowercase();
        while let Some(p_start) = body_lower[ppos..].find("<parameter ") {
            let p_abs = ppos + p_start;
            let p_attr = &invoke_body[p_abs..];
            let p_name_start = p_attr.find("name=\"")? + 6;
            let p_tail = &p_attr[p_name_start..];
            let p_name_end = match (p_tail.find('"'), p_tail.find('>')) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => return None,
            };
            let p_name = &p_tail[..p_name_end];
            let p_body_start = p_attr.find('>')? + 1;
            let p_close = String::from("</parameter>");
            let p_close_pos = p_attr[p_body_start..].find(&p_close)?;
            let p_value = &p_attr[p_body_start..p_body_start + p_close_pos];
            let val: serde_json::Value = serde_json::from_str(p_value.trim())
                .unwrap_or_else(|_| serde_json::Value::String(p_value.to_string()));
            params.insert(p_name.to_string(), val);
            let p_end = p_body_start + p_close_pos + p_close.len();
            ppos += p_start + p_end;
        }
        let arguments = serde_json::to_string(&params).unwrap_or_else(|_| "{}".into());
        calls.push(ToolCall {
            id: next_call_id(),
            ty: "function".to_string(),
            function: Some(FunctionCall {
                name: name.to_string(),
                arguments,
            }),
            custom: None,
            index: calls.len() as u32,
        });
        pos = abs_start + close_pos + close_tag.len();
    }
    if calls.is_empty() {
        return None;
    }
    Some((calls, prefix.to_string() + suffix))
}

fn make_end_chunk(
    model: &str,
    delta: Delta,
    finish_reason: &'static str,
    id: &str,
) -> ChatCompletionsResponseChunk {
    ChatCompletionsResponseChunk {
        id: id.to_string(),
        object: "chat.completion.chunk",
        created: crate::openai_adapter::response::now_secs(),
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason: Some(finish_reason),
            logprobs: None,
        }],
        usage: None,
        service_tier: None,
        system_fingerprint: None,
    }
}

#[derive(Debug)]
enum ToolParseState {
    Detecting { buffer: String },
    CollectingXml { buf: String, start_tag: String },
    Done,
}

pin_project! {
    pub struct ToolCallStream<S> {
        #[pin]
        inner: S,
        state: ToolParseState,
        model: String,
        chatcmpl_id: String,
        finish_emitted: bool,
        tag_config: Arc<TagConfig>,
        last_keepalive: tokio::time::Instant,
    }
}

impl<S> ToolCallStream<S> {
    pub fn new(inner: S, model: String, tag_config: Arc<TagConfig>, chatcmpl_id: String) -> Self {
        Self {
            inner,
            state: ToolParseState::Detecting {
                buffer: String::new(),
            },
            model,
            chatcmpl_id,
            finish_emitted: false,
            tag_config,
            last_keepalive: tokio::time::Instant::now(),
        }
    }
}

impl<S> Stream for ToolCallStream<S>
where
    S: Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>>,
{
    type Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        loop {
            if matches!(&this.state, ToolParseState::CollectingXml { .. })
                && this.last_keepalive.elapsed() >= KEEPALIVE_INTERVAL
            {
                trace!(target: "adapter", ">>> keepalive: 发送空工具增量");
                *this.last_keepalive = tokio::time::Instant::now();
                return Poll::Ready(Some(Ok(ChatCompletionsResponseChunk {
                    id: crate::openai_adapter::response::KEEPALIVE_ID.to_string(),
                    object: "chat.completion.chunk",
                    created: crate::openai_adapter::response::now_secs(),
                    model: this.model.clone(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: Delta {
                            content: Some("".into()),
                            ..Default::default()
                        },
                        finish_reason: None,
                        logprobs: None,
                    }],
                    usage: None,
                    service_tier: None,
                    system_fingerprint: None,
                })));
            }

            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(mut chunk))) => {
                    let choice = match chunk.choices.first_mut() {
                        Some(c) => c,
                        None => return Poll::Ready(Some(Ok(chunk))),
                    };

                    if let Some(content) = choice.delta.content.take() {
                        if content.is_empty() {
                            choice.delta.content = Some(content);
                            return Poll::Ready(Some(Ok(chunk)));
                        }

                        match &mut this.state {
                            ToolParseState::Detecting { buffer } => {
                                buffer.push_str(&content);

                                let maybe_tag = find_start_tag_with(buffer, this.tag_config)
                                    .map(|(pos, tag)| (pos, tag.to_string()));
                                if let Some((pos, start_tag)) = maybe_tag {
                                    trace!(target: "adapter", "[tc] detect→collect tag=\"{}\" pos={} buf_len={}",
                                        start_tag, pos, buffer.len());
                                    let before = buffer[..pos].to_string();
                                    let rest = std::mem::take(buffer)[pos..].to_string();
                                    if let Some((end_pos, matched_end)) = find_end_tag_with(
                                        &rest,
                                        start_tag.len(),
                                        this.tag_config,
                                        Some(&start_tag),
                                    ) {
                                        let inner = &rest[start_tag.len()..end_pos];
                                        if is_start_tag(matched_end, this.tag_config)
                                            && inner.trim().is_empty()
                                        {
                                            if before.is_empty() {
                                                *this.state = ToolParseState::CollectingXml {
                                                    buf: rest,
                                                    start_tag: start_tag.clone(),
                                                };
                                            } else {
                                                choice.delta.content = Some(before);
                                                *this.state = ToolParseState::CollectingXml {
                                                    buf: rest,
                                                    start_tag: start_tag.clone(),
                                                };
                                            }
                                            continue;
                                        }
                                        let end_abs = end_pos + matched_end.len();
                                        let collected = &rest[..end_abs];
                                        if let Some((calls, _)) = parse_tool_calls(collected) {
                                            let names: Vec<&str> = calls.iter()
                                                .filter_map(|c| c.function.as_ref().map(|f| f.name.as_str()))
                                                .collect();
                                            debug!(target: "adapter", "[tc] result→ok n={} names=[{}]",
                                                calls.len(), names.join(", "));
                                            choice.delta.content = if before.is_empty() {
                                                None
                                            } else {
                                                Some(before)
                                            };
                                            choice.delta.tool_calls = Some(calls);
                                            if choice.finish_reason == Some("stop") {
                                                choice.finish_reason = Some("tool_calls");
                                            }
                                            *this.state = ToolParseState::Done;
                                        } else {
                                            // 空标签对检测：去掉 start/end 标签后只剩空白 → 丢弃
                                            let inner_text = collected
                                                .replacen(start_tag.as_str(), "", 1)
                                                .replacen(matched_end, "", 1);
                                            if inner_text.trim().is_empty() {
                                                warn!(target: "adapter",
                                                    "[tc] fallback reason=empty_pair context=\"{}\"",
                                                    safe_truncate(collected, 200));
                                                trace!(target: "adapter", "tool_parser 空工具调用对，丢弃");
                                                *this.state = ToolParseState::Detecting {
                                                    buffer: String::new(),
                                                };
                                                continue;
                                            }
                                            warn!(target: "adapter",
                                                "[tc] fallback reason=parse_fail context=\"{}\"",
                                                safe_truncate(collected, 500));
                                            trace!(target: "adapter", "tool_parser 解析失败，回退为纯文本");
                                            let collected_str = if before.is_empty() {
                                                collected.to_string()
                                            } else {
                                                before.clone() + collected
                                            };
                                            choice.delta.content = Some(collected_str);
                                            *this.state = ToolParseState::Detecting {
                                                buffer: String::new(),
                                            };
                                            return Poll::Ready(Some(Ok(chunk)));
                                        }
                                        return Poll::Ready(Some(Ok(chunk)));
                                    }
                                    if before.is_empty() {
                                        *this.state = ToolParseState::CollectingXml {
                                            buf: rest,
                                            start_tag: start_tag.clone(),
                                        };
                                        continue;
                                    }
                                    choice.delta.content = Some(before);
                                    *this.state = ToolParseState::CollectingXml {
                                        buf: rest,
                                        start_tag: start_tag.clone(),
                                    };
                                    return Poll::Ready(Some(Ok(chunk)));
                                } else {
                                    if buffer.len() % 5000 < 100 && buffer.len() > 0 {
                                        trace!(target: "adapter", "[tc] detect_buffer len={} sample=\"{}\"",
                                            buffer.len(), safe_truncate(buffer, 200));
                                    }
                                    let safe =
                                        floor_char_boundary(buffer, buffer.len().saturating_sub(W));
                                    if safe > 0 {
                                        choice.delta.content = Some(buffer[..safe].to_string());
                                        buffer.drain(..safe);
                                        return Poll::Ready(Some(Ok(chunk)));
                                    }
                                    continue;
                                }
                            }

                            ToolParseState::CollectingXml { buf, start_tag } => {
                                trace!(target: "adapter", "[tc] collect state len={}", buf.len());
                                buf.push_str(&content);
                                if buf.len() > MAX_XML_BUF_LEN {
                                    warn!(target: "adapter", "[tc] parse→buf_overflow len={}", buf.len());
                                    let flushed = std::mem::take(buf);
                                    *this.state = ToolParseState::Detecting {
                                        buffer: String::new(),
                                    };
                                    choice.delta.content = Some(flushed);
                                    return Poll::Ready(Some(Ok(chunk)));
                                }
                                let start_end = buf.find('>').map(|p| p + 1).unwrap_or(0);
                                if let Some((end_pos, en_tag)) = find_end_tag_with(
                                    buf,
                                    start_end,
                                    this.tag_config,
                                    Some(start_tag),
                                ) {
                                    let inner = &buf[start_end..end_pos];
                                    if is_start_tag(en_tag, this.tag_config)
                                        && inner.trim().is_empty()
                                    {
                                        continue;
                                    }
                                    trace!(target: "adapter", "[tc] collect→done tag=\"{}\" len={}",
                                        en_tag, buf.len());
                                    let end_abs = end_pos + en_tag.len();
                                    let collected = buf[..end_abs].to_string();
                                    let en_tag_owned = en_tag.to_string();
                                    let _tail = buf.split_off(end_abs);
                                    if let Some((calls, _)) = parse_tool_calls(&collected) {
                                        let names: Vec<&str> = calls.iter()
                                            .filter_map(|c| c.function.as_ref().map(|f| f.name.as_str()))
                                            .collect();
                                        debug!(target: "adapter", "[tc] result→ok n={} names=[{}]",
                                            calls.len(), names.join(", "));
                                        choice.delta.content = None;
                                        choice.delta.tool_calls = Some(calls);
                                        if choice.finish_reason == Some("stop") {
                                            choice.finish_reason = Some("tool_calls");
                                        }
                                        *this.state = ToolParseState::Done;
                                    } else {
                                        // 空标签对检测：去掉 start/end 标签后只剩空白 → 丢弃
                                        let inner_text = collected
                                            .replacen(start_tag.as_str(), "", 1)
                                            .replacen(en_tag_owned.as_str(), "", 1);
                                        if inner_text.trim().is_empty() {
                                            warn!(target: "adapter",
                                                "[tc] fallback reason=empty_pair context=\"{}\"",
                                                safe_truncate(&collected, 200));
                                            trace!(target: "adapter", "tool_parser 空工具调用对(流结束)，丢弃");
                                            *this.state = ToolParseState::Detecting {
                                                buffer: String::new(),
                                            };
                                        } else {
                                            warn!(target: "adapter",
                                                "[tc] fallback reason=parse_fail context=\"{}\"",
                                                safe_truncate(&collected, 500));
                                            trace!(target: "adapter", "tool_parser 解析失败(流结束)，回退为纯文本");
                                            choice.delta.content = Some(collected.clone());
                                            *this.state = ToolParseState::Detecting {
                                                buffer: String::new(),
                                            };
                                        }
                                    }
                                    return Poll::Ready(Some(Ok(chunk)));
                                }
                                continue;
                            }

                            ToolParseState::Done => {
                                // 在 Done 状态下，持续从 inner 抽取元数据（如 Usage）
                                if let Poll::Ready(Some(Ok(mut chunk))) = this.inner.as_mut().poll_next(cx) {
                                    if chunk.usage.is_some() || chunk.choices.is_empty() {
                                        chunk.id = this.chatcmpl_id.clone();
                                        return Poll::Ready(Some(Ok(chunk)));
                                    }
                                    continue;
                                }

                                if !*this.finish_emitted {
                                    *this.finish_emitted = true;
                                    let chunk = make_end_chunk(
                                        this.model,
                                        Delta::default(),
                                        "tool_calls",
                                        this.chatcmpl_id,
                                    );
                                    log::trace!(target: "adapter", "Done→emitting finish chunk");
                                    return Poll::Ready(Some(Ok(chunk)));
                                }
                                return Poll::Ready(None);
                            }
                        }
                    } else {
                        match &mut this.state {
                            ToolParseState::Detecting { buffer } => {
                                if choice.finish_reason.is_some() {
                                    if !buffer.is_empty() {
                                        choice.delta.content = Some(std::mem::take(buffer));
                                    }
                                    return Poll::Ready(Some(Ok(chunk)));
                                }
                                return Poll::Ready(Some(Ok(chunk)));
                            }
                            ToolParseState::CollectingXml { buf, start_tag: _ } => {
                                if choice.finish_reason.is_some() {
                                    let flushed = std::mem::take(buf);
                                    if let Some((calls, _)) = parse_tool_calls(&flushed) {
                                        let names: Vec<&str> = calls.iter()
                                            .filter_map(|c| c.function.as_ref().map(|f| f.name.as_str()))
                                            .collect();
                                        debug!(target: "adapter", "[tc] result→ok n={} names=[{}]",
                                            calls.len(), names.join(", "));
                                        choice.delta.tool_calls = Some(calls);
                                        if choice.finish_reason == Some("stop") {
                                            choice.finish_reason = Some("tool_calls");
                                        }
                                    } else {
                                        warn!(target: "adapter",
                                            "[tc] fallback reason=parse_fail_on_finish context=\"{}\"",
                                            safe_truncate(&flushed, 500));
                                        trace!(target: "adapter", "tool_parser finish→回退为纯文本");
                                        choice.delta.content = Some(flushed);
                                    }
                                    *this.state = ToolParseState::Done;
                                    return Poll::Ready(Some(Ok(chunk)));
                                }
                                return Poll::Ready(Some(Ok(chunk)));
                            }
                            ToolParseState::Done => {
                                log::trace!(target: "adapter", "Done→polling inner for metadata");
                                if let Poll::Ready(Some(Ok(mut chunk))) = this.inner.as_mut().poll_next(cx) {
                                    log::trace!(target: "adapter", "Done→got metadata chunk usage={} choices={}",
                                        chunk.usage.is_some(), chunk.choices.len());
                                    if chunk.usage.is_some() || chunk.choices.is_empty() {
                                        chunk.id = this.chatcmpl_id.clone();
                                        return Poll::Ready(Some(Ok(chunk)));
                                    }
                                    continue;
                                }
                                log::trace!(target: "adapter", "Done→inner exhausted, finish_emitted={}", *this.finish_emitted);
                                if !*this.finish_emitted {
                                    *this.finish_emitted = true;
                                    let mut end = make_end_chunk(
                                        this.model,
                                        Delta::default(),
                                        "tool_calls",
                                        this.chatcmpl_id,
                                    );
                                    if let Some(ref u) = chunk.usage {
                                        end.usage = Some(u.clone());
                                    }
                                    log::trace!(target: "adapter", "Done→emitting finish chunk");
                                    return Poll::Ready(Some(Ok(end)));
                                }
                                return Poll::Ready(None);
                            }
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    log::trace!(target: "adapter", "Done→inner Poll::Ready(None) finished={}",
                        *this.finish_emitted);
                    let need_finish = !*this.finish_emitted
                        && matches!(&*this.state, ToolParseState::Done);
                    match std::mem::replace(this.state, ToolParseState::Done) {
                    ToolParseState::Detecting { buffer } => {
                        if !buffer.is_empty() {
                            let chunk = make_end_chunk(
                                this.model,
                                Delta {
                                    content: Some(buffer),
                                    ..Default::default()
                                },
                                "stop",
                                this.chatcmpl_id,
                            );
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        return Poll::Ready(None);
                    }
                    ToolParseState::CollectingXml { buf, start_tag: _ } => {
                        if let Some((calls, _)) = parse_tool_calls(&buf) {
                            let names: Vec<&str> = calls.iter()
                                .filter_map(|c| c.function.as_ref().map(|f| f.name.as_str()))
                                .collect();
                            debug!(target: "adapter", "[tc] result→ok n={} names=[{}]",
                                calls.len(), names.join(", "));
                            let chunk = make_end_chunk(
                                this.model,
                                Delta {
                                    tool_calls: Some(calls),
                                    ..Default::default()
                                },
                                "tool_calls",
                                this.chatcmpl_id,
                            );
                            return Poll::Ready(Some(Ok(chunk)));
                        } else {
                            warn!(target: "adapter",
                                "[tc] fallback reason=stream_end_unclosed len={} context=\"{}\"",
                                buf.len(), safe_truncate(&buf, 500));
                            trace!(target: "adapter", "tool_parser 流结束→回退为纯文本");
                            let chunk = make_end_chunk(
                                this.model,
                                Delta {
                                    content: Some(buf),
                                    ..Default::default()
                                },
                                "stop",
                                this.chatcmpl_id,
                            );
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                    }
                    ToolParseState::Done => {
                        if need_finish {
                            *this.finish_emitted = true;
                            let model: &str = this.model;
                            let id: &str = this.chatcmpl_id;
                            return Poll::Ready(Some(Ok(make_end_chunk(
                                model,
                                Delta::default(),
                                "tool_calls",
                                id,
                            ))));
                        }
                        return Poll::Ready(None);
                    }
                    }
                }
                Poll::Pending => break,
            }
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(content: &str) -> String {
        format!("{TOOL_CALL_START}{content}{TOOL_CALL_END}")
    }
    fn tool_ts(content: &str, suffix: &str) -> String {
        format!("{TOOL_CALL_START}{content}{TOOL_CALL_END}{suffix}")
    }

    #[test]
    fn parse_json_tool_calls() {
        let xml = tool(r#"[{"name": "get_weather", "arguments": {"city": "北京"}}]"#);
        let (calls, remaining) = parse_tool_calls(&xml).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.as_ref().unwrap().name, "get_weather");
        assert_eq!(
            calls[0].function.as_ref().unwrap().arguments,
            r#"{"city":"北京"}"#
        );
    }

    #[test]
    fn parse_json_with_surrounding_text() {
        let xml = format!(
            "{TOOL_CALL_START}\n\t以下是工具调用：\n\t[{{\"name\": \"f\", \"arguments\": {{}}}}]\n\t{TOOL_CALL_END}"
        );
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_json_multiple_tools() {
        let xml = tool(
            r#"[{"name": "get_weather", "arguments": {}}, {"name": "get_time", "arguments": {"tz": "bj"}}]"#,
        );
        let (calls, remaining) = parse_tool_calls(&xml).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn parse_json_with_trailing_text() {
        let xml = tool_ts(
            r#"[{"name": "get_weather", "arguments": {}}]"#,
            " trailing text",
        );
        let (calls, remaining) = parse_tool_calls(&xml).unwrap();
        assert_eq!(remaining, " trailing text");
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn repair_backslashes_passes_valid_escapes() {
        assert_eq!(
            repair_invalid_backslashes(r#"hello\nworld"#),
            r#"hello\nworld"#
        );
    }
    #[test]
    fn repair_backslashes_fixes_invalid_escapes() {
        assert_eq!(repair_invalid_backslashes(r#"C:\Users\name"#).len(), 14);
    }
    #[test]
    fn repair_backslashes_keeps_valid_n() {
        assert_eq!(
            repair_invalid_backslashes(r#"line1\nline2"#),
            r#"line1\nline2"#
        );
    }
    #[test]
    fn repair_unquoted_keys_basic() {
        assert_eq!(
            repair_unquoted_keys(r#"{name: "get_weather"}"#),
            r#"{"name": "get_weather"}"#
        );
    }
    #[test]
    fn repair_unquoted_keys_array() {
        assert_eq!(
            repair_unquoted_keys(r#"[{name: "f", arguments: {}}]"#),
            r#"[{"name": "f", "arguments": {}}]"#
        );
    }

    #[test]
    fn parse_tool_calls_with_unquoted_keys() {
        let xml = tool(r#"[{name: "get_weather", arguments: {city: "北京"}}]"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_with_invalid_backslashes() {
        let xml = tool(r#"[{"name": "read_file", "arguments": {"path": "C:\Users\name"}}]"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_with_both_repairs() {
        let xml = tool(r#"[{name: "read_file", arguments: {path: "C:\file"}}]"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_inside_code_fence_skipped() {
        let xml = format!(
            "示例：\n```json\n{TOOL_CALL_START}[{{\"name\": \"get_weather\", \"arguments\": {{}}}}]{TOOL_CALL_END}\n```"
        );
        assert!(parse_tool_calls(&xml).is_none());
    }

    #[test]
    fn parse_tool_calls_not_inside_code_fence() {
        assert!(parse_tool_calls(&tool(r#"[{"name": "get_weather", "arguments": {}}]"#)).is_some());
    }

    #[test]
    fn parse_tool_calls_tool_call_inside_value_not_skipped() {
        let xml = tool(
            r#"[{"name": "format_code", "arguments": {"code": "```rust\nfn main() {}\n```"}}]"#,
        );
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn code_fence_detection() {
        assert!(!is_inside_code_fence("普通文本", 0));
    }

    #[test]
    fn parse_tool_calls_single_object() {
        let xml = tool(r#"{"name": "get_weather", "arguments": {"city": "北京"}}"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_single_object_with_newlines() {
        let xml = format!(
            "{TOOL_CALL_START}\n{{\"name\": \"Bash\", \"arguments\": {{\"command\": \"ls\"}}}}\n{TOOL_CALL_END}"
        );
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_single_object_with_surrounding_text() {
        let xml = format!(
            "{TOOL_CALL_START}以下是工具调用：{{\"name\": \"f\", \"arguments\": {{}}}}{TOOL_CALL_END}"
        );
        let (_calls, remaining) = parse_tool_calls(&xml).unwrap();
        assert_eq!(remaining, "");
    }

    #[test]
    fn parse_tool_calls_single_object_unquoted_keys() {
        let xml = tool(r#"{name: "get_weather", arguments: {city: "北京"}}"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_single_object_and_repair_backslashes() {
        let xml = tool(r#"{"name": "read_file", "arguments": {"path": "C:\Users\name"}}"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn fuzzy_match_hallucinated_marker() {
        // <|tool▁calls▁begin|> 正常标签，但结束标签用 <|tool_calls▁end｜>
        // （ASCII _ + ▁ + 全角 ｜），验证模糊匹配能识别
        let xml = format!(
            r#"{TOOL_CALL_START}[{{"name": "get_weather", "arguments": {{"city": "北京"}}}}]<|tool_calls▁end｜>"#
        );
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn fuzzy_match_alt_block_char() {
        // 模型输出 ▂(U+2582) 而非预期 ▁(U+2581)，验证 U+2581-U+2588 范围归一化
        let start = "<|tool\u{2582}calls\u{2582}begin|>";
        let end = "<|tool\u{2582}calls\u{2582}end|>";
        let xml = format!(
            r#"{start}[{{"name": "get_weather", "arguments": {{"city": "北京"}}}}]{end}"#
        );
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    // ---- JSON 修复新函数测试 ----

    #[test]
    fn repair_trailing_commas_basic() {
        assert_eq!(repair_trailing_commas(r#"{"a": 1,}"#), r#"{"a": 1}"#);
    }

    #[test]
    fn repair_trailing_commas_array() {
        assert_eq!(repair_trailing_commas(r#"[1, 2,]"#), r#"[1, 2]"#);
    }

    #[test]
    fn repair_trailing_commas_inside_string_unchanged() {
        assert_eq!(
            repair_trailing_commas(r#"{"msg": "hello, world,"}"#),
            r#"{"msg": "hello, world,"}"#
        );
    }

    #[test]
    fn repair_trailing_commas_nested() {
        assert_eq!(
            repair_trailing_commas(r#"{"a": {"b": 1,},}"#),
            r#"{"a": {"b": 1}}"#
        );
    }

    #[test]
    fn repair_single_quotes_basic() {
        assert_eq!(
            repair_single_quotes(r#"{'a': 'b'}"#),
            r#"{"a": "b"}"#
        );
    }

    #[test]
    fn repair_single_quotes_nested() {
        assert_eq!(
            repair_single_quotes(r#"[{'name': 'f', 'arguments': {'x': 'y'}}]"#),
            r#"[{"name": "f", "arguments": {"x": "y"}}]"#
        );
    }

    #[test]
    fn repair_single_quotes_double_inside_single() {
        // 单引号字符串内含双引号
        assert_eq!(
            repair_single_quotes(r#"{'msg': 'say "hello"'}"#),
            r#"{"msg": "say \"hello\""}"#
        );
    }

    #[test]
    fn strip_control_chars_basic() {
        assert_eq!(strip_control_chars("a\x00b\x01c"), "abc");
    }

    #[test]
    fn strip_control_chars_keeps_newline() {
        assert_eq!(strip_control_chars("a\nb\tc"), "a\nb\tc");
    }

    #[test]
    fn normalize_unicode_quotes_basic() {
        // \u{201C}=\u{201D}=左/右双引号 → ASCII "
        // \u{2018}=\u{2019}=左/右单引号 → ASCII '
        let result = normalize_unicode_quotes("{\u{201C}name\u{201D}: \u{2018}val\u{2019}}");
        assert_eq!(result, r#"{"name": 'val'}"#);
    }

    #[test]
    fn normalize_unicode_quotes_corner_brackets() {
        assert_eq!(
            normalize_unicode_quotes("{\u{300C}key\u{300D}: 1}"),
            r#"{"key": 1}"#
        );
    }

    // ---- 修复链集成测试 ----

    #[test]
    fn repair_json_trailing_commas_through_chain() {
        let xml = tool(r#"[{"name": "get_weather", "arguments": {"city": "北京",}},]"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn repair_json_single_quotes_through_chain() {
        let xml = tool(r#"[{'name': 'get_weather', 'arguments': {'city': '北京'}}]"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn repair_json_control_chars_through_chain() {
        let xml = format!(
            "{TOOL_CALL_START}[{{\"name\": \"get_weather\", \"arguments\": {{\"city\": \"\x00北\x01京\"}}}}]{TOOL_CALL_END}"
        );
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn repair_json_unicode_quotes_through_chain() {
        // 输入含全角双引号 \u{201C}\u{201D}，正常化为 ASCII " 后应为有效 JSON
        // 注意：\u{201D}}}] 中第一个 } 是 \u{201D} 的语法部分
        let raw = "[{\u{201C}name\u{201D}: \u{201C}get_weather\u{201D}, \u{201C}arguments\u{201D}: {\u{201C}city\u{201D}: \u{201C}北京\u{201D}}}]";
        let xml = format!("{}{}{}", TOOL_CALL_START, raw, TOOL_CALL_END);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn repair_json_all_combined() {
        // 同时包含多种畸形：单引号、尾逗号、控制字符、全角引号
        let body = "[{\u{2018}name\u{2019}: \u{2018}test\u{2019}, \u{2018}arguments\u{2019}: {\u{2018}val\u{2019}: \x00\x01\x02\"x\"},}]";
        let input = format!("{}{}{}", TOOL_CALL_START, body, TOOL_CALL_END);
        let (calls, _) = parse_tool_calls(&input).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn repair_json_empty_array_not_confused() {
        // [] 不应触发修复
        let xml = format!("{TOOL_CALL_START}[]{TOOL_CALL_END}");
        assert!(parse_tool_calls(&xml).is_none());
    }

    #[test]
    fn escape_newlines_in_json_strings() {
        // new_string 值中含真实换行符（非 \\n 转义），标准 JSON 非法，应由 repair_json 修复
        let body = "{\"name\": \"Edit\", \"arguments\": {\"file_path\": \"/tmp/test.md\", \"old_string\": \"foo\", \"new_string\": \"line1\nline2\"}}";
        let xml = format!("{TOOL_CALL_START}[{body}]{TOOL_CALL_END}");
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.as_ref().unwrap().name, "Edit");
    }
}
