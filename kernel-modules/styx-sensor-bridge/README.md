# styx-sensor-bridge

A generic Linux kernel module (GPL-2.0) that stands in for a camera sensor driver: it registers a
V4L2 subdevice bound to the CSI receiver through the device tree, while the sensor itself is
driven from userspace by Styx. Written once; no sensor-specific code. See
`docs/native-stack/README.md` and `PROTOCOL.md`.
