#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MjpegContentLength {
    WithinLimit(usize),
    Oversized(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BoundaryRead {
    Continue,
    HitBoundary,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MjpegHeader {
    End,
    ContentLength(MjpegContentLength),
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MjpegBodyProgress {
    Continue,
    DroppedOversized,
    HitBoundary,
    End,
}

pub(super) struct MjpegFrameParser<'a> {
    parser: MjpegMultipartParser<'a>,
    body: BoundaryBodyReader<'a>,
    line: Vec<u8>,
    buf: Vec<u8>,
    have_boundary: bool,
}

impl<'a> MjpegFrameParser<'a> {
    pub(super) fn new(boundary: &'a str, max_jpeg_bytes: usize, frame_capacity: usize) -> Self {
        Self {
            parser: MjpegMultipartParser::new(boundary, max_jpeg_bytes),
            body: BoundaryBodyReader::new(boundary.as_bytes()),
            line: Vec::with_capacity(1024),
            buf: Vec::with_capacity(frame_capacity.min(max_jpeg_bytes)),
            have_boundary: false,
        }
    }

    pub(super) fn needs_boundary_line(&self) -> bool {
        !self.have_boundary
    }

    pub(super) fn line_buffer(&mut self) -> &mut Vec<u8> {
        self.line.clear();
        &mut self.line
    }

    pub(super) fn accept_boundary_line(&self) -> bool {
        self.parser.is_boundary_line(&self.line)
    }

    pub(super) fn begin_part(&mut self) {
        self.have_boundary = false;
    }

    pub(super) fn header(&self) -> MjpegHeader {
        if self.line.iter().all(|b| b.is_ascii_whitespace()) {
            MjpegHeader::End
        } else if let Some(length) = self.parser.content_length(&self.line) {
            MjpegHeader::ContentLength(length)
        } else {
            MjpegHeader::Other
        }
    }

    pub(super) fn clear_frame(&mut self) {
        self.buf.clear();
        self.body = self.parser.body_reader();
    }

    pub(super) fn append_content_length_chunk(
        &mut self,
        data: &[u8],
        target: usize,
    ) -> Option<usize> {
        self.parser
            .append_content_length_chunk(data, target, &mut self.buf)
    }

    pub(super) fn append_boundary_chunk(
        &mut self,
        data: &[u8],
        stream: &'static str,
    ) -> (usize, MjpegBodyProgress) {
        let (take, outcome) = self.body.append_chunk(data, &mut self.buf);
        match outcome {
            BoundaryRead::Continue if self.buf.len() >= self.parser.max_jpeg_bytes => {
                crate::trace::warn!(
                    backend = "netcam",
                    stream,
                    max_bytes = self.parser.max_jpeg_bytes,
                    buffered_bytes = self.buf.len(),
                    parser_event = "oversized_boundary_frame",
                    "dropping oversized mjpeg frame"
                );
                self.buf.clear();
                (take, MjpegBodyProgress::DroppedOversized)
            }
            BoundaryRead::Continue => (take, MjpegBodyProgress::Continue),
            BoundaryRead::HitBoundary => {
                self.have_boundary = true;
                (take, MjpegBodyProgress::HitBoundary)
            }
            BoundaryRead::End => {
                if !self.buf.is_empty() {
                    crate::trace::warn!(
                        backend = "netcam",
                        stream,
                        buffered_bytes = self.buf.len(),
                        parser_event = "stream_ended_before_boundary",
                        "mjpeg stream ended before the next boundary"
                    );
                }
                (take, MjpegBodyProgress::End)
            }
        }
    }

    pub(super) fn frame_bytes(&self) -> &[u8] {
        &self.buf
    }
}

pub(super) struct BoundaryBodyReader<'a> {
    boundary: &'a [u8],
    pending: Vec<u8>,
}

impl<'a> BoundaryBodyReader<'a> {
    fn new(boundary: &'a [u8]) -> Self {
        Self {
            boundary,
            pending: Vec::with_capacity(boundary.len().saturating_sub(1)),
        }
    }

    pub(super) fn append_chunk(&mut self, data: &[u8], buf: &mut Vec<u8>) -> (usize, BoundaryRead) {
        if data.is_empty() {
            return if self.pending.is_empty() {
                (0, BoundaryRead::End)
            } else {
                crate::trace::warn!(
                    backend = "netcam",
                    stream = "mjpeg",
                    parser_event = "stream_ended_with_pending_boundary_bytes",
                    pending_bytes = self.pending.len(),
                    frame_bytes = buf.len(),
                    "mjpeg stream ended while boundary prefix bytes were pending"
                );
                buf.extend_from_slice(&self.pending);
                self.pending.clear();
                (0, BoundaryRead::End)
            };
        }

        // The pending bytes and the chunk are searched and copied in place (each payload byte is
        // copied once, into `buf`); only the boundary's own bytes are carried over.
        let pending_len = self.pending.len();
        let total = pending_len + data.len();

        if let Some(boundary_idx) = find_boundary(&self.pending, data, self.boundary) {
            let after_boundary = boundary_idx.saturating_add(self.boundary.len());
            let incomplete = after_boundary >= total
                || (joined_at(&self.pending, data, after_boundary) == Some(b'\r')
                    && after_boundary + 1 >= total);
            append_joined(buf, &self.pending, data, boundary_idx);
            if incomplete {
                retain_from(&mut self.pending, data, boundary_idx);
                crate::trace::trace!(
                    backend = "netcam",
                    stream = "mjpeg",
                    parser_event = "boundary_line_pending",
                    boundary_len = self.boundary.len(),
                    pending_bytes = self.pending.len(),
                    frame_bytes = buf.len(),
                    "mjpeg boundary found before the full boundary line was buffered"
                );
                return (data.len(), BoundaryRead::Continue);
            }
            let consume =
                consumed_data_for_boundary(&self.pending, data, boundary_idx, self.boundary.len());
            self.pending.clear();
            crate::trace::trace!(
                backend = "netcam",
                stream = "mjpeg",
                parser_event = "boundary_detected",
                boundary_len = self.boundary.len(),
                consumed_bytes = consume,
                frame_bytes = buf.len(),
                carried_bytes = pending_len,
                "mjpeg boundary detected"
            );
            return (consume, BoundaryRead::HitBoundary);
        }

        let keep = self.boundary.len().saturating_sub(1).min(total);
        let emit_len = total - keep;
        append_joined(buf, &self.pending, data, emit_len);
        retain_from(&mut self.pending, data, emit_len);
        (data.len(), BoundaryRead::Continue)
    }
}

/// The index (in `pending` then `data`, as one run of bytes) of the first boundary: one that
/// starts in `pending` (it may run on into `data`), else one in `data`.
fn find_boundary(pending: &[u8], data: &[u8], boundary: &[u8]) -> Option<usize> {
    let p = pending.len();
    for start in 0..p {
        let head = (p - start).min(boundary.len());
        let rest = boundary.len() - head;
        if pending[start..start + head] == boundary[..head]
            && data.len() >= rest
            && data[..rest] == boundary[head..]
        {
            return Some(start);
        }
    }
    find_subslice(data, boundary).map(|i| p + i)
}

/// The byte at `index` of `pending` then `data`, as one run of bytes.
fn joined_at(pending: &[u8], data: &[u8], index: usize) -> Option<u8> {
    if index < pending.len() {
        pending.get(index).copied()
    } else {
        data.get(index - pending.len()).copied()
    }
}

/// Appends the first `end` bytes of `pending` then `data` to `buf`.
fn append_joined(buf: &mut Vec<u8>, pending: &[u8], data: &[u8], end: usize) {
    let head = end.min(pending.len());
    buf.extend_from_slice(&pending[..head]);
    buf.extend_from_slice(&data[..end - head]);
}

/// Replaces `pending` with the bytes of `pending` then `data` from `start` on.
fn retain_from(pending: &mut Vec<u8>, data: &[u8], start: usize) {
    let p = pending.len();
    if start < p {
        pending.drain(..start);
        pending.extend_from_slice(data);
    } else {
        pending.clear();
        pending.extend_from_slice(&data[start - p..]);
    }
}

pub(super) struct MjpegMultipartParser<'a> {
    boundary: &'a [u8],
    max_jpeg_bytes: usize,
}

