# 工具调用全链路追踪日志设计

## 背景

复杂任务中工具调用频繁出现解析失败、输出截断、行为漂移。当前日志仅记录"解析出 N 个工具调用"或"解析失败"，完全看不到：
- 模型实际输出了什么原始内容
- 解析失败时 buffer 里是什么
- JSON 修复尝试了什么、哪步失败
- 最终发给客户端的是什么

**目标**：在零新基础设施的前提下，让工具调用全链路可观测，定位复杂任务下的失效根因。

## 方案：内联结构化日志

在每个管线环节添加 `debug!`/`trace!`/`warn!` 日志，内容截断至安全长度，统一前缀 `[tc]` 便于 grep。

## 日志断面

### 第一节：请求侧 — 工具注入明细

**文件**: `src/openai_adapter/request/tools.rs`

| 位置 | 级别 | 内容 |
|------|------|------|
| `extract()` 返回后 | `debug` | 工具数量、名称列表、tool_choice、parallel_tool_calls |
| `extract()` 返回后 | `debug` | 三段文本(format/defs/instruction)是否生成 |
| `build_tool_instruction_block()` | `trace` | 注入的工具名列表（不记录全文模板） |

格式示例:
```
[tc] tool_extract n=3 names=[get_weather, search, read_file] tool_choice=auto parallel=true
[tc] tool_extract block=true defs=true instruction=false
```

### 第二节：Prompt 组装概要

**文件**: `src/openai_adapter/request/prompt.rs`

| 位置 | 级别 | 内容 |
|------|------|------|
| `build()` 返回后 | `debug` | prompt 总字符数、合并后消息轮次数、工具注入标识 |
| `build()` 返回后 | `info` (仅 `len > 10000`) | 上下文膨胀预警 |

格式示例:
```
[tc] prompt_build messages=12 len=15234 tool_inject=true sys_reminder=1024 think=true
[tc] prompt_build len=15234 (>10000, ctx inflation warning)
```

### 第三节：原始输出缓冲区 & 解析过程

**文件**: `src/openai_adapter/response/tool_parser.rs`

这是追踪的核心断面。所有样本截断至 500 字符。

#### 状态机事件 (`trace`)

| 事件 | 内容 |
|------|------|
| Detecting → 找到标签 | 标签名、在 buffer 中的位置、buffer 长度、标签附近 300 字符样本 |
| Detecting → CollectingXml | 状态迁移 |
| CollectingXml → 找到结束标签 | 标签名、buffer 长度 |
| CollectingXml → 周期性报告 | 每累计 10KB 或每 2 秒报告一次 buffer_len |
| 任意状态 → Done | 触发原因 |

格式示例:
```
[tc] detect→collect tag="<|tool_calls_begin|>" pos=1200 buf_len=1500
[tc] collect→done tag="</tool_calls_begin|>" len=45000
```

#### 解析失败 (`warn!` — 工具调用丢失是服务质量事件)

| 场景 | 内容 | 样本 |
|------|------|------|
| 滑动窗口内未找到标签 | buffer 末尾 500 字符 | `sample="...最近 500 字符..."` |
| XML 缓冲超限(>64KB) | buffer 长度 | `buf_overflow len=65536` |
| 流结束但标签未闭合 | buffer 末尾 500 字符 | `stream_end_unclosed len=45000` |
| JSON 解析/修复全失败 | 原始 JSON 前 500 字符 | `repair_all_failed sample="{\"name\"..."` |
| 回退纯文本 | 原因+样本 | `fallback reason=parse_fail sample=...` |

格式示例:
```
[tc] parse→buffer_no_tag len=1500 sample="...模型输出的最近 500 字符..."
[tc] parse→buf_overflow len=65536  ← warn!
[tc] parse→stream_end_unclosed len=45000 sample_end="...末尾 500 字符..."  ← warn!
[tc] fallback reason=parse_fail sample="..."  ← warn!
[tc] fallback reason=buf_overflow len=65536  ← warn!
```

#### JSON 修复过程 (`trace`)

每次修复尝试记录修复名+len+结果。成功时记录哪步成功。全失败时记录前 150 字符样本。

```
[tc] json_repair→try step=control_chars len=32000
[tc] json_repair→try step=unicode_quotes
[tc] json_repair→try step=backslash
[tc] json_repair→try step=single_quotes result=parse_ok  ← 成功了
[tc] json_repair→all_failed sample="{\"name\":...150 chars..."
```

#### 最终结果

| 结果 | 级别 | 内容 |
|------|------|------|
| 解析成功 | `debug` | 工具数量、名称列表、finish_reason |
| 回退纯文本 | `warn` | 原因+样本 |
| 缓冲超限回退 | `warn` | buffer 长度 |

```
[tc] result→ok n=2 names=[get_weather, search] finish=tool_calls
[tc] result→fallback reason=parse_fail
[tc] result→fallback reason=buf_overflow
```

## 日志级别策略

| 级别 | 用途 | 预期量 |
|------|------|--------|
| `info` | prompt 膨胀预警 (仅 >10KB 时) | 极少 |
| `warn` | 工具调用丢失 (fallback/buf_overflow) | 每次故障一次 |
| `debug` | 请求侧概览、最终结果 | 每次请求 2-3 条 |
| `trace` | 状态机事件、JSON 修复过程 | 持续输出，调试时启用 |

## 使用方式

```bash
# 查看工具调用正常/失败
RUST_LOG=adapter=debug just serve
# 深挖解析过程
RUST_LOG=adapter=trace just serve
# 只看失败事件
RUST_LOG=adapter=warn just serve
```

## 数据安全

- 所有样本截断至最多 500 字符
- 仅记录工具名和参数结构，不记录用户消息正文
- JSON 修复样本截断至 150 字符
