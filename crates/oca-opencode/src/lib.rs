// Progenitor emits these lints in the pinned generated source. The facade is handwritten and
// checked normally; generated output is not edited in place.
#![allow(
    clippy::default_trait_access,
    clippy::doc_markdown,
    clippy::unnecessary_trailing_comma,
    clippy::unused_self
)]

use std::{fmt, future::Future};

mod facade;
mod follow;
#[allow(clippy::all, dead_code, renamed_and_removed_lints)]
mod generated;

pub use facade::{
    AbortAccepted, ControlAccepted, CreateSessionRequest, MessageId, MessageWithParts,
    OpenCodeClient, OpenCodeError, PromptAccepted, PromptRequest, Session, SessionId, Subscription,
    TextPart,
};
pub use follow::{adapt_message, adapt_sse_event};

/// A parsed server-sent event frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

/// Errors produced while reading or framing an SSE response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseError {
    /// A complete SSE line was not valid UTF-8.
    InvalidUtf8 { line: Vec<u8> },
    /// The underlying response body could not provide its next chunk.
    Source { message: String },
}

impl fmt::Display for SseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 { .. } => formatter.write_str("SSE line is not valid UTF-8"),
            Self::Source { message } => write!(formatter, "SSE source failed: {message}"),
        }
    }
}

impl std::error::Error for SseError {}

/// The injected response-body seam for [`SseStream`].
pub trait SseChunkSource {
    type Error: fmt::Display;

    fn next_chunk(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send;
}

/// An isolated parser for a raw SSE response body.
pub struct SseStream<S> {
    source: S,
    pending: Vec<u8>,
    id: Option<String>,
    event: Option<String>,
    data_lines: Vec<String>,
    finished: bool,
}

impl<S> SseStream<S>
where
    S: SseChunkSource,
{
    pub fn new(source: S) -> Self {
        Self {
            source,
            pending: Vec::new(),
            id: None,
            event: None,
            data_lines: Vec::new(),
            finished: false,
        }
    }

    /// Returns the next complete event, or `None` after the response body ends.
    ///
    /// # Errors
    ///
    /// Returns [`SseError`] when the source fails or a frame contains invalid UTF-8.
    pub async fn next(&mut self) -> Result<Option<SseEvent>, SseError> {
        loop {
            if let Some(line) = self.take_complete_line() {
                if let Some(event) = self.process_line(line)? {
                    return Ok(Some(event));
                }
                continue;
            }

            if self.finished {
                if !self.pending.is_empty() {
                    let line = std::mem::take(&mut self.pending);
                    if let Some(event) = self.process_line(line)? {
                        return Ok(Some(event));
                    }
                    continue;
                }
                return Ok(self.dispatch());
            }

            match self.source.next_chunk().await {
                Ok(Some(chunk)) => self.pending.extend(chunk),
                Ok(None) => self.finished = true,
                Err(error) => {
                    return Err(SseError::Source {
                        message: error.to_string(),
                    });
                }
            }
        }
    }

    fn take_complete_line(&mut self) -> Option<Vec<u8>> {
        let newline = self.pending.iter().position(|byte| *byte == b'\n')?;
        let mut line: Vec<_> = self.pending.drain(..=newline).collect();
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Some(line)
    }

    fn process_line(&mut self, line: Vec<u8>) -> Result<Option<SseEvent>, SseError> {
        let Ok(line) = std::str::from_utf8(&line) else {
            return Err(SseError::InvalidUtf8 { line });
        };

        if line.is_empty() {
            return Ok(self.dispatch());
        }
        if line.starts_with(':') {
            return Ok(None);
        }

        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);

        match field {
            "data" => self.data_lines.push(value.to_owned()),
            "event" => self.event = Some(value.to_owned()),
            "id" if !value.contains('\0') => self.id = Some(value.to_owned()),
            _ => {}
        }
        Ok(None)
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = (!self.data_lines.is_empty()).then(|| SseEvent {
            id: self.id.clone(),
            event: self.event.take(),
            data: self.data_lines.join("\n"),
        });
        self.event = None;
        self.data_lines.clear();
        event
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::Future,
        pin::Pin,
        task::{Context, Poll, Waker},
    };

    use super::{SseChunkSource, SseStream};

    struct Chunks {
        chunks: VecDeque<Vec<u8>>,
    }

    impl SseChunkSource for Chunks {
        type Error = std::convert::Infallible;

        fn next_chunk(
            &mut self,
        ) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send {
            std::future::ready(Ok(self.chunks.pop_front()))
        }
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        let mut context = Context::from_waker(Waker::noop());
        let mut future = std::pin::pin!(future);

        loop {
            match Pin::as_mut(&mut future).poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn parses_event_ids() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([b"id: 42\nevent: session.idle\ndata: finished\n\n".to_vec()]),
        });

        let event = block_on(stream.next()).expect("valid SSE frame");

