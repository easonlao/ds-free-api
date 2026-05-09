# 适配器层与 ds_core 衔接审计

本文档记录 openai_adapter / anthropic_compat 与 ds_core 之间的衔接问题。不含修复方案，仅作问题清单。

---

## 1. Anthropic web_search 自动注入为死代码

**位置**: `src/anthropic_compat/request.rs:53-66` + `src/openai_adapter.rs:148`

**现象**: Anthropic 适配器检测到 `web_search` server tool 时自动设 `web_search_options`，但 OpenAI 适配器只用 `file_result.has_http_urls` 决定 `search_enabled`，完全不读取 `req.web_search_options`。

```rust
// anthropic_compat/request.rs:53 — 设了但没人读
let web_search_options: Option<WebSearchOptions> = if has_web_search {
    Some(WebSearchOptions { search_context_size: Some("high".to_string()), ... })
};

// openai_adapter.rs:148 — 唯一设置 search_enabled 的地方
search_enabled: file_result.has_http_urls,  // 只来自图片 HTTP URL
```

**影响**: Claude Code 请求 web_search 时，DeepSeek 端实际不会启用搜索。静默失效。

---

## 2. 文件双重处理：既上传又变占位文本

**位置**: `src/openai_adapter/request/files.rs` + `src/openai_adapter/request/prompt.rs:322-333`

**现象**: 文件在 `files::extract()` 中被提取为 `FilePayload` 上传到 DeepSeek 会话，但 `prompt::build()` 同时将文件转为 `[文件: {desc}]` 占位文本注入 prompt。

```
请求中的文件 → files::extract() → FilePayload → upload_and_poll() → DeepSeek 会话
              → prompt::build()  → "[文件: xxx]" → 注入 prompt 文本
```

**影响**: 模型既看到了文件内容（通过上传），又看到了占位文本。冗余处理，增加 prompt 噪点。

---

## 3. 图片内容实际不可见

**位置**: `src/openai_adapter/request/prompt.rs:302-312` + `files.rs:85-100`

**现象**: 图片 data URL 会被提取为 `FilePayload` 上传，但 DeepSeek **不处理图片文件内容**。prompt 中图片变为 `[图片: detail=auto]` 或 `[请访问这个链接: {url}]` 文本占位符。模型从未"看到"图片像素数据。

**影响**: 多模态能力链路完全断裂。用户传图片但模型感知不到。

---

## 4. 工具调用全链路为 LLM 依赖

**方式**: prompt 注入（非 API 参数）

```
客户端 tools JSON → tools.rs 自然语言格式化 → System prompt 注入
→ 模型自行理解格式 → 输出 XML 标签 → tool_parser.rs 解析回 JSON
```

模型理解格式这一步完全不可控。当前有三重防护：
- 负面示例注入（`tools.rs:305-322`）
- ContentDelta 幻觉过滤器（`converter.rs:70-209`）
- 空标签对抑制（`tool_parser.rs:919-1183`）

但模型若输出全新格式的幻觉（如 markdown 代码块包裹 JSON），代码层防御会被绕过。

---

## 5. JSON 模式纯 prompt 工程

**位置**: `src/openai_adapter/request/prompt.rs:76-98`

**现象**: `response_format: { type: "json_object" }` 变为中文 prompt 注入 "请直接输出合法的 JSON 对象"。DeepSeek API 无原生 JSON 模式参数。模型不遵守时无兜底。

---

## 6. stop_sequences 纯 prompt 依赖

**位置**: `src/openai_adapter/request/normalize.rs:59-63`

**现象**: `stop` 序列被提取为 `Vec<String>`，通过 `StreamCfg` 传入响应管道用于过滤（代码处理），但同时也通过 prompt 文本让模型自行停止（LLM 依赖）。两部分职责混淆 —— 如果 prompt 中的 stop 指令和代码过滤行为不一致，会产生歧义。

---

## 7. Anthropic server tools 静默丢弃

**位置**: `src/anthropic_compat/request.rs:416-417`

**现象**: `ToolUnion::Other(_)`（如 `bash_20250124`）在 `convert_tools_and_choice` 中被 `filter_map` 静默丢弃，不报错不警告。

```rust
ToolUnion::Other(_) => None,  // 静默丢弃
```

**影响**: 客户端不知道自己请求的 server tool 被忽略了。

---

## 8. Thinking budget_tokens 丢失

**位置**: `src/anthropic_compat/request.rs:35-38`

**现象**: Anthropic 的 `ThinkingConfig::Enabled { budget_tokens: 2048 }` 仅映射为 `reasoning_effort: "high"`。具体预算值丢失。

---

## 记录时间

2026-05-09 — 初始记录，待与 context-session-issues.md 统一处理
