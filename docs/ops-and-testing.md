# 运维与测试现状

## 运维面

### 当前能力

| 能力 | 状态 | 位置 |
|------|------|------|
| 配置热重载 | ✅ | Admin 面板 PUT /admin/api/config，Config::save() 即时生效 |
| 优雅关闭 | ✅ | SIGTERM/SIGINT → `shutdown_signal()` → adapter.shutdown() |
| 日志轮转 | ✅ | runtime.log 10MB×3 文件，管理面板可查 |
| 账号状态查看 | ✅ | Admin Dashboard / GET /admin/api/status |
| 登录限流 | ✅ | `LoginLimiter`（管理面板登录） |

### 未覆盖

| 能力 | 状态 | 说明 |
|------|------|------|
| 请求限流 | ❌ | API 端点无限流，依赖账号池自然瓶颈 |
| 连接数限制 | ❌ | axum 默认无限制 |
| 内存上限 | ❌ | wasmtime + 并发 SSE 流可无限增长 |
| 并发排队 | ⚠️ | `get_account_with_wait(30s)` 超时即返回 Overloaded，无队列 |

### pool_max_active 建议

```
单个 Claude Code 终端：2-3 个活跃账号（请求间快速轮转）
不设上限 (0)：所有账号活跃，多占 DeepSeek session 但无功能影响
```

pool_max_active = 0 时，多余账号以 Standby 状态入池。活跃账号出错时 `try_activate_standby()` 自动替补。

### 关键运维命令

```bash
# 优雅重启：kill -SIGTERM 等待优雅关闭后再启动
kill -TERM $(pgrep ds-free-api)

# 热重载配置（无需重启）
curl -X PUT http://127.0.0.1:22217/admin/api/config -d @config.toml

# 查看账号池状态
curl http://127.0.0.1:22217/admin/api/status

# 监控运行时日志
tail -f runtime.log

# 追踪单个请求
RUST_LOG=adapter=debug,ds_core::accounts=debug 2>&1 | grep 'req=req-'
```

---

## 测试面

### 现有覆盖

```
py-e2e-tests/scenarios/
├── basic/              14 个场景
│   ├── openai/   (7)   chat / reasoning / stream / tools / document / image_base64 / image_http
│   └── anthropic/(7)   chat / reasoning / stream / tools / file_inline / image_base64 / image_http
└── repair/             10 个场景
    └── openai/   (10)  XML 格式解析 / JSON 损坏修复变体
```

### 缺失的关键场景

| 缺失 | 重要度 | 原因 |
|------|--------|------|
| 多轮对话 + 工具往返 | 🔴 | 用户实际场景（Agent 调工具 → 回传结果 → 继续生成） |
| Anthropic 工具调用 | 🔴 | repair 测试全是 OpenAI 格式，未测 Anthropic tool_use→XML→tool_use |
| 流中断（客户端断开） | 🟡 | GuardedStream::drop 的 stop_stream + delete_session 路径 |
| 账号切换/过载重试 | 🟡 | 多账号场景下的轮转和 6 次退避重试 |
| 并发请求 | 🟡 | 多个请求同时打到同一账号池 |
| 配置热重载 | 🟢 | 运行时修改 pool_max_active / models / tool_call tags |

### e2e 测试执行

```bash
# 启动测试服务器
just e2e-serve

# 基础功能（OpenAI + Anthropic 双端点）
just e2e-basic

# 工具调用修复专项
just e2e-repair

# 全部场景 × 3 迭代压测
just e2e-stress

# 单场景快速验证
uv run python runner.py scenarios/basic --filter 流式 --parallel 1
```

### 测试框架结构

```
py-e2e-tests/runner.py      # 统一入口：加载 JSON 场景 → 发送请求 → 执行 checks
scenarios/{name}.json       # 场景定义：request + checks（content_not_empty / has_tool_calls / tool_names / stop_reason）
```

添加新场景只需在 `scenarios/basic/` 或新目录下放 JSON 文件，无需改 runner 代码。

---

## 记录时间

2026-05-09