        assert_eq!(
            event,
            Some(super::SseEvent {
                id: Some("42".to_owned()),
                event: Some("session.idle".to_owned()),
                data: "finished".to_owned(),
            })
        );
    }

    #[test]
    fn retains_the_last_event_id_until_a_later_id_replaces_it() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([
                b"id: old\nevent: custom\ndata: first\n\ndata: second\n\nid: new\ndata: third\n\n"
                    .to_vec(),
            ]),
        });

        let first = block_on(stream.next()).unwrap().unwrap();
        assert_eq!(first.id.as_deref(), Some("old"));
        assert_eq!(first.event.as_deref(), Some("custom"));
        assert_eq!(first.data, "first");

        let second = block_on(stream.next()).unwrap().unwrap();
        assert_eq!(second.id.as_deref(), Some("old"));
        assert_eq!(second.event, None);
        assert_eq!(second.data, "second");

        let third = block_on(stream.next()).unwrap().unwrap();
        assert_eq!(third.id.as_deref(), Some("new"));
        assert_eq!(third.event, None);
        assert_eq!(third.data, "third");
    }

    #[test]
    fn dispatches_events_at_blank_lines() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([b"data: first\n\ndata: second\n\n".to_vec()]),
        });

        assert_eq!(block_on(stream.next()).unwrap().unwrap().data, "first");
        assert_eq!(block_on(stream.next()).unwrap().unwrap().data, "second");
    }

    #[test]
    fn ignores_comment_lines() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([b": keepalive\n: another comment\ndata: ready\n\n".to_vec()]),
        });

        assert_eq!(block_on(stream.next()).unwrap().unwrap().data, "ready");
    }

    #[test]
    fn ignores_retry_hints() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([b"retry: 5000\ndata: ready\n\n".to_vec()]),
        });

        assert_eq!(block_on(stream.next()).unwrap().unwrap().data, "ready");
    }

    #[test]
    fn joins_multiple_data_lines() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([b"data: first\ndata: second\ndata:\n\n".to_vec()]),
        });

        assert_eq!(
            block_on(stream.next()).unwrap().unwrap().data,
            "first\nsecond\n"
        );
    }

    #[test]
    fn accepts_crlf_terminators() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([b"event: update\r\ndata: ready\r\n\r\n".to_vec()]),
        });

        let event = block_on(stream.next()).unwrap().unwrap();
        assert_eq!(event.event.as_deref(), Some("update"));
        assert_eq!(event.data, "ready");
    }

    #[test]
    fn accepts_lf_terminators() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([b"event: update\ndata: ready\n\n".to_vec()]),
        });

        let event = block_on(stream.next()).unwrap().unwrap();
        assert_eq!(event.event.as_deref(), Some("update"));
        assert_eq!(event.data, "ready");
    }

    #[test]
    fn accepts_a_frame_split_across_three_chunks() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([
                b"id: 7\neve".to_vec(),
                b"nt: notice\nda".to_vec(),
                b"ta: done\n\n".to_vec(),
            ]),
        });

        let event = block_on(stream.next()).unwrap().unwrap();
        assert_eq!(event.id.as_deref(), Some("7"));
        assert_eq!(event.event.as_deref(), Some("notice"));
        assert_eq!(event.data, "done");
    }

    #[test]
    fn accepts_a_frame_split_mid_utf8_sequence() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([b"data: caf\xc3".to_vec(), b"\xa9\n\n".to_vec()]),
        });

        assert_eq!(block_on(stream.next()).unwrap().unwrap().data, "café");
    }

    #[test]
    fn dispatches_an_event_at_eof_without_a_trailing_blank_line() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([b"id: 8\ndata: completed".to_vec()]),
        });

        let event = block_on(stream.next()).unwrap().unwrap();
        assert_eq!(event.id.as_deref(), Some("8"));
        assert_eq!(event.data, "completed");
        assert_eq!(block_on(stream.next()).unwrap(), None);
    }

    #[test]
    fn preserves_duplicate_terminal_events_without_parser_duplication() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([
                b"event: session.idle\ndata: terminal\n\nevent: session.idle\ndata: terminal\n\n"
                    .to_vec(),
            ]),
        });

        assert_eq!(block_on(stream.next()).unwrap().unwrap().data, "terminal");
        assert_eq!(block_on(stream.next()).unwrap().unwrap().data, "terminal");
        assert_eq!(block_on(stream.next()).unwrap(), None);
    }

    #[derive(Debug)]
    struct Disconnect;

    impl std::fmt::Display for Disconnect {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("connection reset")
        }
    }

    struct Disconnected;

    impl SseChunkSource for Disconnected {
        type Error = Disconnect;

        fn next_chunk(
            &mut self,
        ) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send {
            std::future::ready(Err(Disconnect))
        }
    }

    #[test]
    fn reports_a_mid_stream_disconnect_as_a_typed_error() {
        let mut stream = SseStream::new(Disconnected);

        assert_eq!(
            block_on(stream.next()),
            Err(super::SseError::Source {
                message: "connection reset".to_owned(),
            })
        );
    }

    #[test]
    fn preserves_interleaved_foreign_session_events() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([
                b"data: {\"sessionID\":\"target\"}\n\ndata: {\"sessionID\":\"foreign\"}\n\n"
                    .to_vec(),
            ]),
        });

        assert_eq!(
            block_on(stream.next()).unwrap().unwrap().data,
            "{\"sessionID\":\"target\"}"
        );
        assert_eq!(
            block_on(stream.next()).unwrap().unwrap().data,
            "{\"sessionID\":\"foreign\"}"
        );
    }

    #[test]
    fn reports_malformed_utf8_frames_as_typed_errors() {
        let mut stream = SseStream::new(Chunks {
            chunks: VecDeque::from([b"data: \xff\n\n".to_vec()]),
        });

        assert_eq!(
            block_on(stream.next()),
            Err(super::SseError::InvalidUtf8 {
                line: b"data: \xff".to_vec(),
            })
        );
    }
}
