//! Bounded framing for the SSE dialect emitted by the finance option-flow API.

use super::MAX_FRAME_BYTES;
use axum::body::Bytes;
use std::ops::Range;

#[derive(Debug)]
pub(super) struct InvalidFrame;

pub(super) struct SseReader {
    response: reqwest::Response,
    chunk: Bytes,
    decoder: Decoder,
}

impl SseReader {
    pub(super) fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            chunk: Bytes::new(),
            decoder: Decoder::default(),
        }
    }

    pub(super) async fn next(&mut self) -> Result<Option<Frame>, InvalidFrame> {
        loop {
            if let Some(frame) = self.decoder.feed(&mut self.chunk)? {
                return Ok(Some(frame));
            }
            match self.response.chunk().await.map_err(|_| InvalidFrame)? {
                Some(chunk) => self.chunk = chunk,
                None if self.decoder.buffer.is_empty() => return Ok(None),
                None => return Err(InvalidFrame),
            }
        }
    }
}

#[derive(Default)]
struct Decoder {
    buffer: Vec<u8>,
    line_start: usize,
}

impl Decoder {
    fn feed(&mut self, input: &mut Bytes) -> Result<Option<Frame>, InvalidFrame> {
        while !input.is_empty() {
            let newline = input.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(input.len(), |position| position + 1);
            if take > MAX_FRAME_BYTES - self.buffer.len() {
                return Err(InvalidFrame);
            }
            self.buffer.extend_from_slice(&input.split_to(take));
            if newline.is_some() {
                let blank = matches!(&self.buffer[self.line_start..], b"\n" | b"\r\n");
                if blank {
                    self.line_start = 0;
                    return Frame::parse(std::mem::take(&mut self.buffer)).map(Some);
                }
                self.line_start = self.buffer.len();
            }
        }
        Ok(None)
    }
}

pub(super) struct Frame {
    wire: String,
    fields: Option<Fields>,
}

struct Fields {
    event: Range<usize>,
    id: Option<Range<usize>>,
    data: Range<usize>,
}

pub(super) struct Dispatch<'a> {
    pub(super) event: &'a str,
    pub(super) id: Option<&'a str>,
    pub(super) data: &'a str,
}

impl Frame {
    fn parse(wire: Vec<u8>) -> Result<Self, InvalidFrame> {
        let wire = String::from_utf8(wire).map_err(|_| InvalidFrame)?;
        let (mut event, mut id, mut data) = (None, None, None);
        let mut has_comment = false;
        let mut offset = 0;
        for raw_line in wire.split_terminator('\n') {
            let start = offset;
            offset += raw_line.len() + 1;
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            if line.is_empty() {
                break;
            }
            // A bare CR also ends a line in a browser. Reject it and every
            // other ASCII control before forwarding any part of the frame.
            if line.bytes().any(|byte| byte.is_ascii_control()) {
                return Err(InvalidFrame);
            }
            let (name, value) = line.split_once(':').ok_or(InvalidFrame)?;
            let field = match name {
                "" => {
                    has_comment = true;
                    continue;
                }
                "event" => &mut event,
                "id" => &mut id,
                "data" => &mut data,
                _ => return Err(InvalidFrame),
            };
            let value = value.strip_prefix(' ').unwrap_or(value);
            if value.is_empty() || field.is_some() {
                return Err(InvalidFrame);
            }
            let end = start + line.len();
            *field = Some(end - value.len()..end);
        }
        let fields = match (event, id, data) {
            (Some(event), id, Some(data)) => Some(Fields { event, id, data }),
            (None, None, None) if has_comment => None,
            _ => return Err(InvalidFrame),
        };
        Ok(Self { wire, fields })
    }

    pub(super) fn dispatch(&self) -> Option<Dispatch<'_>> {
        self.fields.as_ref().map(|fields| Dispatch {
            event: &self.wire[fields.event.clone()],
            id: fields.id.as_ref().map(|id| &self.wire[id.clone()]),
            data: &self.wire[fields.data.clone()],
        })
    }

    pub(super) fn into_bytes(self) -> Bytes {
        self.wire.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_sse_decodes_every_chunk_boundary_without_changing_wire() {
        let wire = "event: observation\r\ndata: {\"note\":\"合成\",\"seq\":\"18446744073709551615\"}\r\nid: synthetic:1\r\n\r\n";
        for boundary in 0..wire.len() {
            let mut decoder = Decoder::default();
            let mut first = Bytes::copy_from_slice(&wire.as_bytes()[..boundary]);
            let mut second = Bytes::copy_from_slice(&wire.as_bytes()[boundary..]);
            assert!(decoder.feed(&mut first).unwrap().is_none());
            let frame = decoder.feed(&mut second).unwrap().unwrap();
            let dispatch = frame.dispatch().unwrap();
            assert_eq!(dispatch.event, "observation");
            assert_eq!(dispatch.id, Some("synthetic:1"));
            assert_eq!(
                dispatch.data,
                r#"{"note":"合成","seq":"18446744073709551615"}"#
            );
            assert_eq!(frame.into_bytes(), wire.as_bytes());
        }
    }

    #[test]
    fn f16_sse_keeps_consecutive_frames_and_keepalive_separate() {
        let mut decoder = Decoder::default();
        let mut input = Bytes::from_static(b":\n\nevent: status\nid: synthetic\ndata: {}\n\n");
        let first = decoder.feed(&mut input).unwrap().unwrap();
        assert!(first.dispatch().is_none());
        assert_eq!(first.into_bytes(), b":\n\n".as_slice());
        let second = decoder.feed(&mut input).unwrap().unwrap();
        let dispatch = second.dispatch().unwrap();
        assert_eq!(dispatch.event, "status");
        assert_eq!(dispatch.id, Some("synthetic"));
        assert_eq!(dispatch.data, "{}");
        assert!(input.is_empty());
        assert!(decoder.buffer.is_empty());
    }

    #[test]
    fn f16_sse_rejects_ambiguous_and_oversized_frames() {
        for invalid in [
            b"event: status\nevent: observation\ndata: {}\n\n".as_slice(),
            b"event: status\nid: a\nid: b\ndata: {}\n\n",
            b"event: status\ndata: {}\ndata: {}\n\n",
            b"event: status\ndata: {}\nretry: 0\n\n",
            b"event: status\ndata: {}\r\rid: other\n\n",
            b": keepalive\rid: hidden\n\n",
            b": keepalive\nid: hidden\n\n",
            b"id: without-dispatch\n\n",
            b"\xef\xbb\xbfevent: status\ndata: {}\n\n",
            b"event: status\ndata: \xff\n\n",
            b"event: status\nid: two\0parts\ndata: {}\n\n",
        ] {
            let mut input = Bytes::from_static(invalid);
            assert!(Decoder::default().feed(&mut input).is_err());
        }

        let mut decoder = Decoder::default();
        let mut input = Bytes::from(vec![b'a'; MAX_FRAME_BYTES]);
        assert!(decoder.feed(&mut input).unwrap().is_none());
        assert!(decoder.feed(&mut Bytes::from_static(b"\n\n")).is_err());
    }
}
