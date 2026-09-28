# SPDX-License-Identifier: GPL-2.0
#
# Out-of-tree build: make KDIR=<prepared kernel tree> ARCH=arm64 CROSS_COMPILE=...
# In-tree (Buildroot linux extension) the Kbuild file is used directly.
# See build.sh.

KDIR ?= /lib/modules/$(shell uname -r)/build
CONFIG_VIDEO_STYX_SENSOR_BRIDGE ?= m
export CONFIG_VIDEO_STYX_SENSOR_BRIDGE

all: modules

modules modules_install clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) $@

.PHONY: all modules modules_install clean
