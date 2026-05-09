# 工具调用全链路追踪日志 — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 ds-free-api 的请求/响应管线中添加结构化追踪日志，使工具调用全链路可观测

**Architecture:** 在现有 3 个文件 (`tools.rs`, `prompt.rs`, `tool_parser.rs`) 中内联添加 `log!` 宏调用，仅增加日志不加业务逻辑。所有日志以 `[tc]` 前缀标记，使用结构化 `key=value` 格式。

**Tech Stack:** Rust `log` crate (现有依赖)

**日志级别策略:**
- `warn!` — 工具调用丢失事件 (fallback / buf overflow)
- `debug!` — 请求侧概览、最终结果
- `trace!` — 状态机事件、JSON 修复过程

---

### Task 1: tools.rs — 工具注入明细日志

**Modify:** `src/openai_adapter/request/tools.rs`

**Context:** `extract()` 函数（第 29 行附近）解析完工具定义后返回 `ToolContext`。需要在返回前记录工具概览。

- [ ] **Step 1: 添加 log import**

在文件顶部 import 块添加 `log`。当前 import（第 6-10 行）：

```rust
use crate::openai_adapter::response::{TOOL_CALL_END, TOOL_CALL_START};
use crate::openai_adapter::types::{
    AllowedTools, AllowedToolsChoice, ChatCompletionsRequest, CustomTool, CustomToolFormat,
    FunctionDefinition, Tool, ToolChoice,
};
```

改为：

```rust
use log::{debug, trace};

use crate::openai_adapter::response::{TOOL_CALL_END, TOOL_CALL_START};
use crate::openai_adapter::types::{
    AllowedTools, AllowedToolsChoice, ChatCompletionsRequest, CustomTool, CustomToolFormat,
    FunctionDefinition, Tool, ToolChoice,
};
```

- [ ] **Step 2: 在 `extract()` 返回前添加日志**

`extract()` 函数最后有多个 return 路径。在所有 `Ok(...)` 返回前添加日志。

找到第 40 行的 `tool_choice == "none"` 提前返回，加一条 trace：

```rust
    if matches!(tool_choice, ToolChoice::Mode(m) if m == "none") {
        trace!(target: "adapter", "[tc] tool_extract tool_choice=none");
        return Ok(ToolContext {
            format_block: None,
            defs_text: None,
            instruction_text: None,
        });
    }
```

在 `extract()` 最后的 `Ok(ToolContext { ... })` 返回前（约第 99 行），添加 debug 日志：

```rust
    if has_tools(req) {
        let tools = req.tools.as_ref().unwrap();
        let names: Vec<&str> = tools.iter()
            .filter_map(|t| t.function.as_ref().map(|f| f.name.as_str()))
            .collect();
        debug!(target: "adapter", "[tc] tool_extract n={} names=[{}] tool_choice={:?} parallel={}",
            names.len(), names.join(", "),
            tool_choice_str(&req.tool_choice.as_ref().unwrap_or(&default_choice)),
            req.parallel_tool_calls.unwrap_or(true),
        );
        debug!(target: "adapter", "[tc] tool_extract block={} defs={} instruction={}",
            ctx.format_block.is_some(),
            ctx.defs_text.is_some(),
            ctx.instruction_text.is_some(),
        );
    }
```

需要新增辅助函数 `tool_choice_str`（在 `extract()` 上方）：

```rust
fn tool_choice_str(tc: &ToolChoice) -> &'static str {
    match tc {
        ToolChoice::Mode(m) => m,
        ToolChoice::Named(_) => "named",
        ToolChoice::AllowedTools(_) => "allowed_tools",
        ToolChoice::Custom(_) => "custom",
    }
}
```

- [ ] **Step 3: 编译检查**

```bash
cargo check 2>&1 | head -30
```
Expected: 编译通过，无 warning

- [ ] **Step 4: 运行测试**

```bash
cargo test --lib openai_adapter::request::tools 2>&1
```
Expected: 所有测试通过

---

