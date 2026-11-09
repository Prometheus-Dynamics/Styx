# Boards

Support is per chip family; the boards are listed under their chip. The table is data
(`styx_pipeline::chips::CHIPS`). **Nothing on this page has run on its board.** All of it is
written from vendor sources and marked written, untested on board, or says what is missing.

| Chip | Boards | ISP | Encoder | 2D scaler |
|---|---|---|---|---|
| RK3588S | ROCK 5C (RK3588S2) | ISP30, written | MPP, written | RGA3, convert and scaling written |
| RK3566 | ZERO 3E | ISP21, written | MPP, written | RGA2, convert and scaling written |
| RV1126B | Luckfox Aura | ISP35, **missing** (software ISP) | MPP, written | RGA, convert and scaling written |
| A733 | Cubie A7A, A7S, A7Z | ISP600, **missing** | Cedar VE, **missing** | G2D, **missing** |
| K230D | K230D AI-CAM | ISP8000 registers written, frame path **missing** (software ISP) | V4L2 M2M, written | V4L2 M2M, written |

A Rockchip chip whose ISP generation Styx does not know runs on the software ISP
(`has_rkisp`), rather than failing when the capture opens.

## What each gap needs

**Lemnos (device layer), not moved:**

- RV1126B: the ISP35 layouts (`rk-isp35-config.h`, develop-6.1; `hw.c:911-914` gives
  `rv1126b_isp_match_data` = `ISP_V35`), `Isp::from_soc("rockchip,rv1126b")` and its nodes in
  `lemnos-rkisp-sys` / `lemnos-drivers-rkisp`. Then Styx's parameters and statistics.
- K230D: the frame path behind the ISP8000.
- Rockchip: the rkcif sensor-to-ISP media graph per chip (`IspSetup`).
- A733: `VIDIOC_S_INPUT` on `VideoDevice` (sunxi-vin powers and selects the sensor there, before
  `S_FMT`); access to `/dev/cedar_dev` and `/dev/g2d` if those are driven directly.

**Styx:**

- A733 capture: the V4L2 backend's multi-planar path for `sunxi-vin`, after `S_INPUT`.
- A733 ISP: 3A is the closed `AWIspApi` library, or raw frames through the software ISP.
- A733 encode: the Cedar VE has no V4L2 device; the vendor `libcedarc`, or a driver.

Sources for the A733: `radxa/allwinner-bsp` (`drivers/vin`, `isp600`, `drivers/ve`,
`drivers/g2d`); `sun60iw2` is the A733.