impl<'a> MjpegMultipartParser<'a> {
    pub(super) fn new(boundary: &'a str, max_jpeg_bytes: usize) -> Self {
        Self {
            boundary: boundary.as_bytes(),
            max_jpeg_bytes,
        }
    }

    pub(super) fn is_boundary_line(&self, line: &[u8]) -> bool {
        line.starts_with(self.boundary)
    }

    pub(super) fn content_length(&self, line: &[u8]) -> Option<MjpegContentLength> {
        let rest = line
            .strip_prefix(b"Content-Length:")
            .or_else(|| line.strip_prefix(b"content-length:"))?;
        std::str::from_utf8(rest)
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .map(|value| {
                if value > self.max_jpeg_bytes {
                    MjpegContentLength::Oversized(value)
                } else {
                    MjpegContentLength::WithinLimit(value)
                }
            })
    }

    pub(super) fn body_reader(&self) -> BoundaryBodyReader<'a> {
        BoundaryBodyReader::new(self.boundary)
    }

    pub(super) fn append_content_length_chunk(
        &self,
        data: &[u8],
        target: usize,
        buf: &mut Vec<u8>,
    ) -> Option<usize> {
        if data.is_empty() || buf.len() >= target {
            return None;
        }
        let need = target - buf.len();
        let take = data.len().min(need);
        buf.extend_from_slice(&data[..take]);
        Some(take)
    }
}

