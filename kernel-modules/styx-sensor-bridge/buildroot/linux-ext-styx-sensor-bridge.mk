################################################################################
# Linux extension: build the Styx sensor bridge into the kernel tree
#
# For a Buildroot external tree (a device package's buildroot-external tree, or a
# CM5 image's Buildroot tree): copy
# the whole kernel-modules/styx-sensor-bridge directory to
# linux/styx-sensor-bridge in the external tree, then place this file (and the
# Config.ext.in entries) in linux/ next to it. Buildroot includes
# linux/linux-ext-*.mk automatically.
#
# Kernel-agnostic: the module is staged as drivers/media/platform/styx/ of
# whatever kernel the package builds (Raspberry Pi 6.12 for image release 1.0.x,
# rpi-7.2.y for 1.1.0) and built as part of it (=m), so it is built with
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
