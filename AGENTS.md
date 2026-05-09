# AGENTS.md

本文件为 Claude Code（claude.ai/code）在此仓库中工作时提供指引。

> 此文件同时承担 `AGENTS.md`（本体）和 `CLAUDE.md`（符号链接 → `AGENTS.md`）双重职责。
> 直接编辑 `AGENTS.md`；`CLAUDE.md` 自动保持同步。

---

## 项目概述

Rust API 代理，对外暴露免费的 DeepSeek 模型端点。将标准的 OpenAI 兼容及 Anthropic 兼容请求转换为 DeepSeek 内部协议，具备账号池轮转、PoW 挑战应答及流式响应支持。

**运行时：** Rust **1.95.0**（锁定于 `rust-toolchain.toml`），**edition 2024**。

**关键依赖及其存在理由：**
- `wasmtime` — 执行 DeepSeek 的 PoW WASM 求解器；整个 PoW 系统依赖此 crate
- `tiktoken-rs` — 客户端 prompt token 计数（DeepSeek 对 `prompt_tokens` 返回 0）
- `pin-project-lite` — 支撑所有流式响应包装器（`SseStream`、`StateStream` 等）
- `axum` / `rquest` — 分别为 HTTP 服务端和客户端；`rquest` 使用 BoringSSL + Chrome 136 TLS 指纹以绕过 WAF
- `tokio`（启用 `signal` feature）— 异步运行时，支持 SIGTERM/SIGINT 优雅关闭

---

## 架构

### 模块结构

```
src/
├── main.rs              # 二进制入口（约 10 行）：init runtime_log，解析 CLI（load_with_args），启动 server
├── lib.rs               # 公开 API：重新导出 Config、DeepSeekCore、OpenAIAdapter 等
├── config.rs            # 配置加载/保存（config.toml，支持 -c / DS_CONFIG_PATH），Arc<RwLock<Config>>
│
├── ds_core/             # DeepSeek 实现门面（src/ds_core.rs）
│   ├── ds_core.rs       # 门面：DeepSeekCore、CoreError；声明子模块
│   ├── accounts.rs      # 账号池：初始化验证、空闲感知选择、AccountGuard（Drop → 释放）、Standby 替补机制
│   ├── pow.rs           # PoW 求解器：wasmtime WASM 加载、动态导出探测、DeepSeekHashV1 计算
│   ├── completions.rs   # 聊天编排：create_session → upload → PoW → stream → GuardedStream
│   └── client.rs        # 原始 HTTP 客户端：API 端点、Envelope 解析，零业务逻辑
│
├── openai_adapter/      # OpenAI 协议适配器门面（src/openai_adapter.rs）
│   ├── openai_adapter.rs # 门面：OpenAIAdapter、OpenAIAdapterError、StreamResponse
│   ├── types.rs         # 请求/响应结构体（ChatCompletionsRequest 等）
│   ├── models.rs        # 模型注册与列表端点
│   ├── request/         # 请求管道：normalize → tools → files → prompt → resolver → tiktoken
│   │   ├── request.rs   # 子模块门面
│   │   ├── normalize.rs # 校验、默认参数
│   │   ├── tools.rs     # 工具定义 → prompt 注入（含负面示例防止幻觉格式）
│   │   ├── files.rs     # Data URL → FilePayload，HTTP URL → 搜索模式
│   │   ├── prompt.rs    # ChatML → DeepSeek 原生标签，工具注入
│   │   ├── resolver.rs  # 模型解析，能力开关
│   │   └── tiktoken.rs  # Token 计数
│   └── response/        # 响应管道：sse_parser → state → converter（含幻觉过滤）→ tool_parser
│       ├── response.rs  # 门面 + StreamCfg 结构体
│       ├── sse_parser.rs    # SseStream：原始字节 → SseEvent（event+data）
│       ├── state.rs         # StateStream：DeepSeek JSON 补丁 → DsFrame
│       ├── converter.rs     # ConverterStream：DsFrame → ChatCompletionsResponseChunk，含 ContentDelta 幻觉文本过滤器
│       └── tool_parser.rs   # ToolCallStream：XML 标签检测，滑动窗口修复，空标签对抑制
│
├── anthropic_compat/    # Anthropic 协议转换器（架于 openai_adapter 之上）
│   ├── anthropic_compat.rs # 门面
│   ├── types.rs         # Anthropic 协议类型定义
│   ├── models.rs        # Anthropic 格式的模型列表
│   ├── request.rs       # Anthropic JSON → OpenAI 请求映射
│   └── response/
│       ├── stream.rs    # OpenAI SSE → Anthropic SSE 事件
│       └── aggregate.rs # OpenAI JSON → Anthropic JSON
│
└── server/              # HTTP 服务端门面（src/server.rs）
    ├── server.rs        # 门面：axum router、auth 中间件、优雅关闭
    ├── admin.rs         # 管理面板路由（setup、login、config、stats、keys、models）
    ├── auth.rs          # JWT 签名/验证、密码设置/登录、登录速率限制
    ├── error.rs         # ServerError：API 错误 JSON 响应
    ├── handlers.rs      # 业务路由处理（OpenAI + Anthropic），内嵌 TokenGuard + TraceRecord
    ├── runtime_log.rs   # 文件日志重定向（stdout → runtime.log），支持 target=level 过滤
    ├── stats.rs         # 请求统计记录（RequestStats、StatsHandle）
    ├── store.rs         # StoreManager：管理/密钥 委托给 Config::save()，统计 → stats.json
    └── stream.rs        # SseBody：包装 StreamResponse → axum::body::Body
```