### Task 2: prompt.rs — Prompt 组装概要日志

**Modify:** `src/openai_adapter/request/prompt.rs`

**Context:** `build()` 函数（第 101 行）构建完整 prompt 后返回 String。需要在返回前记录 prompt 概要。

- [ ] **Step 1: 添加 log import**

文件顶部（第 7-9 行）：

```rust
use super::tools::ToolContext;
use crate::openai_adapter::response::{TOOL_CALL_END, TOOL_CALL_START};
use crate::openai_adapter::types::{ChatCompletionsRequest, ContentPart, Message, MessageContent};
```

改为：

```rust
use log::{debug, info};

use super::tools::ToolContext;
use crate::openai_adapter::response::{TOOL_CALL_END, TOOL_CALL_START};
use crate::openai_adapter::types::{ChatCompletionsRequest, ContentPart, Message, MessageContent};
```

- [ ] **Step 2: 在 `build()` 返回前添加日志**

在 `build()` 函数的 `parts.join("")` 返回前（第 199 行附近），记录 prompt 概要：

```rust
    let result = parts.join("");

    debug!(target: "adapter", "[tc] prompt_build messages={} len={} tool_inject={}",
        merged.len(), result.len(),
        tool_ctx.format_block.is_some() || tool_ctx.defs_text.is_some(),
    );
    if result.len() > 10000 {
        info!(target: "adapter", "[tc] prompt_build len={} (>10000, ctx inflation warning)", result.len());
    }

    result
```

同时需要暴露 `merged` 的长度——当前 `merged` 是在函数内第 102 行定义的局部变量，直接使用即可。注意返回语句需改为先赋值再返回。

找到函数末尾 `parts.join("")`（第 199 行），替换为上述代码。

- [ ] **Step 3: 编译检查**

```bash
cargo check 2>&1 | head -30
```

- [ ] **Step 4: 运行测试**

```bash
cargo test --lib openai_adapter::request::prompt 2>&1
```

---

### Task 3: tool_parser.rs — 状态机 & 缓冲区日志

**Modify:** `src/openai_adapter/response/tool_parser.rs`

这是核心修改。需要添加 `warn` import，并在多个位置添加日志。

- [ ] **Step 1: 添加 warn import**

第 18 行：
```rust
use log::{debug, trace};
```
改为：
```rust
use log::{debug, trace, warn};
```

- [ ] **Step 2: 添加辅助截断函数**

在 `next_call_id()` 函数（约第 187 行）下方添加两个安全截断函数：

```rust
/// 安全截断字符串至 max_len，保证 char boundary
fn safe_truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut i = max_len;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}
```

- [ ] **Step 3: Detecting 状态 — 记录 buffer 滑动日志

在 Detecting 状态处理分支（约第 658-743 行）。以下是现有代码结构，需要新增日志的位置：

**A. 找到 start_tag 时**（在 `if let Some((pos, start_tag)) = maybe_tag` 内，约第 664 行）：

在 `let before = buffer[..pos].to_string();` 前添加：

```rust
                                    trace!(target: "adapter",
                                        "[tc] detect→collect tag=\"{}\" pos={} buf_len={}",
                                        start_tag, pos, buffer.len());
```

**B. 未找到标签时 buffer 增长过多**（在 Detecting 的 else 分支，约第 735 行，`safe` 处理前）：

使用取模采样，每累计 ~100 个 chunk 输出一次 buffer 状态：

```rust
                                    // 每约 5000 字符输出一次 buffer 状态 (trace)
                                    if buffer.len() % 5000 < 100 && buffer.len() > 0 {
                                        trace!(target: "adapter", "[tc] detect_buffer len={} sample=\"{}\"",
                                            buffer.len(), safe_truncate(buffer, 200));
                                    }
```

**C. CollectingXml 状态 — 周期性报告**（在 `poll_next` 的 CollectingXml 分支内，约第 746 行的循环起始处）：

```rust
                            ToolParseState::CollectingXml { buf, start_tag } => {
                                trace!(target: "adapter", "[tc] collect state len={}", buf.len());
                                buf.push_str(&content);
```

