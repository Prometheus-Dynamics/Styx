################################################################################
#
# styx-pispbe: Styx's pisp_be back end driver, an out-of-tree module in updates/
#
# Built against the image's own kernel (Buildroot's kernel-module infrastructure:
# its config, Module.symvers and compiler), so it always matches the kernel it
# ships with. The kernel tree is only read.
#
################################################################################

STYX_PISPBE_VERSION = 1
STYX_PISPBE_SITE = $(BR2_EXTERNAL_STYX_KMODS_PATH)/../../pispbe
STYX_PISPBE_SITE_METHOD = local
STYX_PISPBE_LICENSE = GPL-2.0
STYX_PISPBE_MODULE_MAKE_OPTS = INSTALL_MOD_DIR=updates

# The kernel's own pisp_be sources the Styx driver was made from: rpi-7.2.y 53679a578
# (see kernel-modules/pispbe/README.md). The hash covers these files, concatenated in
# this order.
STYX_PISPBE_BASE_FILES = pisp_be.c pisp_be_formats.h
STYX_PISPBE_BASE_SHA256 = ed9762e653294770825c5e0e4c73895735fb8c043bf0c3c46e5876784e99584e

define STYX_PISPBE_CHECK_BASE
	@have="$$(cd $(LINUX_DIR)/drivers/media/platform/raspberrypi/pisp_be && \
		cat $(STYX_PISPBE_BASE_FILES) | sha256sum | cut -d' ' -f1)"; \
	if [ "$$have" != "$(STYX_PISPBE_BASE_SHA256)" ]; then \
		echo "styx-pispbe: this kernel's pisp_be sources ($$have) are not the ones the"; \
		echo "styx-pispbe: Styx driver was made from; re-import kernel-modules/pispbe"; \
		exit 1; \
	fi
endef
STYX_PISPBE_PRE_BUILD_HOOKS += STYX_PISPBE_CHECK_BASE

$(eval $(kernel-module))
$(eval $(generic-package))
