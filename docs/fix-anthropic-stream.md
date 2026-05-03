# Fix: Anthropic 流式响应异常修复

> 分支：`fix/anthropic-stream`
> 基于：`v0.2.5-10-g906acbd`
> 日期：2026-05-03

## 问题列表

### 问题 1：Keepalive 保活块注入虚假 thinking 内容

**症状**：使用工具调用时，Claude Code 客户端的 `thinking` 块内容中出现 `"tool_calls..."` 字样，干扰模型正常的思考内容。

**根因**：`ToolCallStream`（`openai_adapter/response/tool_parser.rs`）在收集 XML 工具调用期间，每 1 秒发出 `chatcmpl-keepalive` 块以维持连接。`AnthropicStream` 的 `handle_chunk` 将保活块转换为 `ContentBlockDelta(Thinking("tool_calls..."))`，注入到用户的 thinking 内容中。

**修复**：跳过保活块的处理，不产生任何 Anthropic 事件。保活块在 Anthropic 协议中无意义。

**文件**：`src/anthropic_compat/response/stream.rs`
- 移除 `handle_chunk` 中 keepalive → thinking 转换逻辑（第 93-113 行原代码）
- 替换为直接 `return events;`（跳过处理）

### 问题 2：流结束缺少 message_stop

**症状**：中等长度响应（如 "写猫的故事" + max_tokens=200）中，Anthropic SSE 流在最后一个 `content_block_delta` 后直接结束，缺少 `content_block_stop`、`message_delta`、`message_stop`。实测 5/5 失败率。

**根因**：当内层流（OpenAI ChunkStream）返回 `Poll::Ready(None)` 时，`AnthropicStream` 执行优雅关闭逻辑，添加 graceful shutdown 事件到 `pending_events` 并返回第一个事件。但在后续 poll 中再次 poll 内层流时，内层流可能返回 `Poll::Pending` 而非 `None`，导致 `AnthropicStream` 也返回 `Poll::Pending`，剩余事件永远不被消费。

**修复**：添加 `inner_done` 标志。当内层流返回 `None` 时立即设置 `inner_done = true`，后续 poll 在 `pending_events` 排空后直接返回 `None`，不再 poll 内层流，确保所有 graceful shutdown 事件被正确发出。

**文件**：`src/anthropic_compat/response/stream.rs`
- `AnthropicStream` 结构体新增 `inner_done: bool` 字段
- `new()` 中初始化为 `false`
- `poll_next()` 开头新增 `inner_done` 短路检查：`if *this.inner_done { return Poll::Ready(None); }`
- `Poll::Ready(None)` handler 中设置 `*this.inner_done = true`

### 问题 3：工具重复调用（未复现）

10 个城市并发工具调用测试中，结果正确——10 个独立 `tool_use` 块，各自有唯一 ID，正确结束。暂不处理。

## 改动文件清单

| 文件 | 改动 |
|------|------|
| `src/anthropic_compat/response/stream.rs` | 修复 1 + 修复 2 |
| `AGENTS.md` | 新增 Fork 管理章节（分支工作流、上游同步、合并前检查） |

## 验证方法

```bash
# 测试 keepalive 修复（工具调用场景，不应出现 "tool_calls..." thinking）
curl -s -X POST http://127.0.0.1:5317/anthropic/v1/messages \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <KEY>" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "deepseek-v4-flash",
    "max_tokens": 200,
    "stream": true,
    "messages": [{"role": "user", "content": "What is 2+3? Use the get_weather tool."}],
    "tools": [{"name": "get_weather", "description": "Get weather", "input_schema": {"type": "object", "properties": {"location": {"type": "string"}}, "required": ["location"]}}]
  }' | grep "tool_calls" | head -3
# 预期：不应出现包含 "tool_calls" 的 thinking_delta

# 测试 message_stop 修复（中等长度响应）
curl -s -X POST http://127.0.0.1:5317/anthropic/v1/messages \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <KEY>" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "deepseek-v4-flash",
    "max_tokens": 200,
    "stream": true,
    "messages": [{"role": "user", "content": "Write a short story about a cat."}]
  }' | grep -c "message_stop"
# 预期：输出 "1"（表示收到了 message_stop 事件）
```
