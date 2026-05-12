//! OpenAI Chunk 生成器 —— 将 DsFrame 映射为 ChatCompletionsResponseChunk

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use pin_project_lite::pin_project;

use log::{trace, warn};

use crate::openai_adapter::OpenAIAdapterError;
use crate::openai_adapter::types::{ChatCompletionsResponseChunk, ChunkChoice, Delta, Usage};

use super::now_secs;
use super::state::DsFrame;

fn make_usage_chunk(usage: Usage, model: &str, id: String) -> ChatCompletionsResponseChunk {
    ChatCompletionsResponseChunk {
        id,
        object: "chat.completion.chunk",
        created: now_secs(),
        model: model.to_string(),
        choices: vec![],
        usage: Some(usage),
        service_tier: None,
        system_fingerprint: None,
    }
}

fn make_usage(prompt_tokens: u32, completion_tokens: u32) -> Usage {
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
        prompt_tokens_details: None,
        completion_tokens_details: None,
    }
}

pub(crate) fn make_chunk(
    model: &str,
    delta: Delta,
    finish: Option<&'static str>,
    id: String,
) -> ChatCompletionsResponseChunk {
    ChatCompletionsResponseChunk {
        id,
        object: "chat.completion.chunk",
        created: now_secs(),
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason: finish,
            logprobs: None,
        }],
        usage: None,
        service_tier: None,
        system_fingerprint: None,
    }
}

const HALLUCINATION_MARKERS: &[&str] = &[
    "**Tool Call:",
    "Status: Completed",
    "Web search results for query:",
    "Terminal:\n```",
];

fn contains_hallucination(text: &str) -> bool {
    HALLUCINATION_MARKERS.iter().any(|m| text.contains(m))
}

fn contains_safe_marker(text: &str) -> bool {
    text.contains("<|tool_calls") || text.contains("<|tool\u{2581}calls")
}

pin_project! {
    /// 将 DsFrame 增量帧映射为 OpenAI ChatCompletionsResponseChunk 的流转换器
    pub struct ConverterStream<S> {
        #[pin]
        inner: S,
        model: String,
        include_usage: bool,
        include_obfuscation: bool,
        prompt_tokens: u32,
        chatcmpl_id: String,
        finished: bool,
        usage_value: Option<u32>,
        fallback_content: Option<String>,
        has_content: bool,
        // 抑制内容：检测到 **Tool Call:** 等幻觉文本后开始，<|tool_calls 真实标签出现后恢复
        suppress_content: bool,
        // 累计输出字符数，供 DeepSeek 未返回 accumulated_token_usage 时估算 output_tokens
        total_output_chars: usize,
    }
}

impl<S> ConverterStream<S> {
    /// 创建 Chunk 转换流
    pub fn new(
        inner: S,
        model: String,
        include_usage: bool,
        include_obfuscation: bool,
        prompt_tokens: u32,
        chatcmpl_id: String,
    ) -> Self {
        Self {
            inner,
            model,
            include_usage,
            include_obfuscation,
            prompt_tokens,
            chatcmpl_id,
            finished: false,
            usage_value: None,
            fallback_content: None,
            has_content: false,
            suppress_content: false,
            total_output_chars: 0,
        }
    }
}