**附加资源：**
- `config.example.toml` — 权威配置参考，所有字段均有文档说明
- `examples/adapter_cli.rs` + `examples/adapter_cli/` — 调试 CLI + JSON 请求样例
- `py-e2e-tests/` — Python e2e 测试套件（uv 管理，JSON 驱动场景）
- `docs/` — `code-style.md`、`logging-spec.md`、`deepseek-prompt-injection.md`、`deepseek-api-reference.md`

### 二进制 / 库分离

`main.rs` 约 10 行包装：初始化 `runtime_log`，读取 `DS_DATA_DIR`，通过 `Config::load_with_args()` 解析 CLI 参数 → `(Config, PathBuf)`，调用 `server::run(config, config_path)`。crate 可同时构建为库（`cargo build --lib`）和二进制（`cargo build --bin ds-free-api`）。`lib.rs` 定义完整的公开 API。

### 门面模块模式

`ds_core.rs`、`openai_adapter.rs`、`server.rs`、`request.rs`、`response.rs` 及 `anthropic_compat.rs` 均为**门面**：
- 通过 `mod` 声明子模块（保持实现私有）
- 通过 `pub use` 仅重新导出最小公开接口
- 部分门面包含 `#[cfg(test)]` 测试模块

这意味着文件树并不直接对应公开 API。要了解某模块对外暴露了什么，应阅读其门面文件而非目录列表。

### StreamResponse 类型

`StreamResponse` 是适配器层与 HTTP 服务端之间的统一桥梁：
- 每个适配器的流式方法均返回 `StreamResponse`（一个 boxed `Stream<Item = Result<Bytes>> + Send`）
- `server/stream.rs::SseBody` 包装 `StreamResponse` 并将其转换为 `axum::body::Body`
- 这使适配器与 HTTP 框架解耦 —— 适配器产出字节，服务端处理 SSE 帧

### CI 构建流水线

Tag 推送时触发（`.github/workflows/release.yml`）：

```
build-frontend (npm ci + npm run build)
  ├── build-linux-gnu (cargo build, x86_64 + aarch64)
  ├── build-linux-musl (cargo zigbuild, x86_64 + aarch64)
  ├── build-macos (cargo build, x86_64 + aarch64)
  ├── build-windows (cargo build, x86_64 + aarch64)
  └── docker (ghcr.io 镜像)
       └── release (tar.gz + zip)
```

- `build-frontend` 产出 `web-dist` 构件。后续各构建任务下载此构件后再编译 Rust，以确保 `rust_embed` 嵌入真实前端资源
- musl 目标使用 `cargo-zigbuild` + Zig 0.16.0 进行交叉编译
- aarch64-gnu 在原生 ARM runner（ubuntu-24.04-arm）上构建

### 前端（`web/`）

Vite + React + shadcn/ui SPA，位于 `web/` 目录。通过 `npm run build`（于 `web/` 内）构建。
二进制文件在编译时通过 `rust_embed` 嵌入 `web/dist/`。

```
web/
├── src/
│   ├── App.tsx            # 路由（登录 + 受保护布局 + 页面）
│   ├── lib/api.ts         # 带类型的 API 客户端，覆盖所有管理端点
│   ├── lib/auth.tsx       # JWT 认证上下文（localStorage token）
│   ├── pages/             # ConfigPage、DashboardPage、Layout、LoginPage、LogsPage、ModelsPage
│   └── components/ui/     # shadcn/ui 基础组件（badge、button、card、input 等）
├── public/favicon.svg     # → 符号链接至 assets/logo.svg
├── index.html
├── package.json
└── vite.config.ts
```

