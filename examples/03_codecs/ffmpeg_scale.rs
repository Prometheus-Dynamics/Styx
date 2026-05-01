#[cfg(feature = "codec-ffmpeg")]
use std::sync::Arc;
#[cfg(feature = "codec-ffmpeg")]
use std::time::Duration;

#[cfg(feature = "codec-ffmpeg")]
use styx::prelude::*;
#[cfg(feature = "codec-ffmpeg")]
use styx_codec::ffmpeg::FfmpegEncoderOptions;

#[cfg(feature = "codec-ffmpeg")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device = virtual_device();
    let src_mode = device.default_mode().ok_or("virtual device missing mode")?;

    // Force encoder output to a different resolution than the input.
    let target_res = Resolution::new(320, 180).unwrap();
    let encoder = Arc::new(FfmpegH264Encoder::with_options(FfmpegEncoderOptions {
        output_resolution: Some(target_res),
        bitrate: 1_000_000,
        gop: Some(30),
        framerate: Some((30, 1)),
        thread_count: None,
        pool_limits: None,
    })?);

    let mut pipeline = MediaPipelineBuilder::new(device.capture_request())
        .encoder(encoder.clone())
        .without_decoder()
        .start()?;

    println!(
        "input={}x{} output={}x{}",
        src_mode.format.resolution.width,
        src_mode.format.resolution.height,
        target_res.width,
        target_res.height
    );

    let mut frames = 0;
    while frames < 16 {
        match pipeline.next_blocking_result(Duration::from_millis(5))? {
            RecvOutcome::Data(frame) => {
                frames += 1;
                let meta = frame.meta();
                println!(
                    "#{frames:02} fourcc={:?} res={}x{} bytes={}",
                    meta.format.code,
                    meta.format.resolution.width,
                    meta.format.resolution.height,
                    frame.planes().first().map(|p| p.data().len()).unwrap_or(0)
                );
            }
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        }
    }

    // Change output resolution on the fly.
    if let Some(res) = Resolution::new(160, 90) {
        encoder.0.set_output_resolution(Some(res));
        println!("reconfigured output to {}x{}", res.width, res.height);
        let mut more = 0;
        while more < 8 {
            match pipeline.next_blocking_result(Duration::from_millis(5))? {
                RecvOutcome::Data(frame) => {
                    more += 1;
                    let meta = frame.meta();
                    println!(
                        "reconf #{more:02} fourcc={:?} res={}x{} bytes={}",
                        meta.format.code,
                        meta.format.resolution.width,
                        meta.format.resolution.height,
                        frame.planes().first().map(|p| p.data().len()).unwrap_or(0)
                    );
                }
                RecvOutcome::Empty => continue,
                RecvOutcome::Closed => break,
            }
        }
    }

    pipeline.stop();
    Ok(())
}

#[cfg(not(feature = "codec-ffmpeg"))]
fn main() {
    eprintln!("Enable the `codec-ffmpeg` feature to run this example.");
}

#[cfg(feature = "codec-ffmpeg")]
fn virtual_device() -> ProbedDevice {
    CaptureRequest::virtual_source(
        VirtualSourceConfig::new()
            .name("virtual-ffmpeg-scale")
            .resolution(640, 360)
            .fps(30),
    )
    .into_device()
}
