################################################################################
# Linux extension: build the Styx sensor bridge into the kernel tree
#
# For a Buildroot external tree (the Raze device package's
# devices/raze/gaia/buildroot-external, or HeliOS gaia/assets/buildroot): copy
# the whole kernel-modules/styx-sensor-bridge directory to
# linux/styx-sensor-bridge in the external tree, then place this file (and the
# Config.ext.in entries) in linux/ next to it. Buildroot includes
# linux/linux-ext-*.mk automatically. For the optional PiSP back end driver,
# copy kernel-modules/pispbe to linux/pispbe as well.
#
# Kernel-agnostic: the module is staged as drivers/media/platform/styx/ of
# whatever kernel the package builds (Raspberry Pi 6.12 for Raze 1.0.x,
# rpi-7.2.y for Raze 1.1.0) and built as part of it (=m), so it is built with
# that kernel's own Module.symvers, config and compiler and always matches the
# image. The overlays are compiled by that kernel's dtc.
################################################################################

ifeq ($(BR2_LINUX_KERNEL_EXT_STYX_SENSOR_BRIDGE),y)

STYX_BRIDGE_EXT_DIR := $(dir $(lastword $(MAKEFILE_LIST)))styx-sensor-bridge
STYX_BRIDGE_KDIR = $(LINUX_DIR)/drivers/media/platform/styx
STYX_BRIDGE_OVERLAYS = styx-sensor-bridge-cm5 styx-cam0-i2c-fast

define STYX_BRIDGE_COPY_FILES
	@echo "[styx-bridge] staging styx_sensor_bridge into the kernel tree"
	@mkdir -p $(STYX_BRIDGE_KDIR)
	@cp -f $(STYX_BRIDGE_EXT_DIR)/styx_sensor_bridge.c \
		$(STYX_BRIDGE_EXT_DIR)/styx_sensor_bridge.h \
		$(STYX_BRIDGE_EXT_DIR)/Kconfig $(STYX_BRIDGE_KDIR)/
	@cp -f $(STYX_BRIDGE_EXT_DIR)/Kbuild $(STYX_BRIDGE_KDIR)/Makefile
	@grep -q "^obj-y += styx/" $(LINUX_DIR)/drivers/media/platform/Makefile || \
		echo 'obj-y += styx/' >> $(LINUX_DIR)/drivers/media/platform/Makefile
	@grep -q "platform/styx/Kconfig" $(LINUX_DIR)/drivers/media/platform/Kconfig || \
		sed -i '/^endif # MEDIA_PLATFORM_DRIVERS/i source "drivers/media/platform/styx/Kconfig"' \
			$(LINUX_DIR)/drivers/media/platform/Kconfig
	@grep -q "platform/styx/Kconfig" $(LINUX_DIR)/drivers/media/platform/Kconfig || \
		{ echo "[styx-bridge] could not wire Kconfig"; exit 1; }
endef

LINUX_POST_PATCH_HOOKS += STYX_BRIDGE_COPY_FILES

define STYX_BRIDGE_ENABLE_CONFIG
	$(LINUX_DIR)/scripts/config --file $(LINUX_DIR)/.config --module VIDEO_STYX_SENSOR_BRIDGE
	$(MAKE) -C $(LINUX_DIR) $(LINUX_MAKE_FLAGS) olddefconfig
endef

LINUX_POST_CONFIGURE_HOOKS += STYX_BRIDGE_ENABLE_CONFIG

# Compile the overlays; the image's post-build copies $(BINARIES_DIR)/*.dtbo
# like the other device overlays (adjust to the board's overlay handling).
define STYX_BRIDGE_BUILD_OVERLAYS
	for o in $(STYX_BRIDGE_OVERLAYS); do \
		$(LINUX_DIR)/scripts/dtc/dtc -@ -q -I dts -O dtb \
			-o $(BINARIES_DIR)/$$o.dtbo \
			$(STYX_BRIDGE_EXT_DIR)/dts/$$o-overlay.dts || exit 1; \
	done
endef

LINUX_POST_INSTALL_IMAGES_HOOKS += STYX_BRIDGE_BUILD_OVERLAYS

endif

################################################################################
# Optional: the PiSP back end driver with Styx's cheaper per-job config write
# (kernel-modules/pispbe) in place of the kernel's own pisp_be.c.
#
# Only applied when the kernel's stock pisp_be.c is the one the Styx version
# was made from (sha256 below: rpi-7.2.y 53679a5, see pispbe/README.md). Any
# other kernel keeps its own driver and the build log says so: a kernel bump
# never silently ships a driver from another kernel. Re-import to update.
################################################################################

ifeq ($(BR2_LINUX_KERNEL_EXT_STYX_PISPBE),y)

STYX_PISPBE_EXT_DIR := $(dir $(lastword $(MAKEFILE_LIST)))pispbe
STYX_PISPBE_KDIR = $(LINUX_DIR)/drivers/media/platform/raspberrypi/pisp_be
STYX_PISPBE_STOCK_SHA256 = 6d36c1076af7276aeb5b3f5f9d8f3f88ee6a3deb16477d1e3a977ca2459adbce

define STYX_PISPBE_COPY_FILES
	@if [ "$$(sha256sum $(STYX_PISPBE_KDIR)/pisp_be.c | cut -d' ' -f1)" = "$(STYX_PISPBE_STOCK_SHA256)" ]; then \
		echo "[styx-pispbe] replacing pisp_be.c with the Styx version"; \
		cp -f $(STYX_PISPBE_EXT_DIR)/pisp_be.c $(STYX_PISPBE_EXT_DIR)/pisp_be_formats.h \
			$(STYX_PISPBE_KDIR)/; \
	elif cmp -s $(STYX_PISPBE_EXT_DIR)/pisp_be.c $(STYX_PISPBE_KDIR)/pisp_be.c; then \
		echo "[styx-pispbe] pisp_be.c is already the Styx version"; \
	else \
		echo "[styx-pispbe] WARNING: this kernel's pisp_be.c is not the one the Styx version"; \
		echo "[styx-pispbe] was made from; keeping the kernel's driver"; \
	fi
endef

LINUX_POST_PATCH_HOOKS += STYX_PISPBE_COPY_FILES

endif
