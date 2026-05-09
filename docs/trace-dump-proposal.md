# 请求级 Trace Dump 方案

## 背景

当前 ds-free-api 的日志系统按**层级**组织（handler → adapter → ds_core → response），排查问题时需跨 5-6 个 target grep，且缺少请求体、响应体、翻译中间产物的记录。

参考 [claude-tap](https://github.com/liaohch3/claude-tap) 的设计：以请求为单位，把完整往返结构化记录，提供可视化界面快速定位问题。

## 对比分析

| 能力 | claude-tap | ds-free-api 现状 |
|------|-----------|-----------------|
| 请求捕获 | 代理层拦截原始 HTTP | `handlers.rs` 只记录路径+stream 标记 |
| 请求体 | 完整 JSON 保留 | 不记录（prompt 只在 TRACE 可见） |
| 响应体 | 完整保留 | 不记录（SSE 只在 TRACE 可见） |
| 请求分组 | 天然按请求分组 | `req={}` 散落在各层日志中 |
| 前后对比 | 结构 diff 查看两次请求差异 | 无 |
| 可视化 | 自包含 HTML viewer（搜索、模型分组、token 分解） | Admin 面板只有 raw log 列表 |
| 实时追踪 | `--tap-live` 实时浏览器 viewer | 无 |

## ds-free-api 可借鉴的两点

### 1. 请求级 Trace Dump（轻量，高价值）

思路：在每个请求完成时，将完整往返 dump 到一个结构化 JSON 文件。

```json
{
  "timestamp": "2026-05-09T15:30:00Z",
  "request_id": "req-123",
  "account": "user1@example.com",
  "input": {
    "protocol": "anthropic",
    "model": "deepseek-expert",
    "message_count": 5,
    "tool_count": 3,
    "tool_names": ["Read", "Bash", "Write"],
    "max_tokens": 16384,
    "thinking": "enabled",
    "search": "disabled",
    "request_summary": "..."
  },
  "intermediate": {
    "prompt_len": 4500,
    "prompt_sample": "<|System|>...",
    "model_type": "expert",
    "thinking_enabled": true,
    "file_count": 1,
    "session_id": "abc123"
  },
  "output": {
    "stop_reason": "tool_use",
    "content_block_types": ["thinking", "text", "tool_use"],
    "tool_call_names": ["Read"],
    "input_tokens": 5000,
    "output_tokens": 200,
    "error": null
  },
  "timing": {
    "total_ms": 3200,
    "first_byte_ms": 1500,
    "ds_core_ms": 2800
  }
}
```

实施位置：`openai_adapter.rs` 的 `chat_completions()` 方法返回点。在该方法中已持有所有需要的信息（`req`、`prompt`、`model_res`、`file_result`、`chat_resp`、`account_id`）。

存储机制：
- 文件写入 `{DS_DATA_DIR}/traces/req-{n}-{timestamp}.json`
- 单文件上限 ~50KB，保留最近 100 个文件（~5MB）
- 与 `runtime_log` 类似的轮转策略

### 2. 自有 HTML Viewer

复用 claude-tap viewer 的交互模式：
- 搜索（全文、模型、工具名）
- 展开/折叠请求详情
- Token 分解（input/output/cache）
- 请求间 diff（快速发现上下文变化）

实施方式：
- 新增 API endpoint：`GET /admin/api/traces`（返回 trace 列表）
- 新增 API endpoint：`GET /admin/api/traces/{id}`（返回单个 trace 详情）
- Admin 面板新增 "请求追踪" Tab

## 实施建议

### 第一步（解决当前盲区）

在 `openai_adapter.rs` 的请求处理完成点增加 trace dump：

```
需新增代码位置:
- src/openai_adapter.rs:170 (stream 分支出口)
- src/openai_adapter.rs:260 (非 stream 分支出口)
- src/openai_adapter.rs:299 (重试耗尽出口)
- src/server/store.rs 或新增 trace.rs (trace 文件的读写管理)

数据收集点:
- adapter 入口: req (ChatCompletionsRequest)
- prompt::build 后: prompt 字符串
- ds_core 返回: account_id, session_id
- response 管道后: stop_reason, usage, content blocks
- 全程 timing: Instant 计时
```

代码量估算：~200 行（trace 结构体 + serialize + 文件写 + 轮转）

### 第二步（后续可视化）

Admin 面板新增 Tab，复用 Admin 现有模式（React + shadcn/ui）。前端代码量估算：~500 行。

## 与现有日志系统的关系

Trace dump **不替代**现有日志，而是互补：

| 维度 | runtime_log | trace dump |
|------|-----------|------------|
| 粒度 | 单条日志 | 完整请求 |
| 用途 | 实时监控、告警 | 事后排查、审计 |
| 内容 | 关键事件摘要 | 入站/出站/翻译全文 |
| 保留 | 环形 2000 条 + 40MB 文件 | 最近 100 个请求 |
| 查看 | API 查询 / runtime.log | Admin 面板 / 直接读文件 |

## 记录时间

2026-05-09 — 初始方案，待讨论后实施