impl MjpegContentLength {
    pub(super) fn into_len(self) -> usize {
        match self {
            MjpegContentLength::WithinLimit(len) | MjpegContentLength::Oversized(len) => len,
        }
    }
}

/// How many bytes of `data` the boundary at `boundary_idx` (in `pending` then `data`) uses up,
/// its line break included.
fn consumed_data_for_boundary(
    pending: &[u8],
    data: &[u8],
    boundary_idx: usize,
    boundary_len: usize,
) -> usize {
    let after_boundary = boundary_idx.saturating_add(boundary_len);
    let at = |index| joined_at(pending, data, index);
    // The end of the line break, in the joined bytes: a `\r` carried over in `pending` (its
    // `\n` arriving with this chunk) is not counted again.
    let end = if at(after_boundary) == Some(b'\r') && at(after_boundary + 1) == Some(b'\n') {
        after_boundary + 2
    } else if at(after_boundary) == Some(b'\n') {
        after_boundary + 1
    } else {
        after_boundary
    };
    end.saturating_sub(pending.len()).min(data.len())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_length_header_parser_is_shared_and_capped() {
        let parser = MjpegMultipartParser::new("--frame", 1024);
        assert_eq!(
            parser.content_length(b"Content-Length: 42\r\n"),
            Some(MjpegContentLength::WithinLimit(42))
        );
        assert_eq!(
            parser.content_length(b"content-length: 7\n"),
            Some(MjpegContentLength::WithinLimit(7))
        );
        assert_eq!(
            parser.content_length(b"Content-Length: 2048\r\n"),
            Some(MjpegContentLength::Oversized(2048))
        );
        assert_eq!(parser.content_length(b"Content-Type: image/jpeg\r\n"), None);
    }

    #[test]
    fn boundary_body_reader_reports_take_and_hit_boundary() {
        let parser = MjpegMultipartParser::new("--frame", 1024);
        let mut body = parser.body_reader();
        let mut buf = Vec::new();
        assert_eq!(
            body.append_chunk(b"abc--frame\r\nContent-Type: image/jpeg\r\n", &mut buf),
            (12, BoundaryRead::HitBoundary)
        );
        assert_eq!(buf, b"abc");

        let mut body = parser.body_reader();
        let mut buf = Vec::new();
        assert_eq!(
            body.append_chunk(b"abcdef", &mut buf),
            (6, BoundaryRead::Continue)
        );
        assert_eq!(buf, b"");
        let (take, outcome) = body.append_chunk(b"--frame\r\n", &mut buf);
        assert_eq!((take, outcome), (9, BoundaryRead::HitBoundary));
        assert_eq!(buf, b"abcdef");
    }

    #[test]
    fn boundary_body_reader_handles_boundary_split_across_chunks() {
        let parser = MjpegMultipartParser::new("--frame", 1024);
        let mut body = parser.body_reader();
        let mut buf = Vec::new();
        assert_eq!(
            body.append_chunk(b"jpeg-bytes--fra", &mut buf),
            (15, BoundaryRead::Continue)
        );
        assert_eq!(buf, b"jpeg-byte");

        let (take, outcome) = body.append_chunk(b"me\r\nContent-Type: image/jpeg\r\n", &mut buf);
        assert_eq!((take, outcome), (4, BoundaryRead::HitBoundary));
        assert_eq!(buf, b"jpeg-bytes");
    }

    #[test]
    fn boundary_body_reader_survives_arbitrary_streams() {
        let boundary = "--frame";
        let mut seed = 0x6e65_7463_616du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..500 {
            let parser = MjpegMultipartParser::new(boundary, 1024);
            let mut body = parser.body_reader();
            let mut buf = Vec::new();
            let mut fed = 0usize;
            for _ in 0..(next() % 20) {
                // Random bytes, sometimes with (partial) boundaries and line breaks mixed in.
                let mut chunk: Vec<u8> = (0..next() % 64).map(|_| next() as u8).collect();
                if next() % 3 == 0 {
                    let at = (next() as usize) % (chunk.len() + 1);
                    let cut = 1 + (next() as usize) % boundary.len();
                    chunk.splice(at..at, boundary.as_bytes()[..cut].iter().copied());
                }
                if next() % 4 == 0 {
                    chunk.extend_from_slice(b"\r\n");
                }
                let mut rest = &chunk[..];
                while !rest.is_empty() {
                    let (take, outcome) = body.append_chunk(rest, &mut buf);
                    assert!(take <= rest.len());
                    fed += take;
                    rest = &rest[take..];
                    if outcome == BoundaryRead::HitBoundary {
                        buf.clear();
                        body = parser.body_reader();
                    } else if take == 0 {
                        break;
                    }
                }
                assert!(buf.len() <= fed, "emitted more than was fed");
            }
            let _ = body.append_chunk(&[], &mut buf);
        }
    }

    /// The frames of a multipart stream fed in chunks of `chunk` bytes (each part's body as the
    /// parser's caller sees it: the bytes up to the next boundary).
    fn frames_in_chunks(stream: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let parser = MjpegMultipartParser::new("--frame", 1 << 20);
        let mut body = parser.body_reader();
        let mut buf = Vec::new();
        let mut frames = Vec::new();
        for piece in stream.chunks(chunk) {
            let mut rest = piece;
            while !rest.is_empty() {
                let (take, outcome) = body.append_chunk(rest, &mut buf);
                assert!(take <= rest.len());
                rest = &rest[take..];
                match outcome {
                    BoundaryRead::HitBoundary => {
                        frames.push(std::mem::take(&mut buf));
                        body = parser.body_reader();
                    }
                    BoundaryRead::Continue => {}
                    BoundaryRead::End => break,
                }
            }
        }
        frames
    }

    /// A multipart stream whose parts hold `payloads` (with boundary-like bytes in them), and the
    /// frames it must yield: each payload, with the line break that precedes the boundary.
    fn multipart_stream(payloads: &[Vec<u8>]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut stream = Vec::new();
        let mut frames = Vec::new();
        for payload in payloads {
            stream.extend_from_slice(payload);
            stream.extend_from_slice(b"\r\n--frame\r\n");
            let mut frame = payload.clone();
            frame.extend_from_slice(b"\r\n");
            frames.push(frame);
        }
        (stream, frames)
    }

    #[test]
    fn multipart_frames_are_identical_in_odd_chunk_sizes() {
        let mut seed = 0x6a70_6567u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let payloads: Vec<Vec<u8>> = (0..12)
            .map(|i| {
                let mut p: Vec<u8> = (0..(next() % 900 + 1)).map(|_| next() as u8).collect();
                // Boundary-like bytes, including a prefix at the end of a payload.
                p.extend_from_slice(if i % 2 == 0 { b"--fram" } else { b"--fra-x\r" });
                p
            })
            .collect();
        let (stream, expected) = multipart_stream(&payloads);
        for chunk in [1, 7, 4096, stream.len()] {
            assert_eq!(
                frames_in_chunks(&stream, chunk),
                expected,
                "chunk size {chunk}"
            );
        }
    }
}