**管理面板配置编辑器**：`ConfigPage.tsx` 从 `GET /admin/api/config` 拉取配置，编辑全部分节（accounts、api_keys、server、deepseek、models、proxy、tool_call 标签），通过 `PUT /admin/api/config` 提交（全量替换 + 热重载）。密码/密钥字段以 `***` 或空值发送时，服务端将其合并至现有值。

**开发模式（HMR）**：运行 `cd web && npm run dev`（Vite HMR）配合 `just serve`。
后端在可用时从 `web/dist/` 文件系统读取。

---

## 原则

### 1. 单一职责
每个模块只做一件事。模块间边界严格：
- `config.rs`：仅配置加载与保存，不创建客户端，不含业务逻辑
- `client.rs`：仅原始 HTTP 调用，无 token 缓存、重试或 SSE 解析
- `accounts.rs`：仅账号池管理，无网络请求
- `pow.rs`：仅 WASM 计算，不管理账号或发送请求
- `anthropic_compat.rs`：仅协议翻译，不直接访问 `ds_core`

### 2. 最小可行
- 不做过早抽象：需要时再提取 trait/结构体，不提前
- 不写冗余代码：删除未使用的 import，避免过度文档化，不写预设测试
- 延迟引入依赖：仅在确实需要时才添加

### 3. 控制复杂度
- 显式优于隐式：依赖通过参数注入，无全局状态
- 组合优于继承：小型模块通过函数组合，无深层继承
- 清晰边界：模块通过显式接口交互，内部逻辑不泄露

---

## 核心架构模式

### 账号池模型

1 个账号 = 1 个 session = 1 个并发。通过 `config.toml` 中增加账号实现横向扩展。

**活跃上限机制**：`pool_max_active` 配置项限制同时活跃的账号数量。超过上限的账号进入 **Standby**（备用）状态，等待活跃数下降后自动激活。设为 0 时无限制（全部账号均为活跃）。

`AccountGuard` 包装 `Arc<Account>`。创建时将账号标记为 `Busy`（通过 `AtomicBool`），`Drop` 时释放。由 `GuardedStream` 持有以在流式传输期间保持账号占用。

`AccountState` 枚举：`Idle` | `Busy` | `Error` | `Invalid` | `Standby`

### 账号初始化流程

`AccountPool::init()` 并发启动账号（通过 `tokio::sync::Semaphore` 限制最大 13 并发）：
1. `login` — 获取 Bearer token
2. `create_session` — 创建聊天会话
3. `health_check` — 测试一次 completion（含 PoW）以验证会话可写
4. `update_title` — 将会话重命名为 "managed-by-ai-free-api"

每个账号失败时重试 3 次，间隔 2s。若某账号全部重试失败，标记为 `InitFailed`。
初始化的活跃数量受 `pool_max_active` 限制；超出部分初始化为 `Standby`。

### 请求流程（每次对话）

`v0_chat()` → `get_account()` → `split_history()` → `create_session()` → `upload_files()` → `compute_pow()` → `completion()` → `parse_ready()` → `GuardedStream`

每次 `v0_chat()` 调用创建专属会话，将多轮历史作为文件上传，然后流式传输响应。流结束时通过 `GuardedStream::drop` 销毁会话，异常断开时还会调用 `stop_stream`。会话追踪于 `active_sessions: Arc<Mutex<HashMap<String, ActiveSession>>>`。

### 单结构体管道（OpenAI）

适配器对整个请求管道使用**单一结构体**（`ChatCompletionsRequest`）—— 无中间类型：

```
ChatCompletionsRequest
  → normalize::apply |
  → tools::extract   |  直接读取 ChatCompletionsRequest 字段
  → files::extract   |
  → prompt::build    |
  → resolver::resolve|
  → tiktoken
  → try_chat (ds_core::ChatRequest)
  → 若 req.stream → ChatCompletionsResponseChunk | 否则 → ChatCompletionsResponse
```

### 响应管道（OpenAI）— 4 层流式链 + 幻觉过滤器

```
ds_core SSE 字节 → SseStream (sse_parser)
                 → StateStream (state/patch 状态机)
                 → ConverterStream (converter + ContentDelta 幻觉过滤器)
                 → ToolCallStream (tool_parser + 空标签对抑制)
                 → SSE 字节
```