**D. CollectingXml 找到结束标签**（在 `if let Some((end_pos, en_tag)) = find_end_tag_with(...)` 成功时，约第 758 行）：

```rust
                                if let Some((end_pos, en_tag)) = find_end_tag_with(/* ... */) {
                                    trace!(target: "adapter", "[tc] collect→done tag=\"{}\" len={}",
                                        en_tag, buf.len());
```

**E. 缓冲超限**（约第 748-756 行，现有 `debug!` 已记录）：

```rust
                                if buf.len() > MAX_XML_BUF_LEN {
                                    warn!(target: "adapter", "[tc] parse→buf_overflow len={}", buf.len());
```

将现有 `debug!` 升级为 `warn!`。

- [ ] **Step 4: 解析失败的关键上下文日志**

在 `parse_tool_calls` 或 `parse_tool_calls_with` 返回 `None` 时，记录当时的输入内容样本。

**A. `parse_tool_calls_with` 中找不到起始标签**（`find_start_tag_with` 返回 `None` 处，约第 414 行）。

在 `parse_tool_calls_with` 函数中，在 `let (start, start_tag) = find_start_tag_with(xml, cfg)?;` 的调用之前无法加日志（因为 `?` 提前返回），需要修改函数签名或添加包装日志。改为在调用前记录：

实际上 `find_start_tag_with` 返回 `None` 时会被 `?` 吃掉。更好的方法：在调用处 wrap：

不直接改 `parse_tool_calls_with`，而是在 `ToolCallStream` 中调用 `parse_tool_calls` 失败的各个位置添加日志。

**B. `ToolCallStream` 中 `parse_tool_calls` 返回 `None` 的位置**（共 4 处）：

第 694-718 行（Detecting 状态内，即开即闭的完整 XML）：
```rust
                                        if let Some((calls, _)) = parse_tool_calls(collected) {
                                            // ... 成功处理 ...
                                        } else {
                                            warn!(target: "adapter",
                                                "[tc] fallback reason=parse_fail context=\"{}\"",
                                                safe_truncate(collected, 500));
                                            trace!(target: "adapter", "tool_parser 解析失败，回退为纯文本");
```

第 773-788 行（CollectingXml 状态，流中闭合）：
```rust
                                    if let Some((calls, _)) = parse_tool_calls(&collected) {
                                        // ... 成功处理 ...
                                    } else {
                                        warn!(target: "adapter",
                                            "[tc] fallback reason=parse_fail context=\"{}\"",
                                            safe_truncate(&collected, 500));
                                        trace!(target: "adapter", "...");
```

第 830-841 行（CollectingXml 状态，finish_reason 触发闭合）：
```rust
                                    if let Some((calls, _)) = parse_tool_calls(&flushed) {
                                        // ... 成功处理 ...
                                    } else {
                                        warn!(target: "adapter",
                                            "[tc] fallback reason=parse_fail_on_finish context=\"{}\"",
                                            safe_truncate(&flushed, 500));
                                        trace!(target: "adapter", "...");
```

第 891-915 行（流结束，Poll::Ready(None) 时 CollectingXml 状态残留）：
```rust
                                    if let Some((calls, _)) = parse_tool_calls(&buf) {
                                        // ... 成功处理 ...
                                    } else {
                                        warn!(target: "adapter",
                                            "[tc] fallback reason=stream_end_unclosed len={} context=\"{}\"",
                                            buf.len(), safe_truncate(&buf, 500));
                                        trace!(target: "adapter", "...");
```

- [ ] **Step 5: 编译检查**

```bash
cargo check 2>&1 | head -30
```

---

### Task 4: tool_parser.rs — JSON 修复过程日志 & 成功结果日志

- [ ] **Step 1: JSON 修复函数 `repair_json` 添加 trace 日志**

在 `repair_json` 函数（第 377 行）中，每一步后添加 trace 日志：

