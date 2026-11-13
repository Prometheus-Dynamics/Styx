// Styx's accessors over the libcamera C shim: stream configuration fields and the control ids
// the backend reads and writes. Vendored C shim (c_api/) is from libcamera-sys 0.7 (MIT OR
// Apache-2.0, lit-robotics); this file is Styx's.
#ifndef STYX_LIBCAMERA_SHIM_H
#define STYX_LIBCAMERA_SHIM_H

#include <stdbool.h>
#include <stdint.h>
#include "stream.h"

#ifdef __cplusplus
extern "C" {
#endif

uint32_t styx_stream_fourcc(const libcamera_stream_configuration_t *config);
uint64_t styx_stream_modifier(const libcamera_stream_configuration_t *config);
unsigned int styx_stream_width(const libcamera_stream_configuration_t *config);
unsigned int styx_stream_height(const libcamera_stream_configuration_t *config);
unsigned int styx_stream_stride(const libcamera_stream_configuration_t *config);
unsigned int styx_stream_frame_size(const libcamera_stream_configuration_t *config);
unsigned int styx_stream_buffer_count(const libcamera_stream_configuration_t *config);
void styx_stream_set_format(libcamera_stream_configuration_t *config, uint32_t fourcc, uint64_t modifier, unsigned int width, unsigned int height);
void styx_stream_set_buffer_count(libcamera_stream_configuration_t *config, unsigned int count);

/* Control ids by Styx's name: 0 ExposureTime, 1 AnalogueGain, 2 DigitalGain, 3 SensorTimestamp,
   4 AeEnable, 5 FrameDuration. Any other value returns 0 (no control). */
unsigned int styx_control_id(unsigned int which);
/* Property ids by Styx's name: 0 ScalerCropMaximum. Any other value returns 0. */
unsigned int styx_property_id(unsigned int which);

#ifdef __cplusplus
}
#endif

#endif