所有流包装器均使用 `pin_project_lite::pin_project!` 宏，并通过 `poll_next` 实现 `Stream`。每个包装器是一个包含内部流和状态的 pinned 结构体，在 `poll_next` 中通过 `Projection` 访问字段。

**ContentDelta 幻觉文本过滤器**（位于 `converter.rs`）：
- 检测模型输出的 Claude Code UI 格式幻觉文本（如 `**Tool Call:**`、`**function_name**` 等）
- 检测到幻觉内容后开始抑制，直到 `<|tool_calls_begin|>` 等真实工具调用标签出现才恢复
- 防止模型将工具调用以自然语言格式输出而非正确的 XML 标签格式

### 工具调用（XML 格式）

工具定义、格式规范和调用指令通过 `<｜System｜>` 块注入（参见 `docs/deepseek-prompt-injection.md`）。
Prompt 末尾以 `<｜Assistant｜>` 干净边界结尾，模型按原生训练行为自行管理 `<think>` 开闭配对。
模型输出的 `<|tool_calls_begin|>...<|tool_calls_end|>` XML 标签由 `ToolCallStream` 解析回结构化 JSON。

**解析流程：**
1. **滑动窗口检测器** 累积 content chunk，扫描 `<|tool_calls_begin|>`（或回退标签）
2. **多标签格式支持**：ASCII `<|tool_calls_begin|>` / `<|tool_calls_end|>`（主标签），`<|tool_calls_start|>` / `<|tool_calls_end|>`（备用），`<|tool_calls|>`（pipe 变体），`<|tool_calls_section_begin|>` / `<|tool_calls_section_end|>`（section 变体）
3. **模糊字符归一化**：U+FF5C → `|`，U+2581 → `_`（SentencePiece 历史变体兜底）
4. **JSON 修复链**：escape_newlines → backslash repair → trailing commas → unquoted keys → single quotes → unicode quotes
5. **回退标签**：通过 `TagConfig.extra_starts` / `extra_ends` 在 `config.toml` 中配置
6. `<|tool_call|>...</|>` 块级标签回退（替代 JSON 数组格式）
7. `arguments` 字段始终归一化为 JSON 字符串
8. 模型输出 `[{...}], [{...}]` 多独立数组时自动拆分解析

**双重防护机制：**
- **ContentDelta 幻觉过滤器**（converter 层）：在 `DsFrame::ContentDelta` 进入 tool_parser 之前即检测并抑制 Claude Code UI 格式的幻觉文本
- **空标签对抑制**（tool_parser 层）：检测 `start` + `end` 标签间仅含空白的无效工具调用并丢弃；流结束时亦做同样清理

**负面示例注入**（`tools.rs`）：在工具定义提示中包含 Claude Code UI 格式的负面示例（`**Tool Call:**` 等），教导模型不要以自然语言输出工具调用，而应使用正确的 XML 标签格式。

主标签对：`<|tool_calls_begin|>` / `<|tool_calls_end|>`。全部使用 ASCII `_` 和 `|`，避免 SentencePiece tokenizer 对 U+2581（`▁`）和 U+FF5C（`｜`）的边界歧义。

### 历史拆分与文件上传

多轮对话通过 `split_history_prompt()` 拆分历史：
- 优先策略：找到最后一个带 `<think>` 的 assistant 块，仅该块内联（保留推理上下文），其余所有块（含之后的 user/tool 消息）进入历史文件
- 无 think 块时：最后一个 user/tool 块到末尾内联，之前的历史进入文件
- 早期轮次包装在 `[file content begin]` / `[file content end]` 标记中，作为 `EMPTY.txt` 上传
- 用户外部文件不再上传到 DeepSeek 会话（本地工具直接处理）
- 上传轮询：3 次尝试，退避间隔 0.5 / 1 / 2s，通过 `fetch_files` 检查文件是否存在

### 能力开关

请求字段映射于 `request/resolver.rs`：
- **推理（Reasoning）**：默认 `"high"`（开启）。设为 `"none"` 以关闭
- **联网搜索**：不启用（由本地工具处理搜索逻辑）
- **文件上传**：不启用（由本地工具直接访问文件系统）
- **响应格式**：`response_format` → prompt 中注入 JSON/schema 文本

### 过载重试

`OpenAIAdapter::try_chat()` 在 `CoreError::Overloaded` 时最多重试 **6 次**，采用**指数退避**（1s → 2s → 4s → 8s → 16s），由 DeepSeek 的 `rate_limit_reached` SSE 提示或所有账号繁忙触发。

### Anthropic 兼容层

