use super::*;

/// Basic tuning knobs for FFmpeg encoders.
#[derive(Clone, Copy, Debug)]
pub struct FfmpegEncoderOptions {
    pub bitrate: u64,
    pub gop: Option<i32>,
    pub framerate: Option<(u32, u32)>,
    pub thread_count: Option<usize>,
    pub pool_limits: Option<(usize, usize, usize)>,
    pub output_resolution: Option<Resolution>,
}

impl Default for FfmpegEncoderOptions {
    fn default() -> Self {
        Self {
            bitrate: 20_000_000,
            gop: None,
            framerate: None,
            thread_count: None,
            pool_limits: None,
            output_resolution: None,
        }
    }
}

impl FfmpegVideoEncoder {
    pub fn set_bitrate(&self, bitrate: u64) {
        if bitrate == 0 {
            return;
        }
        if let Ok(mut guard) = self.state.lock()
            && let Some(state) = guard.as_mut()
        {
            state
                .encoder
                .set_bit_rate(usize::try_from(bitrate).unwrap_or(usize::MAX));
        }
        if let Ok(mut opts) = self.opts.lock() {
            opts.bitrate = bitrate;
        }
    }

    pub fn set_gop(&self, gop: Option<i32>) {
        if let Ok(mut locked) = self.opts.lock() {
            locked.gop = gop;
        }
        if let Some(g) = gop
            && let Ok(mut guard) = self.state.lock()
            && let Some(state) = guard.as_mut()
        {
            let gop_u32: u32 = g.try_into().unwrap_or(u32::MAX);
            state.encoder.set_gop(gop_u32);
        }
    }

    pub fn set_framerate(&self, framerate: Option<(u32, u32)>) {
        if let Ok(mut locked) = self.opts.lock() {
            locked.framerate = framerate;
        }
        if let Some((num, den)) = framerate
            && let Ok(mut guard) = self.state.lock()
            && let Some(state) = guard.as_mut()
        {
            state.encoder.set_time_base((den as i32, num as i32));
        }
    }

    pub fn set_output_resolution(&self, res: Option<Resolution>) {
        if let Ok(mut locked) = self.opts.lock() {
            locked.output_resolution = res;
        }
        if let Ok(mut guard) = self.state.lock() {
            *guard = None;
        }
    }
}

pub(crate) fn probe_v4l2m2m_encoder(enc: &FfmpegVideoEncoder) -> Result<(), CodecError> {
    let probes = [(640_u32, 480_u32), (1280_u32, 720_u32)];
    let mut last_err: Option<CodecError> = None;

    for (w, h) in probes {
        let res = (|| {
            let input = enc.descriptor.input;
            let resolution = Resolution::new(w, h)
                .ok_or_else(|| CodecError::Codec("invalid probe resolution".into()))?;
            let meta = FrameMeta::new(MediaFormat::new(input, resolution, ColorSpace::Unknown), 0);

            let frame = match input {
                FourCc { .. } if input == FourCc::new(*b"RG24") => {
                    let bytes = (w as usize).saturating_mul(h as usize).saturating_mul(3);
                    let pool = BufferPool::with_capacity(1, bytes.max(1));
                    let mut buf = pool.lease();
                    buf.resize(bytes);
                    buf.as_mut_slice().fill(0);
                    let stride = (w as usize).saturating_mul(3).max(1);
                    FrameLease::single_plane(meta, buf, bytes, stride)
                }
                FourCc { .. } if input == FourCc::new(*b"NV12") => {
                    let y_len = (w as usize).saturating_mul(h as usize);
                    let uv_len = (w as usize).saturating_mul(h as usize).saturating_div(2);
                    let pool = BufferPool::with_capacity(2, y_len.max(uv_len).max(1));

                    let mut y = pool.lease();
                    y.resize(y_len);
                    y.as_mut_slice().fill(0);

                    let mut uv = pool.lease();
                    uv.resize(uv_len);
                    uv.as_mut_slice().fill(0);

                    FrameLease::multi_plane(
                        meta,
                        smallvec::smallvec![y, uv],
                        smallvec::smallvec![
                            PlaneLayout {
                                offset: 0,
                                len: y_len,
                                stride: w as usize
                            },
                            PlaneLayout {
                                offset: 0,
                                len: uv_len,
                                stride: w as usize
                            },
                        ],
                    )
                }
                other => {
                    return Err(CodecError::Codec(format!(
                        "unsupported v4l2m2m probe input: {other:?}"
                    )));
                }
            };

            let _ = enc.encode_all(&frame)?;
            Ok(())
        })();

        match res {
            Ok(()) => {
                let _ = enc.clear_state();
                return Ok(());
            }
            Err(err) => {
                let _ = enc.clear_state();
                last_err = Some(err);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| CodecError::Codec("v4l2m2m probe failed".into())))
}
