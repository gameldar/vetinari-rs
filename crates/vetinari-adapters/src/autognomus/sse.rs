//! Server-sent events: the framing of `GET /v1/runs/events`.
//!
//! Only the part of the format the gateway uses: `event` and `data` fields,
//! comments, and a blank line ending each message. `id` and `retry` are
//! read past, since resuming is done with `seq` and a refetch, not with
//! `Last-Event-ID`.

/// One message: its event name and its data lines joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseMessage {
    /// The `event` field, or `message` when there was none.
    pub event: String,
    /// The `data` fields, joined with newlines.
    pub data: String,
}

/// Turns chunks of a stream into messages, however the chunks fall.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    /// Feed a chunk, and take every message it completes.
    ///
    /// A message with an `event` but no data is still a message, unlike the
    /// browser's rule, because `resync` says everything in its name.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseMessage> {
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..end]);
            let line = line.strip_suffix('\r').unwrap_or(&line);
            if line.is_empty() {
                messages.extend(self.dispatch());
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.event = Some(value.to_owned()),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        messages
    }

    fn dispatch(&mut self) -> Option<SseMessage> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        let event = self.event.take().unwrap_or_else(|| "message".to_owned());
        let data = std::mem::take(&mut self.data).join("\n");
        Some(SseMessage { event, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(event: &str, data: &str) -> SseMessage {
        SseMessage {
            event: event.to_owned(),
            data: data.to_owned(),
        }
    }

    #[test]
    fn a_blank_line_ends_a_message() {
        let mut decoder = SseDecoder::default();
        assert_eq!(
            decoder.feed(b"event: node.ready\ndata: {\"seq\":1}\n\n"),
            [message("node.ready", "{\"seq\":1}")]
        );
    }

    #[test]
    fn a_message_split_across_chunks_is_put_back_together() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.feed(b"event: node.re").is_empty());
        assert!(decoder.feed(b"ady\nda").is_empty());
        assert!(decoder.feed(b"ta: {}\n").is_empty());
        assert_eq!(decoder.feed(b"\n"), [message("node.ready", "{}")]);
    }

    #[test]
    fn data_lines_are_joined_with_newlines() {
        let mut decoder = SseDecoder::default();
        assert_eq!(
            decoder.feed(b"data: one\ndata: two\n\n"),
            [message("message", "one\ntwo")]
        );
    }

    #[test]
    fn comments_ids_and_retries_are_read_past() {
        let mut decoder = SseDecoder::default();
        assert_eq!(
            decoder.feed(b": keep-alive\n\nid: 7\nretry: 1000\nevent: resync\ndata:\n\n"),
            [message("resync", "")]
        );
    }

    #[test]
    fn crlf_line_endings_and_a_missing_space_are_accepted() {
        let mut decoder = SseDecoder::default();
        assert_eq!(
            decoder.feed(b"event:completed\r\ndata:{}\r\n\r\n"),
            [message("completed", "{}")]
        );
    }

    #[test]
    fn several_messages_in_one_chunk_come_out_in_order() {
        let mut decoder = SseDecoder::default();
        let messages = decoder.feed(b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\nevent: c\n");
        assert_eq!(messages, [message("a", "1"), message("b", "2")]);
        assert_eq!(decoder.feed(b"data: 3\n\n"), [message("c", "3")]);
    }

    #[test]
    fn a_blank_line_with_nothing_before_it_is_no_message() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.feed(b"\n\n: ping\n\n").is_empty());
    }
}