```rust
fn repair_json(s: &str) -> Option<String> {
    let mut current = strip_control_chars(s);
    current = normalize_unicode_quotes(&current);
    if try_json_parse(&current).is_some() {
        return Some(current);
    }

    current = repair_invalid_backslashes(&current);
    trace!(target: "adapter", "[tc] json_repair→try step=backslash len={} ok={}",
        current.len(), try_json_parse(&current).is_some());
    if try_json_parse(&current).is_some() {
        return Some(current);
    }

    current = repair_single_quotes(&current);
    trace!(target: "adapter", "[tc] json_repair→try step=single_quotes len={} ok={}",
        current.len(), try_json_parse(&current).is_some());
    if try_json_parse(&current).is_some() {
        return Some(current);
    }

    current = repair_trailing_commas(&current);
    trace!(target: "adapter", "[tc] json_repair→try step=trailing_commas len={} ok={}",
        current.len(), try_json_parse(&current).is_some());
    if try_json_parse(&current).is_some() {
        return Some(current);
    }

    current = repair_unquoted_keys(&current);
    trace!(target: "adapter", "[tc] json_repair→try step=unquoted_keys len={} ok={}",
        current.len(), try_json_parse(&current).is_some());
    if try_json_parse(&current).is_some() {
        return Some(current);
    }

    trace!(target: "adapter", "[tc] json_repair→all_failed sample=\"{}\"",
        safe_truncate(s, 150));
    None
}
```

注意：需要将 `safe_truncate` 设为 `pub(crate)` 或在 `repair_json` 上方定义。建议在文件顶部（第 18 行后）定义一个模块级辅助函数而非 `pub(crate)`。

- [ ] **Step 2: 解析成功时记录最终结果**

在 `ToolCallStream::poll_next` 中解析成功的所有位置，在 `debug!` 后添加工具名列表：

查找所有形如 `debug!(target: "adapter", "tool_parser 解析出 {} 个工具调用", calls.len());` 的位置（共 4 处），替换为：

```rust
                                            let names: Vec<&str> = calls.iter()
                                                .filter_map(|c| c.function.as_ref().map(|f| f.name.as_str()))
                                                .collect();
                                            debug!(target: "adapter", "[tc] result→ok n={} names=[{}]",
                                                calls.len(), names.join(", "));
```

- [ ] **Step 3: 编译检查**

```bash
cargo check 2>&1 | head -30
```

- [ ] **Step 4: 运行所有测试**

```bash
cargo test --lib 2>&1 | tail -20
```
Expected: 所有测试通过

---

### Task 5: 集成验证

- [ ] **Step 1: 查看 diff**

```bash
git diff --stat
```
Expected: 只修改 3 个文件：`tools.rs`, `prompt.rs`, `tool_parser.rs`

- [ ] **Step 2: 确认日志输出**

启动服务器并发送一个带工具调用的请求：

```bash
RUST_LOG=adapter=debug just serve 2>&1 &
# 在另一个终端发送 curl 请求
curl http://127.0.0.1:22217/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-default",
    "messages": [{"role": "user", "content": "天气怎么样"}],
    "tools": [{"type": "function", "function": {"name": "get_weather", "description": "查天气", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}}]
  }'
```
Expected: 日志中出现 `[tc] tool_extract`, `[tc] prompt_build`, `[tc] result→ok` 等标记

- [ ] **Step 3: 提交**

```bash
git add src/openai_adapter/request/tools.rs src/openai_adapter/request/prompt.rs src/openai_adapter/response/tool_parser.rs
git commit -m "feat: 工具调用全链路追踪日志

添加 [tc] 前缀的结构化日志覆盖工具调用全管线：
- tools.rs: 工具注入明细 (debug)
- prompt.rs: prompt 组装概要 (debug, len>10K → info)
- tool_parser.rs: 状态机事件 (trace)、
  解析失败上下文 (warn)、JSON 修复过程 (trace)、
  最终结果 (debug/fallback→warn)

允许通过 RUST_LOG=adapter=debug/trace 按需开启。
Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```
