################################################################################
#
# styx-ov9282: Styx's ov9282 driver, an out-of-tree module in updates/
#
# Built against the image's own kernel (Buildroot's kernel-module infrastructure:
# its config, Module.symvers and compiler), so it always matches the kernel it
# ships with. The kernel tree is only read.
#
################################################################################

STYX_OV9282_VERSION = 1
STYX_OV9282_SITE = $(BR2_EXTERNAL_STYX_KMODS_PATH)/..
STYX_OV9282_SITE_METHOD = local
STYX_OV9282_LICENSE = GPL-2.0
STYX_OV9282_MODULE_MAKE_OPTS = INSTALL_MOD_DIR=updates

# The kernel's own ov9282.c the Styx driver was made from: rpi-7.2.y 53679a578 with
# the Raze image's two OV9782 patches (see kernel-modules/ov9282/README.md).
STYX_OV9282_BASE_SHA256 = 2e148740bc18e49454cd38396c614851cc5462d85234f2108755dee9c0c25c9c

define STYX_OV9282_CHECK_BASE
	@have="$$(sha256sum $(LINUX_DIR)/drivers/media/i2c/ov9282.c | cut -d' ' -f1)"; \
	if [ "$$have" != "$(STYX_OV9282_BASE_SHA256)" ]; then \
		echo "styx-ov9282: this kernel's ov9282.c ($$have) is not the one the Styx"; \
		echo "styx-ov9282: driver was made from; re-import kernel-modules/ov9282"; \
		exit 1; \
	fi
endef
STYX_OV9282_PRE_BUILD_HOOKS += STYX_OV9282_CHECK_BASE

$(eval $(kernel-module))
$(eval $(generic-package))
