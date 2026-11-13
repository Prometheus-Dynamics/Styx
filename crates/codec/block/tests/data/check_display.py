#!/usr/bin/env python3
"""Checks a stream's display order against a decoder's output, which is what the expectations
claim (`display pic=...`). Usage:

    python3 -I check_display.py <source.yuv> <decoded.yuv> <width> <height>

`source.yuv` holds the frames the stream was encoded from (I420, in display order). `decoded.yuv`
is what a decoder output, in its output order (GStreamer's `vah264dec`, here). Each decoded frame
is matched to the source frame it is closest to; the printed list is those source indices. A
display order is right when the list is 0, 1, 2, ... The mean absolute error of each match is
printed beside it, so a wrong match shows as a large error.
"""
import sys

import numpy as np


def main():
    src_path, dec_path, width, height = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
    frame = width * height * 3 // 2
    src = np.frombuffer(open(src_path, "rb").read(), np.uint8)
    dec = np.frombuffer(open(dec_path, "rb").read(), np.uint8)
    ns, nd = len(src) // frame, len(dec) // frame
    s = src[: ns * frame].reshape(ns, frame).astype(np.int32)
    d = dec[: nd * frame].reshape(nd, frame).astype(np.int32)
    out = []
    for i in range(nd):
        err = np.abs(s - d[i]).mean(axis=1)
        j = int(np.argmin(err))
        out.append(f"{j}({err[j]:.1f})")
    print(" ".join(out))


if __name__ == "__main__":
    main()
