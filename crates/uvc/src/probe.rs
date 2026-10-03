//! The video probe and commit controls (UVC 1.5 §4.3.1.1): format, frame size and interval
//! negotiation, and the frame and payload sizes the device then promises.

/// `VS_PROBE_CONTROL` / `VS_COMMIT_CONTROL` selectors.
pub const VS_PROBE_CONTROL: u8 = 0x01;
pub const VS_COMMIT_CONTROL: u8 = 0x02;

/// The probe/commit structure (the UVC 1.5 fields past byte 34 are kept as bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamingParams {
    /// `bmHint`: bit 0 keeps `dwFrameInterval` fixed while negotiating.
    pub hint: u16,
    pub format_index: u8,
    pub frame_index: u8,
    /// 100 ns units.
    pub frame_interval: u32,
    pub key_frame_rate: u16,
    pub p_frame_rate: u16,
    pub comp_quality: u16,
    pub comp_window_size: u16,
    pub delay: u16,
    pub max_video_frame_size: u32,
    /// Bytes per (micro)frame the stream needs: the bandwidth to reserve.
    pub max_payload_transfer_size: u32,
    pub clock_frequency: u32,
    pub framing_info: u8,
    pub preferred_version: u8,
    pub min_version: u8,
    pub max_version: u8,
    pub uvc15: [u8; 14],
}

impl StreamingParams {
    /// The structure's length for a device of `bcdUVC` `version`: 26 (1.0), 34 (1.1), 48 (1.5).
    pub fn len_for(version: u16) -> usize {
        if version >= 0x0150 {
            48
        } else if version >= 0x0110 {
            34
        } else {
            26
        }
    }

    /// A request for format `format`, frame `frame`, `interval` (100 ns), interval fixed.
    pub fn request(format: u8, frame: u8, interval: u32) -> StreamingParams {
        StreamingParams {
            hint: 1,
            format_index: format,
            frame_index: frame,
            frame_interval: interval,
            ..StreamingParams::default()
        }
    }

    /// Encodes `len` bytes (26, 34 or 48).
    pub fn encode(&self, len: usize) -> Vec<u8> {
        let mut b = Vec::with_capacity(48);
        b.extend_from_slice(&self.hint.to_le_bytes());
        b.push(self.format_index);
        b.push(self.frame_index);
        b.extend_from_slice(&self.frame_interval.to_le_bytes());
        for v in [
            self.key_frame_rate,
            self.p_frame_rate,
            self.comp_quality,
            self.comp_window_size,
            self.delay,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.max_video_frame_size.to_le_bytes());
        b.extend_from_slice(&self.max_payload_transfer_size.to_le_bytes());
        b.extend_from_slice(&self.clock_frequency.to_le_bytes());
        b.extend_from_slice(&[
            self.framing_info,
            self.preferred_version,
            self.min_version,
            self.max_version,
        ]);
        b.extend_from_slice(&self.uvc15);
        b.truncate(len);
        b.resize(len, 0);
        b
    }

    /// Decodes what the device returned (missing fields stay 0).
    pub fn decode(b: &[u8]) -> StreamingParams {
        let mut full = [0u8; 48];
        let n = b.len().min(48);
        full[..n].copy_from_slice(&b[..n]);
        let u16at = |at: usize| u16::from_le_bytes([full[at], full[at + 1]]);
        let u32at =
            |at: usize| u32::from_le_bytes([full[at], full[at + 1], full[at + 2], full[at + 3]]);
        let mut uvc15 = [0u8; 14];
        uvc15.copy_from_slice(&full[34..48]);
        StreamingParams {
            hint: u16at(0),
            format_index: full[2],
            frame_index: full[3],
            frame_interval: u32at(4),
            key_frame_rate: u16at(8),
            p_frame_rate: u16at(10),
            comp_quality: u16at(12),
            comp_window_size: u16at(14),
            delay: u16at(16),
            max_video_frame_size: u32at(18),
            max_payload_transfer_size: u32at(22),
            clock_frequency: u32at(26),
            framing_info: full[30],
            preferred_version: full[31],
            min_version: full[32],
            max_version: full[33],
            uvc15,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_at_every_length() {
        let p = StreamingParams {
            hint: 1,
            format_index: 2,
            frame_index: 3,
            frame_interval: 333_333,
            max_video_frame_size: 614_400,
            max_payload_transfer_size: 3072,
            clock_frequency: 48_000_000,
            framing_info: 3,
            ..StreamingParams::default()
        };
        for (version, len) in [(0x0100, 26), (0x0110, 34), (0x0150, 48)] {
            assert_eq!(StreamingParams::len_for(version), len);
            let b = p.encode(len);
            assert_eq!(b.len(), len);
            let back = StreamingParams::decode(&b);
            assert_eq!(back.frame_interval, 333_333);
            assert_eq!(back.max_payload_transfer_size, 3072);
            assert_eq!(back.clock_frequency, if len > 26 { 48_000_000 } else { 0 });
        }
        assert_eq!(&p.encode(26)[..8], &[1, 0, 2, 3, 0x15, 0x16, 0x05, 0x00]);
    }
}