位于 `openai_adapter` 之上的纯协议转换器 —— 不直接访问 `ds_core`：
- 请求：`Anthropic JSON → to_openai_request() → OpenAIAdapter::chat_completions() / try_chat()`
- 响应：`OpenAI SSE/JSON → from_chat_completion_stream() / from_chat_completion_bytes() → Anthropic SSE/JSON`
- ID 映射：`chatcmpl-{hex}` → `msg_{hex}`，`call_{suffix}` → `toolu_{suffix}`
- `ToolUnion`（`request.rs` 中）缺失时默认 `Custom` 类型（与 Claude Code 的向后兼容）

### 错误转换链

错误逐层向上传播，在每层模块边界进行转换：

1. **`client.rs`**：`ClientError`（`Http` | `Status` | `Business` | `Json` | `InvalidHeader`）
   - 通过 `Envelope::into_result()` 解析 DeepSeek 的包装信封 `{code, msg, data: {biz_code, biz_msg, biz_data}}`
2. **`accounts.rs`**：`PoolError`（`AllAccountsFailed` | `Client`(ClientError) | `Pow`(PowError) | `Validation` | `Exists`）
3. **`ds_core.rs`**：`CoreError`（`Overloaded` | `ProofOfWorkFailed` | `ProviderError` | `Stream`）
4. **`openai_adapter.rs`**：`OpenAIAdapterError`（`BadRequest` | `Overloaded` | `ProviderError` | `Internal` | `ToolCallRepairNeeded`）
5. **`anthropic_compat.rs`**：`AnthropicCompatError`（`BadRequest` | `Overloaded` | `Internal`）
6. **`server/error.rs`**：`ServerError`（`Adapter`(OpenAIAdapterError) | `Anthropic`(AnthropicCompatError) | `Unauthorized` | `NotFound`(String)）

所有错误均使用 `thiserror` derive 宏。

### 请求追踪与账号标识

每个请求在处理层获得一个 `req-{n}` ID，贯穿 adapter → `ds_core`。关键日志点携带 `req=` 用于跨层追踪：
```bash
RUST_LOG=debug 2>&1 | grep 'req=req-1'
```
HTTP 响应头 `x-ds-account` 将账号标识号向上游传递。

`chatcmpl_id` 贯穿整个流式管道（sse_parser → state → converter → tool_parser），用于细粒度的请求-响应关联追踪。

### HTTP 路由

| 端点 | 处理函数 | 描述 |
|----------|---------|-------------|
| `GET /` | `handlers::root` | 重定向至 /admin |
| `POST /v1/chat/completions` | `handlers::openai_chat` | OpenAI chat completion |
| `GET /v1/models` | `handlers::openai_models` | 列出模型 |
| `GET /v1/models/{id}` | `handlers::openai_model` | 获取模型 |
| `POST /anthropic/v1/messages` | `handlers::anthropic_messages` | Anthropic messages |
| `GET /anthropic/v1/models` | `handlers::anthropic_models` | 列出模型（Anthropic 格式） |
| `GET /anthropic/v1/models/{id}` | `handlers::anthropic_model` | 获取模型（Anthropic 格式） |

可选 Bearer auth 通过配置文件中的 `[[api_keys]]` 控制；列表为空时无需认证。

### 模型 ID 映射

`[deepseek]` 配置中的 `model_types`（默认：`["default", "expert"]`）映射为 OpenAI 模型 ID 格式 `deepseek-{type}`（例如 `deepseek-default`、`deepseek-expert`）。Anthropic 兼容层使用相同 ID。

---

## 约定

### 编码规范

```rust
// import 分组：std → 第三方 → crate → 本地，以空行分隔
use std::sync::Arc;

use serde::Deserialize;

use crate::config::Config;

use super::inner::Helper;
```

- **可见性**：非公开 API 的类型使用 `pub(crate)`；门面模块通过 `mod` 保持子模块私有
- **注释**：源文件中使用中文（团队偏好）
- **错误消息**：面向用户的输出使用中文；内部/调试使用英文
- **命名**：模块/函数使用 `snake_case`，类型/枚举变体使用 `PascalCase`，常量使用 `SCREAMING_SNAKE_CASE`
- **模块文件**：`foo.rs` 声明子模块，`foo/` 包含实现

### 注释规范

遵循 `docs/code-style.md`：
- `//!` — 模块级文档：第一行 = 职责，之后为关键设计决策
- `///` — 公开 API 文档：动词引导，注明副作用和 panic 条件
- `//` — 行内注释：解释"为什么"，而非"是什么"

### 日志规范

