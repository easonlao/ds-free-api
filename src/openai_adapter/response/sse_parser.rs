//! SSE 解析 —— 将 ds_core 字节流切分为独立 SSE 事件

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use pin_project_lite::pin_project;

use log::{trace, warn};

use crate::openai_adapter::OpenAIAdapterError;

/// 单个 SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

pin_project! {
    // 包装底层字节流，将其切分为独立的 SSE 事件
    pub struct SseStream<S> {
        #[pin]
        inner: S,
        text_buf: String,
        raw_buf: Vec<u8>,
    }
}

impl<S> SseStream<S> {
    /// 创建 SSE 流包装器
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            text_buf: String::new(),
            raw_buf: Vec::new(),
        }
    }
}

impl<S, E> Stream for SseStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: std::fmt::Display,
{
    type Item = Result<SseEvent, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    this.raw_buf.extend_from_slice(&bytes);
                    decode_utf8_prefix(this.raw_buf, this.text_buf);
                    if let Some(evt) = try_pop_event(this.text_buf) {
                        trace!(target: "adapter", "<<< {} {}", evt.event.as_deref().unwrap_or("-"), evt.data);
                        return Poll::Ready(Some(Ok(evt)));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    warn!(target: "adapter", "SSE 流错误: {}", e);
                    return Poll::Ready(Some(Err(OpenAIAdapterError::Internal(format!(
                        "SSE 流错误: {}",
                        e
                    )))));
                }
                Poll::Ready(None) => {
                    decode_utf8_prefix(this.raw_buf, this.text_buf);
                    if !this.raw_buf.is_empty() {
                        // 流结束时 raw_buf 中残留的是不完整的 UTF-8 尾字节，无法补全。
                        // 丢弃它们而不使用 from_utf8_lossy，避免向用户输出 `�` 乱码。
                        warn!(target: "adapter", "SSE 流结束时丢弃 {} 个不完整的 UTF-8 尾字节", this.raw_buf.len());
                        this.raw_buf.clear();
                    }
                    return if let Some(evt) = try_pop_event(this.text_buf) {
                        Poll::Ready(Some(Ok(evt)))
                    } else {
                        Poll::Ready(None)
                    };
                }
                Poll::Pending => {
                    decode_utf8_prefix(this.raw_buf, this.text_buf);
                    if let Some(evt) = try_pop_event(this.text_buf) {
                        return Poll::Ready(Some(Ok(evt)));
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}

/// 把 raw_buf 中完整的 UTF-8 前缀移动到 text_buf，残留不完整的字节留在 raw_buf
fn decode_utf8_prefix(raw: &mut Vec<u8>, text: &mut String) {
    if raw.is_empty() {
        return;
    }
    match std::str::from_utf8(raw) {
        Ok(s) => {
            text.push_str(s);
            raw.clear();
        }
        Err(e) => {
            let up_to = e.valid_up_to();
            if up_to > 0 {
                unsafe { text.push_str(std::str::from_utf8_unchecked(&raw[..up_to])) };
                raw.drain(..up_to);
            }
        }
    }
}

/// 从 buf 中提取第一个以 \n\n 分隔的 SSE 事件块
fn try_pop_event(buf: &mut String) -> Option<SseEvent> {
    let pos = buf.find("\n\n")?;
    let tail = buf.split_off(pos);
    let block = std::mem::take(buf);
    *buf = tail;
    if buf.starts_with("\n\n") {
        buf.drain(..2);
    }

    let mut event = None;
    let mut data = String::new();
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v.trim_start());
        }
    }
    Some(SseEvent { event, data })
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    #[tokio::test]
    async fn split_simple_events() {
        let input = Bytes::from("event: ready\ndata: {}\n\nevent: finish\ndata: {}\n\n");
        let stream = SseStream::new(futures::stream::iter(vec![Ok::<_, std::io::Error>(input)]));
        let events: Vec<_> = stream.map(|r| r.unwrap()).collect().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("ready"));
        assert_eq!(events[0].data, "{}");
        assert_eq!(events[1].event.as_deref(), Some("finish"));
    }

    #[tokio::test]
    async fn split_across_chunks() {
        let parts: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from("event: ready\ndata: {}")),
            Ok(Bytes::from("\n\nevent: finish\ndata: {}\n\n")),
        ];
        let stream = SseStream::new(futures::stream::iter(parts));
        let events: Vec<_> = stream.map(|r| r.unwrap()).collect().await;
        assert_eq!(events.len(), 2);
    }

    /// 测试流在多字节 UTF-8 字符中间结束时, 不会产生乱码 `�`
    #[tokio::test]
    async fn no_garbled_on_incomplete_utf8_at_eof() {
        // "你" 的 UTF-8 编码: 0xE4 0xBD 0xA0
        // 模拟数据: data: {"text": "你
        // 在 "你" 的第二个字节后截断, 即 [0xE4, 0xBD]
        let partial = vec![0xE4, 0xBD];
        let mut full = b"event: data\ndata: {".to_vec();
        full.extend_from_slice(&partial);
        full.extend_from_slice(b"}\n\n");

        let stream = SseStream::new(futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(full))]));
        let events: Vec<_> = stream.map(|r| r.unwrap()).collect().await;

        if let Some(event) = events.first() {
            eprintln!("Event data: {:?}", event.data);
            // 当前实现会调用 from_utf8_lossy 产生 �, 断言失败以暴露问题
            assert!(
                !event.data.contains('�'),
                "发现乱码字符 '�' in data: {:?}",
                event.data
            );
        }
    }

    /// 测试多字节 UTF-8 字符跨多个 chunk 传输后正常解码
    #[tokio::test]
    async fn multibyte_utf8_across_chunks() {
        // "你好" 的 UTF-8 编码: 0xE4 0xBD 0xA0 0xE5 0xA5 0xBD
        let full = "你好";
        let bytes = full.as_bytes();
        // 分块: 第一块包含前 2 字节, 第二块包含剩余 4 字节
        let chunk1 = bytes[..2].to_vec();  // [0xE4, 0xBD]
        let chunk2 = bytes[2..].to_vec();  // [0xA0, 0xE5, 0xA5, 0xBD]

        let mut sse1 = b"event: data\ndata: ".to_vec();
        sse1.extend_from_slice(&chunk1);
        let mut sse2 = chunk2;
        sse2.extend_from_slice(b"\n\n");

        let stream = SseStream::new(futures::stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from(sse1)),
            Ok(Bytes::from(sse2)),
        ]));
        let events: Vec<_> = stream.map(|r| r.unwrap()).collect().await;

        assert_eq!(events.len(), 1);
        assert!(events[0].data.contains("你好"));
        assert!(!events[0].data.contains('�'));
    }

    /// 测试混合内容 (ASCII + 中文) 在分块传输后正常解码
    #[tokio::test]
    async fn mixed_content_across_chunks() {
        let content = "Hello, 世界!";
        let bytes = content.as_bytes();
        // 在 ASCII 和中文的边界处分割
        let split_pos = 7; // "Hello, " (7 字节) 后是 "世界!"
        let chunk1 = bytes[..split_pos].to_vec();
        let chunk2 = bytes[split_pos..].to_vec();

        let mut sse1 = b"event: data\ndata: ".to_vec();
        sse1.extend_from_slice(&chunk1);
        let mut sse2 = chunk2;
        sse2.extend_from_slice(b"\n\n");

        let stream = SseStream::new(futures::stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from(sse1)),
            Ok(Bytes::from(sse2)),
        ]));
        let events: Vec<_> = stream.map(|r| r.unwrap()).collect().await;

        assert_eq!(events.len(), 1);
        assert!(events[0].data.contains("Hello, 世界!"));
        assert!(!events[0].data.contains('�'));
    }
}
