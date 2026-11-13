# H.264 stream fixtures

Real encoder output for `tests/h264_streams.rs`. Each `<name>.h264` is an Annex B byte stream;
`<name>.expect` is what the parser must report for it, written by `make_expect.py`. The
expectations come from outside this crate: the syntax from FFmpeg's CBS trace
(`-bsf:v trace_headers`), the reference lists and marking from a model of ITU-T H.264 8.2.4 and
8.2.5 in `make_expect.py`, and the display order is checked against a hardware decoder's output
by `check_display.py`.

Every stream is 16 pictures, 160x128 (or 256x256 for the weighted ones), 4:2:0, one IDR at the
start, one IDR per stream and no MMCO 5.

## Encoders and versions

- VA-API (`h264_vaapi`, and GStreamer `vah264enc`): AMD Radeon RX 6800 XT (`/dev/dri/renderD129`,
  Mesa radeonsi, VA-API 1.23 / libva 2.23.0).
- NVENC (`h264_nvenc`): NVIDIA GeForce RTX 4060, driver 595.104.02.
- FFmpeg 8.1.3 (built with gcc 16). This build has no native H.264 decoder (only libopenh264, which
  cannot decode B slices or CABAC), so `ffprobe -show_frames` cannot give picture types or output
  order for these streams. The GStreamer `vah264dec` decoder does, and the display order
  was checked with it.
- GStreamer 1.28.7.

## Commands

The H.264 streams were written to `.mkv` by FFmpeg, then extracted to Annex B:
`ffmpeg -y -loglevel error -i X.mkv -c copy -bsf:v h264_mp4toannexb -f h264 X.h264`.

- `vaapi_high_bf2_cabac`: `ffmpeg -y -init_hw_device vaapi=va:/dev/dri/renderD129 -filter_hw_device va -f lavfi -i testsrc=size=160x128:rate=25 -frames:v 16 -vf format=nv12,hwupload -c:v h264_vaapi -bf 2 -b_depth 2 -profile:v high -coder cabac -g 16 X.mkv`
- `vaapi_main_p_cavlc`: as above with `-bf 0 -profile:v main -coder cavlc -g 16`.
- `nvenc_high_bf3_each_cabac`: `ffmpeg -y -f lavfi -i testsrc=size=160x128:rate=25 -frames:v 16 -pix_fmt yuv420p -c:v h264_nvenc -profile:v high -bf 3 -b_ref_mode each -refs 4 -coder cabac -g 16 X.mkv`
- `nvenc_main_refs4_cavlc`: as above with `-profile:v main -bf 0 -coder cavlc -refs 4 -g 16`.
- `nvenc_high_p_wp_cabac`: `ffmpeg -y -f lavfi -i "testsrc=size=256x256:rate=25,geq=lum='max(0,lum(X,Y)-4*N)':cb='cb(X,Y)':cr='cr(X,Y)'" -frames:v 16 -pix_fmt yuv420p -c:v h264_nvenc -profile:v high -bf 0 -weighted_pred 1 -g 16 X.mkv`
- `nvenc_main_p_wp_cavlc`: as above with `-profile:v main -bf 0 -coder cavlc -weighted_pred 1 -g 16`.
- `vaapi_gst_b2_refs4_slices3_cabac` (and `_cavlc` with `cabac=false`):
  `gst-launch-1.0 -q videotestsrc num-buffers=16 pattern=ball ! video/x-raw,format=NV12,width=160,height=128,framerate=25/1 ! vah264enc b-frames=2 ref-frames=4 num-slices=3 cabac=true key-int-max=16 ! h264parse ! video/x-h264,stream-format=byte-stream,alignment=au ! filesink location=X.h264`
  (`vah264enc` with `b-pyramid=true` fails on this driver: B-frames as L1 references are not supported.)

Expectations: `python3 -I make_expect.py X.h264 X.expect`.

Display order: decode `X.h264` with `gst-launch-1.0 -q filesrc location=X.h264 ! h264parse ! vah264dec ! videoconvert ! video/x-raw,format=I420 ! filesink location=dec.yuv`
and check it with `python3 -I check_display.py source.yuv dec.yuv W H`, where `source.yuv` is the
frames the stream was encoded from. Every fixture gives `0 1 ... 15` (mean error at most 0.5).

## Not covered by a real stream

No encoder here produces these, so they remain covered only by the hand-built headers in
`src/h264/tests.rs`: long-term references (MMCO 2, 3 with a long-term index, 4, 6 and
`long_term_reference_flag`), MMCO 5, temporal direct (`direct_spatial_mv_pred_flag` 0), weighted
bi-prediction (`weighted_bipred_idc` 1), weight tables with more than one active entry, and list
modifications with long-term entries (idc 2). Field pictures and FMO are refused by design.
