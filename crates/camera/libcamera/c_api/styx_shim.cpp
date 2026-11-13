// Styx's accessors over the libcamera C shim (see styx_shim.h).
#include "styx_shim.h"

#include <libcamera/control_ids.h>
#include <libcamera/geometry.h>
#include <libcamera/pixel_format.h>

extern "C" {

uint32_t styx_stream_fourcc(const libcamera_stream_configuration_t *config) {
    return config->pixelFormat.fourcc();
}

uint64_t styx_stream_modifier(const libcamera_stream_configuration_t *config) {
    return config->pixelFormat.modifier();
}

unsigned int styx_stream_width(const libcamera_stream_configuration_t *config) {
    return config->size.width;
}

unsigned int styx_stream_height(const libcamera_stream_configuration_t *config) {
    return config->size.height;
}

unsigned int styx_stream_stride(const libcamera_stream_configuration_t *config) {
    return config->stride;
}

unsigned int styx_stream_frame_size(const libcamera_stream_configuration_t *config) {
    return config->frameSize;
}

unsigned int styx_stream_buffer_count(const libcamera_stream_configuration_t *config) {
    return config->bufferCount;
}

void styx_stream_set_format(libcamera_stream_configuration_t *config, uint32_t fourcc, uint64_t modifier, unsigned int width, unsigned int height) {
    config->pixelFormat = libcamera::PixelFormat(fourcc, modifier);
    config->size = libcamera::Size(width, height);
}

void styx_stream_set_buffer_count(libcamera_stream_configuration_t *config, unsigned int count) {
    config->bufferCount = count;
}

unsigned int styx_control_id(unsigned int which) {
    switch (which) {
    case 0: return libcamera::controls::ExposureTime.id();
    case 1: return libcamera::controls::AnalogueGain.id();
    case 2: return libcamera::controls::DigitalGain.id();
    case 3: return libcamera::controls::SensorTimestamp.id();
    case 4: return libcamera::controls::AeEnable.id();
    case 5: return libcamera::controls::FrameDuration.id();
    default: return 0;
    }
}

}
