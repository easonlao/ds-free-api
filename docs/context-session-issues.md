# 会话与上下文管理已知问题

本文档记录 ds_core 会话管理机制中已识别的潜在问题。不包含具体修复方案，仅作为问题清单供后续统一处理。

---

## 1. `split_history_prompt` think 分支上下文截断

**位置**: `src/ds_core/completions.rs:720-739`

**现象**: 当前一轮 assistant 响应包含 `<think>` 标签时，think 块之后的所有消息（含最新的用户消息）被静默丢弃。

**原因**: think 分支的 history 构建仅取 `blocks[..think_idx]`（think 之前的块），`blocks[think_idx+1..]`（think 之后的块）被忽略。

```
输入 prompt:
<｜System｜>...工具定义...
<｜User｜>帮我查天气
<｜Assistant｜><think>需要调用工具</think><|tool_calls_begin|>...
<｜User｜>工具返回: 北京 25°C
<｜Assistant｜>

解析后的块: [system, user"查天气", assistant+think, user"工具返回", assistant""]
                                         think_idx = 2

当前行为:
  inline  = block[2]           ← assistant+think 块
  history = block[0..2]        ← system + user"查天气"
  丢失    = block[3..5]        ← user"工具返回" + assistant""  ← 最新消息被丢弃！
```

**影响**: 工具调用多轮对话中，工具返回结果丢失，模型看不到工具输出，导致幻觉或上下文混乱。

**注释与实现不一致**: 注释写"history = 其余所有块"，但代码实际只取了 think 之前的块。

---

## 2. 文件上传失败回退可能导致静默截断

**位置**: `src/ds_core/completions.rs:349-353`

**现象**: 历史文件上传失败时，回退为完整原始 prompt 内联发送，无 token 长度校验。

```rust
let completion_prompt = if history_upload_failed {
    &req.prompt  // 完整原始 prompt，可能远超 token 窗口
} else {
    inline_prompt
};
```

**影响**: 长对话时，完整 prompt 可能超出 DeepSeek token 限制，模型只看到截断内容，无告警、无报错。

---

## 3. 会话清理不可靠

**位置**: `src/ds_core/completions.rs:68-98` (`GuardedStream::drop`)

**现象**: 会话清理（`stop_stream` + `delete_session`）在 `tokio::spawn` 异步任务中执行。以下场景会导致清理失败：

- 进程收到 SIGKILL / OOM kill
- tokio runtime 在 spawn 任务完成前关闭
- 网络断开导致 API 调用失败（仅 warn 日志，不重试）

**影响**: DeepSeek 侧残留垃圾 session。不影响功能，但累积浪费配额资源。

---

## 4. Bearer Token 无主动刷新

**位置**: `src/ds_core/accounts.rs:645-669` (`start_recovery_task`)

**现象**: Token 仅在账号变为 `Error` 状态后才触发重登。重扫间隔为 **1 小时**。

- 若 token 在两次扫描之间过期，该账号的请求全部失败
- 正常 `Idle`/`Busy` 账号的 token 永不会自动刷新（除非发生错误）

**影响**: 取决于 DeepSeek token 有效期。若有效期短于 1 小时，会出现周期性全账号不可用。

---

## 5. `active_sessions` 仅为追踪用途

**位置**: `src/ds_core/completions.rs:153`

**现象**: `active_sessions: Arc<Mutex<HashMap<String, ActiveSession>>>` 仅在 `GuardedStream::new` 时插入、`drop` 时移除，无业务使用。`Arc<Mutex>` 在高并发 session 创建/销毁时有轻微锁竞争。

**影响**: 当前无功能影响（仅追踪），但如果未来依赖此 map 做业务判断，需要注意并发一致性。

---

## 记录时间

2026-05-09 — 初始记录，待统一修复
