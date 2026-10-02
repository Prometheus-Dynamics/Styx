#!/usr/bin/env python3
"""Image quality of NV12 frames of the same scene through different paths (e.g. libcamera and
the native PiSP path at the same fixed exposure): shading, per-zone colour, white balance,
tone curve, noise in flat areas, sharpness. Writes a JSON summary and PNG crops.

    quality.py --size 1280x800 --out DIR NAME=FILE[:full|:limited] ...

FILE holds the planes packed (Y rows, then interleaved CbCr rows, no stride padding).
`full` is full-range BT.601 (the PiSP's "jpeg" encoding, the default), `limited` is
limited-range BT.709 (libcamera's video colour space). The first frame is the reference.
Needs numpy and Pillow.
"""

import argparse
import json
import os

import numpy as np
from PIL import Image


def load(path, w, h, rng):
    raw = np.fromfile(path, dtype=np.uint8)
    y = raw[: w * h].reshape(h, w).astype(np.float64)
    uv = raw[w * h : w * h * 3 // 2].reshape(h // 2, w // 2, 2).astype(np.float64)
    u = np.repeat(np.repeat(uv[:, :, 0], 2, 0), 2, 1)
    v = np.repeat(np.repeat(uv[:, :, 1], 2, 0), 2, 1)
    if rng == "limited":
        yn = (y - 16) / 219
        cb, cr = (u - 128) / 224, (v - 128) / 224
        kr, kb = 0.2126, 0.0722
    else:
        yn = y / 255
        cb, cr = (u - 128) / 255, (v - 128) / 255
        kr, kb = 0.299, 0.114
    kg = 1 - kr - kb
    r = yn + 2 * (1 - kr) * cr
    b = yn + 2 * (1 - kb) * cb
    g = (yn - kr * r - kb * b) / kg
    rgb = np.clip(np.stack([r, g, b], -1), 0, 1)
    # Luma on one scale for both (BT.601 weights of the decoded RGB).
    luma = rgb @ np.array([0.299, 0.587, 0.114])
    return rgb, luma


def zones(a, nx, ny):
    h, w = a.shape[:2]
    out = np.zeros((ny, nx) + a.shape[2:])
    for j in range(ny):
        for i in range(nx):
            out[j, i] = a[j * h // ny : (j + 1) * h // ny, i * w // nx : (i + 1) * w // nx].mean((0, 1))
    return out


def box(a, k):
    c = np.cumsum(np.cumsum(np.pad(a, ((k, k), (k, k)), mode="edge"), 0), 1)
    c = np.pad(c, ((1, 0), (1, 0)))
    n = 2 * k + 1
    s = c[n:, n:] - c[:-n, n:] - c[n:, :-n] + c[:-n, :-n]
    return s / (n * n)


def flat_noise(luma, ref_luma):
    """Noise (std of luma minus its 5x5 mean) in the 10% flattest 16x16 blocks of the
    reference, on the 0..255 scale."""
    hp = luma - box(luma, 2)
    smooth = box(ref_luma, 2)
    gy, gx = np.gradient(smooth)
    grad = np.hypot(gx, gy)
    b = 16
    h, w = luma.shape
    blocks = []
    for y in range(0, h - b, b):
        for x in range(0, w - b, b):
            m = smooth[y : y + b, x : x + b].mean()
            if 0.1 < m < 0.9:
                blocks.append((grad[y : y + b, x : x + b].mean(), y, x, m))
    blocks.sort()
    sel = blocks[: max(1, len(blocks) // 10)]
    stds = [hp[y : y + b, x : x + b].std() * 255 for _, y, x, _ in sel]
    means = [m for *_, m in sel]
    return float(np.median(stds)), float(np.mean(means)), sel


def sharpness(luma):
    """Mean gradient magnitude in the 5% strongest-gradient pixels (0..255 per pixel) and the
    share of the luma's energy above a quarter of the sampling frequency."""
    gy, gx = np.gradient(luma * 255)
    g = np.hypot(gx, gy)
    top = np.sort(g.ravel())[-len(g.ravel()) // 20 :]
    f = np.abs(np.fft.rfft2(luma - luma.mean())) ** 2
    fy = np.abs(np.fft.fftfreq(luma.shape[0]))[:, None]
    fx = np.fft.rfftfreq(luma.shape[1])[None, :]
    hf = f[(np.maximum(fx, fy) > 0.25)].sum() / f.sum()
    return float(top.mean()), float(hf)


def edge_width(luma, ref_luma):
    """10-90% rise distance (pixels) of the strongest vertical-ish edges, averaged."""
    gx = np.abs(np.gradient(box(ref_luma, 1), axis=1))
    h, w = luma.shape
    widths = []
    cand = np.argsort(gx[8:-8, 8:-8].ravel())[::-1][:4000]
    used = set()
    for idx in cand:
        y, x = divmod(idx, w - 16)
        y, x = y + 8, x + 8
        if (y // 8, x // 8) in used:
            continue
        used.add((y // 8, x // 8))
        p = luma[y, x - 8 : x + 9]
        lo, hi = p[:3].mean(), p[-3:].mean()
        if abs(hi - lo) < 0.15:
            continue
        q = (p - lo) / (hi - lo)
        if q[0] > q[-1]:
            q = 1 - q
        try:
            a = np.interp(0.1, np.maximum.accumulate(q), np.arange(17))
            b = np.interp(0.9, np.maximum.accumulate(q), np.arange(17))
            widths.append(b - a)
        except ValueError:
            continue
        if len(widths) >= 200:
            break
    return float(np.median(widths)) if widths else None


def tone(luma, ref_luma):
    """Mean luma of this frame per bin of the reference's luma (the transfer between them)."""
    bins = np.linspace(0, 1, 17)
    idx = np.digitize(ref_luma.ravel(), bins) - 1
    out = []
    for i in range(16):
        m = idx == i
        out.append(float(luma.ravel()[m].mean()) if m.sum() > 500 else None)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--size", default="1280x800")
    ap.add_argument("--out", required=True)
    ap.add_argument("frames", nargs="+")
    a = ap.parse_args()
    w, h = map(int, a.size.split("x"))
    os.makedirs(a.out, exist_ok=True)
    frames = []
    for spec in a.frames:
        name, rest = spec.split("=", 1)
        if rest.endswith((":full", ":limited")):
            path, rng = rest.rsplit(":", 1)
        else:
            path, rng = rest, "full"

        rgb, luma = load(path, w, h, rng)
        frames.append((name, rgb, luma))
    ref_luma = frames[0][2]
    report = {}
    nx, ny = 8, 5
    ref_zl = zones(ref_luma, nx, ny)
    ref_zr = zones(frames[0][1], nx, ny)
    for name, rgb, luma in frames:
        zl = zones(luma, nx, ny)
        zr = zones(rgb, nx, ny)
        # Against the reference (same scene): shading and colour differences per zone.
        rel = zl / ref_zl
        rel = rel / rel[ny // 2, nx // 2 - 1 : nx // 2 + 1].mean()
        d_rg = zr[..., 0] / zr[..., 1] / (ref_zr[..., 0] / ref_zr[..., 1])
        d_bg = zr[..., 2] / zr[..., 1] / (ref_zr[..., 2] / ref_zr[..., 1])
        centre = zl[ny // 2, nx // 2 - 1 : nx // 2 + 1].mean()
        corners = [zl[0, 0], zl[0, -1], zl[-1, 0], zl[-1, -1]]
        noise, noise_level, _ = flat_noise(luma, ref_luma)
        tenengrad, hf = sharpness(luma)
        report[name] = {
            "mean_rgb": [float(x) for x in rgb.reshape(-1, 3).mean(0)],
            "grey_world_rg_bg": [
                float(rgb[..., 0].mean() / rgb[..., 1].mean()),
                float(rgb[..., 2].mean() / rgb[..., 1].mean()),
            ],
            "mean_luma": float(luma.mean()),
            "corner_over_centre": [float(c / centre) for c in corners],
            "corners_vs_reference": [float(rel[0, 0]), float(rel[0, -1]), float(rel[-1, 0]), float(rel[-1, -1])],
            "zone_luma_vs_reference_min_max": [float(rel.min()), float(rel.max())],
            "zone_rg_vs_reference_mean_min_max": [float(d_rg.mean()), float(d_rg.min()), float(d_rg.max())],
            "zone_bg_vs_reference_mean_min_max": [float(d_bg.mean()), float(d_bg.min()), float(d_bg.max())],
            "zone_luma": zl.round(4).tolist(),
            "zone_rg": (zr[..., 0] / zr[..., 1]).round(3).tolist(),
            "zone_bg": (zr[..., 2] / zr[..., 1]).round(3).tolist(),
            "flat_noise_std_255": noise,
            "flat_noise_at_luma": noise_level,
            "sharpness_top5pct_gradient": tenengrad,
            "hf_energy_share": hf,
            "edge_10_90_px": edge_width(luma, ref_luma),
            "tone_vs_reference": tone(luma, ref_luma),
        }
    with open(os.path.join(a.out, "quality.json"), "w") as f:
        json.dump(report, f, indent=1)
    # Crops side by side: full (half size), centre, a corner, the flattest area, an edge area.
    _, _, sel = flat_noise(ref_luma, ref_luma)
    fy, fx = sel[0][1], sel[0][2]
    crops = {
        "full": None,
        "centre": (h // 2 - 128, w // 2 - 128),
        "corner": (h - 256, 0),
        "flat": (min(max(fy - 120, 0), h - 256), min(max(fx - 120, 0), w - 256)),
    }
    gy, gx = np.gradient(box(ref_luma, 1))
    e = np.hypot(gx, gy)
    ey, ex = np.unravel_index(np.argmax(box(e, 24)), e.shape)
    crops["edges"] = (min(max(ey - 128, 0), h - 256), min(max(ex - 128, 0), w - 256))
    names = []
    for crop, at in crops.items():
        tiles = []
        for _, rgb, _ in frames:
            img = (rgb * 255 + 0.5).astype(np.uint8)
            if at is None:
                tiles.append(Image.fromarray(img).resize((w // 2, h // 2), Image.BILINEAR))
            else:
                y, x = at
                t = Image.fromarray(img[y : y + 256, x : x + 256])
                tiles.append(t.resize((512, 512), Image.NEAREST))
        tw, th = tiles[0].size
        sheet = Image.new("RGB", (tw * len(tiles) + 8 * (len(tiles) - 1), th), (255, 255, 255))
        for i, t in enumerate(tiles):
            sheet.paste(t, (i * (tw + 8), 0))
        path = os.path.join(a.out, f"quality-{crop}.png")
        sheet.save(path)
        names.append(path)
    print(json.dumps({k: {kk: vv for kk, vv in v.items() if not kk.startswith("zone")} for k, v in report.items()}, indent=1))
    print("order:", [n for n, *_ in frames])
    print("\n".join(names))


if __name__ == "__main__":
    main()