- 使用 `log` crate，**必须指定 target**。禁止无 target 的日志（如裸 `log::info!`）。
- 使用的 target：
  - `ds_core::accounts`、`ds_core::client`
  - `adapter`（用于 `openai_adapter`）
  - `http::server`、`http::request`、`http::response`（用于 `server`）
  - `anthropic_compat`、`anthropic_compat::models`、`anthropic_compat::request`、`anthropic_compat::response::stream`、`anthropic_compat::response::aggregate`
- 参见 `docs/logging-spec.md` 了解完整的 target/level 映射
- `runtime_log` 支持 `RUST_LOG` target=level 过滤

### 配置

- `config.toml` 中未注释的值 = 必填；已注释 = 可选，有默认值
- `src/config.rs` 是配置加载的唯一入口 —— 其他模块不得直接读取配置文件
- `Config::load_with_args()` 返回 `(Config, PathBuf)` —— path 传递至 `AppState.config_path` 用于热重载
- `Config` 包装在 `Arc<RwLock<Config>>` 中 —— 运行时可修改，管理面板变更通过 `Config::save()` 自动持久化
- `Config::save()` 原子写入（tmp + rename，0600 权限）。`Config` 现已包含 `AdminConfig`（密码哈希、JWT 密钥）和 `api_keys: Vec<ApiKeyEntry>` —— 无独立 JSON 文件

### 测试

- 所有测试均内联（`#[cfg(test)]` 置于 `src/` 文件中）。无独立 `tests/` 目录
- `request.rs` 含同步单元测试用于解析逻辑
- `response.rs` 含 `tokio::test` 异步测试用于流聚合
- `println!` / `eprintln!` 允许在 `#[cfg(test)]` 内使用以调试失败；库代码中禁止

---

## 反模式

- **不得**创建独立的配置入口点 —— `src/config.rs` 是唯一来源
- **不得**在 `*_core/` 模块之外实现 provider 逻辑
- **不得**提交 `config.toml`（仅提交 `config.example.toml`）
- **不得**在库代码中使用 `println!` / `eprintln!` —— 应使用带 target 的 `log` crate
- **不得**使用无 target 的日志宏 —— 必须始终指定 `target: "…"`
- **不得**从 `anthropic_compat` 直接访问 `ds_core` —— 始终通过 `OpenAIAdapter`
- **不得**在 `src/ds_core/client.rs` 之外添加 `#[allow(...)]` —— 仅原始 HTTP 客户端层允许标记死 API 方法和 API 对称性所需的待反序列化字段
- **不得**将管理/认证配置保留在独立 JSON 文件中（`admin.json`、`api_keys.json`）—— 它们已合并到 `Config` 字段中，通过 `Config::save()` 写入 `config.toml`
- **不得**在未获得用户明确许可的情况下运行 `git checkout`、`git commit` 或 `gh` 命令 —— 破坏性或持久性操作必须事先征得同意

---

## 故障排查

| 问题 | 现象 | 可能原因 / 解决方案 |
|-------|---------|--------------------|
| WASM 加载失败 | 启动时出现 `PowError::Execution` | DeepSeek 重新编译了 WASM。PowSolver 现使用动态导出探测（无硬编码符号）。如 WASM URL 变更，请更新 `config.toml` 中的 `wasm_url` |
| WAF 拦截（非美国） | AWS WAF Challenge 响应（状态 202） | 在 `config.toml` 的 `[proxy]` 中配置非美国代理 |
| WAF 拦截（指纹） | HTTP 403 或连接重置 | `rquest` 配合 BoringSSL 自动模拟 Chrome 136 TLS 指纹。如被拦截，尝试更新 `rquest` 或切换模拟配置 |
| 账号初始化失败 | 所有账号卡在 init 状态 | 凭据错误（登录首先失败）或频率受限（session 过多）。检查配置中的 `[accounts]` |
| 工具调用解析失败 | 响应中无 `tool_calls`，可见原始 XML | 模型输出了解析列表中不存在的标签变体。在 `config.toml` 的 `[deepseek]` 中添加 `extra_starts` / `extra_ends` 回退标签 |
| 工具调用幻觉 | 输出 `**Tool Call:**` 等自然语言格式而非 XML | ContentDelta 过滤器应自动抑制。若未生效，检查负面示例是否正确注入、`pool_max_active` 是否充足 |
| 频率受限 | 反复出现 `CoreError::Overloaded` | 增加账号数量或降低并发。6 次指数退避可应对瞬时峰值 |
| 流中途会话错误 | `invalid message id`、session 未找到 | 通常由 `GuardedStream::drop` 清理处理。若持续出现，检查同一账号的并发访问 |
| 流传输卡住 | 初始连接后无 SSE 事件 | 检查 `RUST_LOG=adapter=trace,ds_core::accounts=debug,info` 以定位管道何处停止 |
| 请求排查困难 | 跨多个 target grep 效率低 | 查看 `{DS_DATA_DIR}/traces/req-*.json`，每次请求的完整 trace 记录含入站摘要、中间产物、出站信息和耗时 |

