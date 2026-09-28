/*
 * Generates the layout assertions in crates/pisp/src/uapi/layout.rs from the kernel headers
 * the device runs. Not part of the crate build: run ./run.sh <linux-source-tree> and diff.
 *
 * Built twice: -DFE against drivers/media/platform/raspberrypi/rp1_cfe/ (front end config and
 * statistics, the copies the rp1-cfe driver uses) and -DBE against
 * include/uapi/linux/media/raspberrypi/ (back end config).
 */
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>

#define SIZE(c, r) printf("const _: () = assert!(size_of::<%s>() == %zu);\n", #r, sizeof(struct c))
#define OFF(c, r, f, rf) \
	printf("const _: () = assert!(offset_of!(%s, %s) == %zu);\n", #r, #rf, offsetof(struct c, f))

#ifdef FE
typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
typedef int32_t s32;
typedef uint64_t u64;
#include "pisp_fe_config.h"
#include "pisp_statistics.h"

int main(void)
{
	printf("// Front end and statistics (rp1_cfe/pisp_fe_config.h, pisp_statistics.h).\n");
	SIZE(pisp_image_format_config, ImageFormatConfig);
	SIZE(pisp_bla_config, BlaConfig);
	SIZE(pisp_compress_config, CompressConfig);
	SIZE(pisp_decompress_config, DecompressConfig);
	SIZE(pisp_fe_global_config, FeGlobalConfig);
	SIZE(pisp_fe_input_config, FeInputConfig);
	SIZE(pisp_fe_output_config, FeOutputConfig);
	SIZE(pisp_fe_input_buffer_config, FeInputBufferConfig);
	SIZE(pisp_fe_decompand_config, FeDecompandConfig);
	SIZE(pisp_fe_dpc_config, FeDpcConfig);
	SIZE(pisp_fe_lsc_config, FeLscConfig);
	SIZE(pisp_fe_rgby_config, FeRgbyConfig);
	SIZE(pisp_fe_agc_stats_config, FeAgcStatsConfig);
	OFF(pisp_fe_agc_stats_config, FeAgcStatsConfig, row_offset_x, row_offset_x);
	OFF(pisp_fe_agc_stats_config, FeAgcStatsConfig, row_shift, row_shift);
	SIZE(pisp_fe_awb_stats_config, FeAwbStatsConfig);
	OFF(pisp_fe_awb_stats_config, FeAwbStatsConfig, r_lo, r_lo);
	SIZE(pisp_fe_floating_stats_config, FeFloatingStatsConfig);
	SIZE(pisp_fe_cdaf_stats_config, FeCdafStatsConfig);
	SIZE(pisp_fe_crop_config, FeCropConfig);
	SIZE(pisp_fe_downscale_config, FeDownscaleConfig);
	SIZE(pisp_fe_output_axi_config, FeOutputAxiConfig);
	SIZE(pisp_fe_output_branch_config, FeOutputBranchConfig);
	OFF(pisp_fe_output_branch_config, FeOutputBranchConfig, output, output);
	SIZE(pisp_fe_config, FeConfig);
#define FEOFF(f) OFF(pisp_fe_config, FeConfig, f, f)
	FEOFF(stats_buffer);
	FEOFF(output_buffer);
	FEOFF(input_buffer);
	FEOFF(global);
	FEOFF(input);
	FEOFF(decompress);
	FEOFF(decompand);
	FEOFF(bla);
	FEOFF(dpc);
	FEOFF(stats_crop);
	FEOFF(spare1);
	FEOFF(blc);
	FEOFF(rgby);
	FEOFF(lsc);
	FEOFF(agc_stats);
	FEOFF(awb_stats);
	FEOFF(cdaf_stats);
	FEOFF(floating_stats);
	FEOFF(output_axi);
	FEOFF(ch);
	FEOFF(dirty_flags);
	FEOFF(dirty_flags_extra);

	SIZE(pisp_agc_statistics_zone, RawAgcZone);
	SIZE(pisp_agc_statistics, RawAgcStatistics);
	OFF(pisp_agc_statistics, RawAgcStatistics, histogram, histogram);
	OFF(pisp_agc_statistics, RawAgcStatistics, floating, floating);
	SIZE(pisp_awb_statistics_zone, RawAwbZone);
	SIZE(pisp_awb_statistics, RawAwbStatistics);
	SIZE(pisp_cdaf_statistics, RawCdafStatistics);
	SIZE(pisp_statistics, RawStatistics);
	OFF(pisp_statistics, RawStatistics, agc, agc);
	OFF(pisp_statistics, RawStatistics, cdaf, cdaf);
	return 0;
}
#endif

#ifdef BE
#include "pisp_be_config.h"

int main(void)
{
	printf("// Back end (uapi/linux/media/raspberrypi/pisp_be_config.h, pisp_common.h).\n");
	SIZE(pisp_wbg_config, WbgConfig);
	SIZE(pisp_axi_config, AxiConfig);
	SIZE(pisp_be_global_config, BeGlobalConfig);
	SIZE(pisp_be_input_buffer_config, BeInputBufferConfig);
	SIZE(pisp_be_dpc_config, BeDpcConfig);
	SIZE(pisp_be_geq_config, BeGeqConfig);
	SIZE(pisp_be_tdn_config, BeTdnConfig);
	SIZE(pisp_be_sdn_config, BeSdnConfig);
	SIZE(pisp_be_stitch_config, BeStitchConfig);
	SIZE(pisp_be_cdn_config, BeCdnConfig);
	SIZE(pisp_be_lsc_config, BeLscConfig);
	SIZE(pisp_be_lsc_extra, BeLscExtra);
	SIZE(pisp_be_cac_config, BeCacConfig);
	SIZE(pisp_be_debin_config, BeDebinConfig);
	SIZE(pisp_be_tonemap_config, BeTonemapConfig);
	SIZE(pisp_be_demosaic_config, BeDemosaicConfig);
	SIZE(pisp_be_ccm_config, BeCcmConfig);
	OFF(pisp_be_ccm_config, BeCcmConfig, offsets, offsets);
	SIZE(pisp_be_sat_control_config, BeSatControlConfig);
	SIZE(pisp_be_false_colour_config, BeFalseColourConfig);
	SIZE(pisp_be_sharpen_config, BeSharpenConfig);
	OFF(pisp_be_sharpen_config, BeSharpenConfig, kernel4, kernel4);
	OFF(pisp_be_sharpen_config, BeSharpenConfig, threshold_offset0, thresholds);
	OFF(pisp_be_sharpen_config, BeSharpenConfig, positive_strength, positive_strength);
	OFF(pisp_be_sharpen_config, BeSharpenConfig, negative_limit, negative_limit);
	OFF(pisp_be_sharpen_config, BeSharpenConfig, enables, enables);
	SIZE(pisp_be_sh_fc_combine_config, BeShFcCombineConfig);
	SIZE(pisp_be_gamma_config, BeGammaConfig);
	SIZE(pisp_be_crop_config, BeCropConfig);
	SIZE(pisp_be_resample_config, BeResampleConfig);
	SIZE(pisp_be_resample_extra, BeResampleExtra);
	SIZE(pisp_be_downscale_config, BeDownscaleConfig);
	SIZE(pisp_be_downscale_extra, BeDownscaleExtra);
	SIZE(pisp_be_hog_config, BeHogConfig);
	SIZE(pisp_be_axi_config, BeAxiConfig);
	SIZE(pisp_be_output_format_config, BeOutputFormatConfig);
	SIZE(pisp_be_config, BeConfig);
#define BEOFF(f) OFF(pisp_be_config, BeConfig, f, f)
	BEOFF(global);
	BEOFF(input_format);
	BEOFF(decompress);
	BEOFF(dpc);
	BEOFF(geq);
	BEOFF(tdn_input_format);
	BEOFF(tdn_decompress);
	BEOFF(tdn);
	BEOFF(tdn_compress);
	BEOFF(tdn_output_format);
	BEOFF(sdn);
	BEOFF(blc);
	BEOFF(stitch_compress);
	BEOFF(stitch_output_format);
	BEOFF(stitch_input_format);
	BEOFF(stitch_decompress);
	BEOFF(stitch);
	BEOFF(lsc);
	BEOFF(wbg);
	BEOFF(cdn);
	BEOFF(cac);
	BEOFF(debin);
	BEOFF(tonemap);
	BEOFF(demosaic);
	BEOFF(ccm);
	BEOFF(sat_control);
	BEOFF(ycbcr);
	BEOFF(sharpen);
	BEOFF(false_colour);
	BEOFF(sh_fc_combine);
	BEOFF(ycbcr_inverse);
	BEOFF(gamma);
	BEOFF(csc);
	BEOFF(downscale);
	BEOFF(resample);
	BEOFF(output_format);
	BEOFF(hog);
	BEOFF(axi);
	BEOFF(pad1);
	SIZE(pisp_tile, Tile);
#define TOFF(f) OFF(pisp_tile, Tile, f, f)
	TOFF(input_addr_offset);
	TOFF(input_offset_x);
	TOFF(tdn_input_addr_offset);
	TOFF(lsc_grid_offset_x);
	TOFF(cac_grid_offset_x);
	TOFF(crop_x_start);
	TOFF(crop_y_end);
	TOFF(downscale_phase_x);
	TOFF(downscale_phase_y);
	TOFF(resample_in_width);
	TOFF(resample_phase_x);
	TOFF(resample_phase_y);
	TOFF(output_offset_x);
	TOFF(output_width);
	TOFF(output_height);
	TOFF(output_addr_offset);
	TOFF(output_addr_offset2);
	TOFF(output_hog_addr_offset);
	SIZE(pisp_be_tiles_config, BeTilesConfig);
	OFF(pisp_be_tiles_config, BeTilesConfig, tiles, tiles);
	OFF(pisp_be_tiles_config, BeTilesConfig, num_tiles, num_tiles);
	return 0;
}
#endif