impl<S> Stream for ConverterStream<S>
where
    S: Stream<Item = Result<DsFrame, OpenAIAdapterError>>,
{
    type Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // 如果已结束且有待发 usage，优先发送
        if *this.finished
            && *this.include_usage
            && let Some(u) = this.usage_value.take()
        {
            return Poll::Ready(Some(Ok(make_usage_chunk(
                make_usage(*this.prompt_tokens, u),
                this.model,
                this.chatcmpl_id.clone(),
            ))));
        }

        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame {
                    DsFrame::Role => {
                        trace!(target: "adapter", ">>> conv: role=assistant");
                        // 第一个 chunk 带上 prompt_tokens，供下游（如 AnthropicStream）提前获取
                        let mut chunk = make_chunk(
                            this.model,
                            Delta {
                                role: Some("assistant"),
                                ..Default::default()
                            },
                            None,
                            this.chatcmpl_id.clone(),
                        );
                        chunk.usage = Some(make_usage(*this.prompt_tokens, 0));
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    DsFrame::ThinkDelta(text) => {
                        trace!(target: "adapter", ">>> conv: thinking len={}", text.len());
                        *this.total_output_chars += text.len();
                        *this.fallback_content =
                            Some(this.fallback_content.take().unwrap_or_default() + &text);
                        return Poll::Ready(Some(Ok(make_chunk(
                            this.model,
                            Delta {
                                reasoning_content: Some(text),
                                ..Default::default()
                            },
                            None,
                            this.chatcmpl_id.clone(),
                        ))));
                    }
                    DsFrame::SearchResults(results) => {
                        // 将搜索结果格式化为 Markdown 注入 reasoning_content
                        let mut md = String::from("🔍 **搜索结果:**\n\n");
                        for (i, res) in results.iter().enumerate() {
                            md.push_str(&format!("{}. [{}]({})\n", i + 1, res.title, res.url));
                        }
                        md.push('\n');
                        trace!(target: "adapter", ">>> conv: search results len={}", results.len());
                        return Poll::Ready(Some(Ok(make_chunk(
                            this.model,
                            Delta {
                                reasoning_content: Some(md),
                                ..Default::default()
                            },
                            None,
                            this.chatcmpl_id.clone(),
                        ))));
                    }
                    DsFrame::ContentDelta(text) => {
                        trace!(target: "adapter", ">>> conv: content delta len={}", text.len());
                        *this.total_output_chars += text.len();
                        // 幻觉文本过滤器：检测 Claude Code UI 显示格式（**Tool Call:** 等），
                        // 进入抑制模式直到真实工具调用标签出现
                        if *this.suppress_content {
                            if contains_safe_marker(&text) {
                                *this.suppress_content = false;
                                warn!(target: "adapter", ">>> conv: 幻觉抑制结束，检测到真实工具调用标签，放行");
                            } else {
                                trace!(target: "adapter", ">>> conv: 抑制中，丢弃 hallucination content");
                                continue;
                            }
                        } else if contains_hallucination(&text) && !contains_safe_marker(&text) {
                            *this.suppress_content = true;
                            warn!(target: "adapter", ">>> conv: 检测到幻觉文本，开始抑制 content（**Tool Call:** 等）");
                            continue;
                        }
                        *this.has_content = true;
                        return Poll::Ready(Some(Ok(make_chunk(
                            this.model,
                            Delta {
                                content: Some(text),
                                ..Default::default()
                            },
                            None,
                            this.chatcmpl_id.clone(),
                        ))));
                    }
                    DsFrame::Status(status) if status == "FINISHED" && !*this.finished => {
                        trace!(target: "adapter", ">>> conv: finish=stop");
                        *this.finished = true;
                        // 若只有 thinking 无 RESPONSE content，回退输出 thinking 作为 content
                        if !*this.has_content
                            && let Some(fb) = this.fallback_content.take()
                        {
                            let mut chunk = make_chunk(
                                this.model,
                                Delta {
                                    content: Some(fb),
                                    ..Default::default()
                                },
                                Some("stop"),
                                this.chatcmpl_id.clone(),
                            );
                            if *this.include_usage
                                && let Some(u) = this.usage_value.take()
                                && u > 0
                            {
                                chunk.usage = Some(make_usage(*this.prompt_tokens, u));
                            } else if *this.include_usage {
                                let est = (*this.total_output_chars as u32 / 3).max(1);
                                chunk.usage = Some(make_usage(*this.prompt_tokens, est));
                            }
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        let mut chunk = make_chunk(
                            this.model,
                            Delta::default(),
                            Some("stop"),
                            this.chatcmpl_id.clone(),
                        );
                        if *this.include_usage
                            && let Some(u) = this.usage_value.take()
                            && u > 0
                        {
                            chunk.usage = Some(make_usage(*this.prompt_tokens, u));
                        } else if *this.include_usage {
                            let est = (*this.total_output_chars as u32 / 3).max(1);
                            chunk.usage = Some(make_usage(*this.prompt_tokens, est));
                        }
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    DsFrame::Status(_) => {}
                    DsFrame::Usage(u) => {
                        trace!(target: "adapter", ">>> conv: usage={}", u);
                        *this.usage_value = Some(u);
                        // emit 独立 usage chunk（仅当 u>0，避免覆盖下游已有的非零值）
                        if *this.include_usage && u > 0 {
                            return Poll::Ready(Some(Ok(make_usage_chunk(
                                make_usage(*this.prompt_tokens, u),
                                this.model,
                                this.chatcmpl_id.clone(),
                            ))));
                        }
                    }
                    DsFrame::Finish if !*this.finished => {
                        trace!(target: "adapter", ">>> conv: finish=stop");
                        *this.finished = true;
                        let mut chunk = make_chunk(
                            this.model,
                            Delta::default(),
                            Some("stop"),
                            this.chatcmpl_id.clone(),
                        );
                        if *this.include_usage
                            && let Some(u) = this.usage_value.take()
                            && u > 0
                        {
                            chunk.usage = Some(make_usage(*this.prompt_tokens, u));
                        } else if *this.include_usage {
                            let est = (*this.total_output_chars as u32 / 3).max(1);
                            chunk.usage = Some(make_usage(*this.prompt_tokens, est));
                        }
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    DsFrame::Finish => {}
                },
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    if !*this.finished {
                        warn!(target: "adapter", "转换器流提前结束: model={}, usage_value={:?}", this.model, this.usage_value);
                        // 防御性 emit finish chunk，确保下游能正确关闭
                        *this.finished = true;
                        let mut chunk = make_chunk(
                            this.model,
                            Delta::default(),
                            Some("stop"),
                            this.chatcmpl_id.clone(),
                        );
                        if *this.include_usage
                            && let Some(u) = this.usage_value.take()
                            && u > 0
                        {
                            chunk.usage = Some(make_usage(*this.prompt_tokens, u));
                        } else if *this.include_usage {
                            let est = (*this.total_output_chars as u32 / 3).max(1);
                            chunk.usage = Some(make_usage(*this.prompt_tokens, est));
                        }
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    if *this.finished
                        && *this.include_usage
                        && let Some(u) = this.usage_value.take()
                        && u > 0
                    {
                        return Poll::Ready(Some(Ok(make_usage_chunk(
                            make_usage(*this.prompt_tokens, u),
                            this.model,
                            this.chatcmpl_id.clone(),
                        ))));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