---

## 功能定位索引

| 任务 | 位置 | 说明 |
|------|----------|-------|
| 配置加载 | `src/config.rs` | 统一入口，支持 `-c` 参数 |
| 配置参考 | `config.example.toml` | 全部字段均有文档说明与示例（权威来源） |
| DeepSeek 聊天流程 | `src/ds_core/` | accounts → pow → completions → client |
| 聊天编排 + 文件上传 | `src/ds_core/completions.rs` | `v0_chat()`、历史拆分、上传重试、`GuardedStream` |
| OpenAI 请求解析 | `src/openai_adapter/request/` | normalize → tools → files → prompt → resolver |
| 工具定义注入（含负面示例） | `src/openai_adapter/request/tools.rs` | 工具 schema → prompt 注入，含 Claude Code UI 格式负面示例 |
| 文件上传提取 | `src/openai_adapter/request/files.rs` | data URL → FilePayload，HTTP URL → 搜索模式 |
| OpenAI 响应转换 | `src/openai_adapter/response/` | sse_parser → state → converter（含幻觉过滤）→ tool_parser |
| ContentDelta 幻觉过滤器 | `src/openai_adapter/response/converter.rs` | 检测并抑制 `**Tool Call:**` 等 Claude Code UI 格式幻觉 |
| 工具调用解析器 & 空标签抑制 | `src/openai_adapter/response/tool_parser.rs` | `TagConfig`（extra_starts/extra_ends）；空标签对检测与抑制 |
| 流管道配置 | `src/openai_adapter/response.rs` | `StreamCfg` 结构体（整合 8 个流参数） |
| Anthropic 兼容层 | `src/anthropic_compat/` | 架于 openai_adapter 之上，不直接访问 ds_core |
| Anthropic 流式响应 | `src/anthropic_compat/response/stream.rs` | OpenAI SSE → Anthropic SSE 事件流 |
| Anthropic 聚合响应 | `src/anthropic_compat/response/aggregate.rs` | OpenAI JSON → Anthropic JSON |
| OpenAI 协议类型 | `src/openai_adapter/types.rs` | 请求/响应结构体，`#![allow(dead_code)]` |
| 模型列表 | `src/openai_adapter/models.rs` | 模型注册与列表 |
| HTTP 服务端/路由 | `src/server/` | handlers → stream → error |
| 账号池管理 | `src/ds_core/accounts.rs` | Standby 替补、pool_max_active、恢复任务（每小时） |
| PoW WASM 求解器 | `src/ds_core/pow.rs` | wasmtime 加载、动态导出探测、DeepSeekHashV1 |
| DeepSeek HTTP 客户端 | `src/ds_core/client.rs` | `Envelope::into_result()`、WAF 检测、全部 API 方法 |
| 统一调试 CLI | `examples/adapter_cli.rs` | 模式：chat / raw / compare / concurrent N / status / models |
| 示例请求 JSON | `examples/adapter_cli/` | 预构建 ChatCompletionsRequest 样例 |
| 脚本化回归测试 | `just adapter-cli -- source examples/adapter_cli-script.txt` | 按序运行全部 JSON 样例 |
| e2e 场景测试框架 | `py-e2e-tests/` | JSON 驱动场景及校验 |
| CI 流水线 | `.github/workflows/ci.yml` | `cargo check + clippy + fmt + audit + machete` + `cargo test` |
| 发布流水线 | `.github/workflows/release.yml` | Tag `v*` → 8 目标、4 平台、CHANGELOG 发布 |
| 代码风格 | `docs/code-style.md` | 注释、命名约定（源文件中使用中文） |
| 日志规范 | `docs/logging-spec.md` | target、级别、及 `log` crate 的消息格式 |
| Prompt 注入策略 | `docs/deepseek-prompt-injection.md` | DeepSeek 原生标签、claude-3.5-sonnet 系统 prompt 研究 |
| API 参考 | `docs/deepseek-api-reference.md` | DeepSeek 端点详情 |
| 管理面板路由 | `src/server/admin.rs` | setup/login/config/status/stats/models/logs 处理函数 |
| JWT 认证 + 密码 | `src/server/auth.rs` | `setup_admin()` / `login_admin()`，JWT 签名/验证，登录速率限制 |
| Store 管理器 | `src/server/store.rs` | API key 验证、统计持久化、管理/密钥委托给 `Config::save()` |
| 请求统计 | `src/server/stats.rs` | `RequestStats`、`StatsHandle`，后台刷新至 `stats.json` |
| Runtime 日志 | `src/server/runtime_log.rs` | stdout 重定向至 `runtime.log`（含轮转），支持 target=level 过滤 |

