################################################################################
#
# styx-rp1-cfe: Styx's rp1-cfe-downstream receiver driver, an out-of-tree module
# in updates/
#
# Built against the image's own kernel (Buildroot's kernel-module infrastructure:
# its config, Module.symvers and compiler), so it always matches the kernel it
# ships with. The kernel tree is only read.
#
################################################################################

STYX_RP1_CFE_VERSION = 1
STYX_RP1_CFE_SITE = $(BR2_EXTERNAL_STYX_KMODS_PATH)/../../rp1-cfe
STYX_RP1_CFE_SITE_METHOD = local
STYX_RP1_CFE_LICENSE = GPL-2.0
STYX_RP1_CFE_MODULE_MAKE_OPTS = INSTALL_MOD_DIR=updates

# The kernel's own rp1_cfe sources the Styx driver was made from: rpi-7.2.y 53679a578
# (see kernel-modules/rp1-cfe/README.md). The hash covers these files, concatenated in
# this order.
STYX_RP1_CFE_BASE_FILES = cfe.c cfe.h cfe_fmts.h csi2.c csi2.h dphy.c dphy.h \
	pisp_common.h pisp_fe.c pisp_fe.h pisp_fe_config.h pisp_statistics.h pisp_types.h
STYX_RP1_CFE_BASE_SHA256 = 578084f9d2241ec6c346b06aa5f9fc11a8b46684254f9dc3405ccc2f5a78dd0a

define STYX_RP1_CFE_CHECK_BASE
	@have="$$(cd $(LINUX_DIR)/drivers/media/platform/raspberrypi/rp1_cfe && \
		cat $(STYX_RP1_CFE_BASE_FILES) | sha256sum | cut -d' ' -f1)"; \
	if [ "$$have" != "$(STYX_RP1_CFE_BASE_SHA256)" ]; then \
		echo "styx-rp1-cfe: this kernel's rp1_cfe sources ($$have) are not the ones the"; \
		echo "styx-rp1-cfe: Styx driver was made from; re-import kernel-modules/rp1-cfe"; \
		exit 1; \
	fi
endef
STYX_RP1_CFE_PRE_BUILD_HOOKS += STYX_RP1_CFE_CHECK_BASE

$(eval $(kernel-module))
$(eval $(generic-package))
