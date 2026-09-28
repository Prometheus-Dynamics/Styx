################################################################################
# Linux extension: build the Styx sensor bridge into the kernel tree
#
# For a Buildroot external tree (e.g. HeliOS gaia/assets/buildroot): copy the
# whole kernel-modules/styx-sensor-bridge directory to linux/styx-sensor-bridge
# in the external tree, then place this file (and the Config.ext.in entry) in
# linux/ next to it. Buildroot includes linux/linux-ext-*.mk automatically.
#
# The module is staged as drivers/media/platform/styx/ and built as part of the
# kernel (=m), so it is built with the kernel's own Module.symvers, config and
# compiler and always matches the image. The overlay is compiled by the
# kernel's dtc and installed next to the other overlays.
################################################################################

ifeq ($(BR2_LINUX_KERNEL_EXT_STYX_SENSOR_BRIDGE),y)

STYX_BRIDGE_EXT_DIR := $(dir $(lastword $(MAKEFILE_LIST)))styx-sensor-bridge
STYX_BRIDGE_KDIR = $(LINUX_DIR)/drivers/media/platform/styx

define STYX_BRIDGE_COPY_FILES
	@echo "[styx-bridge] staging styx_sensor_bridge into the kernel tree"
	@mkdir -p $(STYX_BRIDGE_KDIR)
	@cp -f $(STYX_BRIDGE_EXT_DIR)/styx_sensor_bridge.c \
		$(STYX_BRIDGE_EXT_DIR)/styx_sensor_bridge.h \
		$(STYX_BRIDGE_EXT_DIR)/Kconfig $(STYX_BRIDGE_KDIR)/
	@cp -f $(STYX_BRIDGE_EXT_DIR)/Kbuild $(STYX_BRIDGE_KDIR)/Makefile
	@grep -q "platform/styx/" $(LINUX_DIR)/drivers/media/platform/Makefile || \
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

# Compile the CM5 overlay; the image's post-build copies $(BINARIES_DIR)/*.dtbo
# like the other HeliOS overlays (adjust to the board's overlay handling).
define STYX_BRIDGE_BUILD_OVERLAY
	$(LINUX_DIR)/scripts/dtc/dtc -@ -q -I dts -O dtb \
		-o $(BINARIES_DIR)/styx-sensor-bridge-cm5.dtbo \
		$(STYX_BRIDGE_EXT_DIR)/dts/styx-sensor-bridge-cm5-overlay.dts
endef

LINUX_POST_INSTALL_IMAGES_HOOKS += STYX_BRIDGE_BUILD_OVERLAY

endif