---

## Fork 管理（easonlao/ds-free-api）

### 远程仓库设置
- `origin` = `easonlao/ds-free-api`（你的 fork）
- `upstream` = `NIyueeE/ds-free-api`（原始作者）

### 分支工作流
- 功能/修复分支从 `main` 分出，不直接从 upstream 分出
- 命名：`fix/<描述>`、`feat/<描述>`、`refactor/<描述>`
- 始终在分支中工作，永远不要直接在 `main` 或 `upstream/*` 上操作

### 与上游同步
```bash
# 将 main 与上游同步
git checkout main
git pull upstream main
git push origin main

# 将功能分支变基到最新上游
git checkout fix/anthropic-stream
git rebase main
# 解决冲突，如已推送则强制推送
git push --force-with-lease origin fix/anthropic-stream

# 修复已在上游合并或已废弃后，清理本地分支
git branch -d fix/anthropic-stream
git push origin --delete fix/anthropic-stream
```

### PR 合并前检查
1. 在功能分支上执行 `git rebase main`
2. `cargo test` 全部通过
3. `cargo clippy -- -D warnings` 无告警
4. `cargo build --release` 成功
5. 通过实际 e2e 测试验证：`just e2e-serve` 后 `just e2e-basic`

---

## 常用命令

```bash
# 设置（首次运行自动创建配置；仅当需要默认值时复制示例配置）

# 启用 pre-commit hook（check + clippy + fmt + audit + machete + cargo test）
git config core.hooksPath .githooks

# 一站式检查（check + clippy + fmt + audit + 未使用依赖）
just check

# 启动 HTTP 服务，基本日志级别
just serve
RUST_LOG=info just serve
# 追踪整个 SSE 管道
RUST_LOG=adapter=trace,ds_core::accounts=debug,info just serve
# 模块级日志过滤
RUST_LOG=ds_core::accounts=debug,ds_core::client=warn,info just serve
RUST_LOG=adapter=debug,anthropic_compat=debug just serve

# 运行统一协议调试 CLI（模式：chat、raw、compare、concurrent N、status、models、model <id>）
just adapter-cli
RUST_LOG=debug just adapter-cli
# 脚本模式 —— 按序运行全部 JSON 样例（完整回归）
just adapter-cli -- source examples/adapter_cli-script.txt
# 使用指定配置的交互模式
cargo run --example adapter_cli -- -c /path/to/config.toml

# 运行特定测试模块
just test-adapter-request
just test-adapter-response
just test-adapter-request converter_emits_role_and_content -- --exact

# 运行单个 Rust 测试（使用 -- --exact 精确匹配名称）
cargo test converter_emits_role_and_content -- --exact

# 运行全部 Rust 测试
cargo test

# 仅运行库测试（跳过示例编译，迭代更快）
cargo test --lib

# e2e 测试（需要 `uv`，服务端口 22217）
just e2e-basic    # Basic：基础功能测试（OpenAI + Anthropic 双端点）
just e2e-repair   # Repair：工具调用损坏修复专项测试
just e2e-stress   # Stress：全部场景 × 3 次迭代压测
# 详见 docs/development.md 了解完整 e2e CLI 参数（filter、parallel、model、report 等）

# 使用 e2e 配置启动服务
just e2e-serve

# 查看请求级 trace（每请求完整记录）
ls traces/                            # 查看所有 trace 文件
cat traces/req-1.json                 # 查看单次请求 trace（入站/出站/耗时/error）

# 单项检查
cargo check
cargo clippy -- -D warnings
cargo fmt --check
cargo audit        # 需要：cargo install cargo-audit
cargo machete      # 需要：cargo install cargo-machete

# 构建
cargo build
cargo build --release

# 发布（tag 推送触发 CI：8 目标、4 平台，aarch64 在 ARM runner 上构建）
git tag v0.x.x
git push origin v0.x.x
# CI 从 CHANGELOG.md 中提取 changelog，创建 GitHub Release
```
