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
                tracing::warn!(
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
                    tracing::warn!(
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
                tracing::warn!(
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

        let pending_len = self.pending.len();
        let mut combined = Vec::with_capacity(pending_len + data.len());
        combined.extend_from_slice(&self.pending);
        combined.extend_from_slice(data);

        if let Some(boundary_idx) = find_subslice(&combined, self.boundary) {
            let after_boundary = boundary_idx.saturating_add(self.boundary.len());
            if after_boundary >= combined.len()
                || (combined.get(after_boundary) == Some(&b'\r')
                    && after_boundary + 1 >= combined.len())
            {
                buf.extend_from_slice(&combined[..boundary_idx]);
                self.pending.clear();
                self.pending.extend_from_slice(&combined[boundary_idx..]);
                tracing::trace!(
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
            buf.extend_from_slice(&combined[..boundary_idx]);
            self.pending.clear();
            let consume = consumed_data_for_boundary(
                &combined,
                pending_len,
                boundary_idx,
                self.boundary.len(),
            );
            tracing::trace!(
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

        let keep = self.boundary.len().saturating_sub(1).min(combined.len());
        let emit_len = combined.len().saturating_sub(keep);
        buf.extend_from_slice(&combined[..emit_len]);
        self.pending.clear();
        self.pending.extend_from_slice(&combined[emit_len..]);
        (data.len(), BoundaryRead::Continue)
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

fn consumed_data_for_boundary(
    combined: &[u8],
    pending_len: usize,
    boundary_idx: usize,
    boundary_len: usize,
) -> usize {
    let mut consumed = boundary_idx
        .saturating_add(boundary_len)
        .saturating_sub(pending_len);
    let after_boundary = boundary_idx.saturating_add(boundary_len);
    if combined.get(after_boundary) == Some(&b'\r')
        && combined.get(after_boundary + 1) == Some(&b'\n')
    {
        consumed = consumed.saturating_add(2);
    } else if combined.get(after_boundary) == Some(&b'\n') {
        consumed = consumed.saturating_add(1);
    }
    consumed.min(combined.len().saturating_sub(pending_len))
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
}
