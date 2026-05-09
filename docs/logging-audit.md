# 日志覆盖审计

本文档记录从请求到响应的全管道日志覆盖审计结果。对照 logging-spec.md 定义的 target/级别规范，逐段排查缺失。

---

## 管道逐段审计

### 1. HTTP 请求入口 —— handlers.rs

| Target | 级别 | 内容 | 状态 |
|--------|------|------|------|
| `http::request` | DEBUG | `req={} POST /v1/chat/completions stream={}` | ✅ |
| `http::request` | DEBUG | `req={} POST /anthropic/v1/messages stream={}` | ✅ |
| — | — | 请求体摘要（message 数、tool 数、max_tokens） | ❌ 缺失 |
| — | — | Anthropic 原始请求内容 | ❌ 缺失 |

### 2. Anthropic→OpenAI 映射 —— anthropic_compat/request.rs

| Target | 级别 | 内容 | 状态 |
|--------|------|------|------|
| — | — | 整段映射函数 `into_chat_completions()` 零日志 | ❌ 缺失 |
| — | — | web_search 自动注入 | ❌ 缺失 |
| — | — | server tools 过滤 | ❌ 缺失 |

### 3. OpenAI 适配器入口 —— openai_adapter.rs

| Target | 级别 | 内容 | 状态 |
|--------|------|------|------|
| `adapter` | DEBUG | `req={} 适配器开始处理: model={}, stream={}` | ✅ |
| `adapter` | INFO | 重试成功 | ✅ |
| `adapter` | WARN | 过载 / 标签泄漏 | ✅ |
| — | — | `thinking_enabled` / `search_enabled` 实际值 | ❌ 缺失 |
| — | — | 文件提取结果（数量、大小） | ❌ 缺失 |
| — | — | 适配器处理完成（出口日志） | ❌ 缺失 |

### 4. Prompt 构建 —— prompt.rs + tools.rs

| Target | 级别 | 内容 | 状态 |
|--------|------|------|------|
| `adapter` | DEBUG | `[tc] prompt_build messages={} len={} tool_inject={}` | ✅ |
| `adapter` | INFO | `len > 10000` 上下文膨胀警告 | ✅ |
| `adapter` | DEBUG | `[tc] tool_extract n={} names=[...] tool_choice={}` | ✅ |
| — | — | prompt 实际内容（仅 TRACE 在 ds_core 侧可见） | ⚠️ 跨层 |
| — | — | stop_sequences 注入 | ❌ 缺失 |

### 5. ds_core 聊天编排 —— completions.rs (target: `ds_core::accounts`)

| Target | 级别 | 内容 | 状态 |
|--------|------|------|------|
| `ds_core::accounts` | DEBUG | 分配账号 / 创建 session / PoW 完成 / SSE ready | ✅ |
| `ds_core::accounts` | TRACE | completion prompt + ref_file_ids + raw SSE bytes | ✅ |
| `ds_core::accounts` | WARN | 文件上传失败 / hint 限流 / 账号池空 | ✅ |
| — | — | 文件上传成功（数量、大小、耗时） | ❌ 缺失 |

### 6. DeepSeek HTTP 客户端 —— client.rs

| Target | 级别 | 内容 | 状态 |
|--------|------|------|------|
| `ds_core::client` | WARN | WAF 拦截（含详细提示文案） | ✅ |
| — | — | HTTP 请求/响应摘要（端点、状态码、耗时） | ❌ 缺失 |
| — | — | API 调用耗时 | ❌ 缺失 |

### 7. 响应管道 —— response/* (target: `adapter`)

| 层 | Target | 级别 | 内容 | 状态 |
|----|--------|------|------|------|
| sse_parser | `adapter` | DEBUG | event type + data length | ✅ |
| sse_parser | `adapter` | TRACE | 事件内容 | ✅ |
| state | `adapter` | TRACE | frame 输出 | ✅ |
| converter | `adapter` | TRACE | 每类 DsFrame | ✅ |
| converter | `adapter` | WARN | 幻觉检测 / 抑制 | ✅ |
| converter | `adapter` | WARN | 流提前结束 | ✅ |
| tool_parser | `adapter` | TRACE | 检测/收集/解析/修复 全流程 | ✅ |
| tool_parser | `adapter` | DEBUG | `[tc] result→ok n={} names=[...]` | ✅ |
| tool_parser | `adapter` | WARN | 解析失败 / 空标签对 / 缓冲区溢出 | ✅ |

### 8. Anthropic 出站映射 —— anthropic_compat/response/stream.rs

| Target | 级别 | 内容 | 状态 |
|--------|------|------|------|
| `anthropic_compat::response::stream` | DEBUG | 映射启动 / 流结束 `started={} finished={}` | ✅ |
| `anthropic_compat::response::stream` | TRACE | 每个入站 chunk | ✅ |
| — | — | 出站 Anthropic 事件（仅 `types.rs` TRACE 打 SSE 文本） | ⚠️ 跨文件 |
| — | — | 出站事件类型序列（message_start→content_block→...→message_stop）| ❌ 缺失 |

### 9. HTTP 响应出口 —— handlers.rs

| Target | 级别 | 内容 | 状态 |
|--------|------|------|------|
| `http::response` | DEBUG | `req={} 200 SSE stream started` | ✅ |
| `http::response` | DEBUG | `req={} 200 JSON response {} bytes` | ✅ |
| — | — | 端到端延迟（请求到达 → 首字节 → 尾字节） | ❌ 缺失 |
| — | — | 响应 `x-ds-account` header | ❌ 缺失 |

---

## 缺口汇总

### 🔴 高优先级

| # | 位置 | 问题 |
|---|------|------|
| 1 | `anthropic_compat/request.rs` | 全段零日志，Anthropic→OpenAI 映射完全不可见 |
| 2 | 全管道 | 无端到端延迟指标（请求到达 / 首字节 / 尾字节） |

### 🟡 中优先级

| # | 位置 | 问题 |
|---|------|------|
| 3 | `openai_adapter.rs` | 有入口无出口；thinking/search/file 开关值不记录 |
| 4 | `ds_core/client.rs` | HTTP 请求/响应无摘要，API 耗时不可见 |
| 5 | handlers.rs | 请求体摘要缺失（message 数、tool 数），排查效率低 |
| 6 | completions.rs | 文件上传成功不记录（数量、大小、耗时） |

### 🟢 低优先级

| # | 位置 | 问题 |
|---|------|------|
| 7 | `anthropic_compat/response/stream.rs` | 出站 Anthropic 事件 DEBUG 级别不可见 |
| 8 | prompt.rs | stop_sequences 注入不记录 |

---

## 当前可用的调试 RUST_LOG 组合

```bash
# 端到端追踪（日志量大）
RUST_LOG=adapter=trace,ds_core::accounts=trace,anthropic_compat::response::stream=trace

# 核心路径排查（推荐日常）
RUST_LOG=adapter=debug,ds_core::accounts=debug

# 工具调用排查
RUST_LOG=adapter=trace 2>&1 | grep '\[tc\]'

# 请求追踪（单一请求）
RUST_LOG=adapter=debug,ds_core::accounts=debug 2>&1 | grep 'req=req-1'
```

---

## 记录时间

2026-05-09 — 初始记录，待后续补充日志后更新
