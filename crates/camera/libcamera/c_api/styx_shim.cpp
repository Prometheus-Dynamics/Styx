// Styx's accessors over the libcamera C shim (see styx_shim.h).
#include "styx_shim.h"
#include "camera.h"
#include "controls.h"
#include "request.h"

#include <libcamera/control_ids.h>
#include <libcamera/geometry.h>
#include <libcamera/pixel_format.h>

// The numbers the Rust side passes across the boundary (src/sys.rs) are libcamera's own. These
// checks fail the build if libcamera renumbers one, rather than letting a request or role mean
// something else. Checked against the 0.6.0 and 0.7.0 headers.
static_assert(static_cast<int>(libcamera::StreamRole::Raw) == 0, "sys.rs: role");
static_assert(static_cast<int>(libcamera::StreamRole::StillCapture) == 1, "sys.rs: role");
static_assert(static_cast<int>(libcamera::StreamRole::VideoRecording) == 2, "sys.rs: ROLE_VIDEO_RECORDING");
static_assert(static_cast<int>(libcamera::StreamRole::Viewfinder) == 3, "sys.rs: ROLE_VIEW_FINDER");
static_assert(static_cast<int>(libcamera::CameraConfiguration::Valid) == 0, "sys.rs: CONFIG_VALID");
static_assert(static_cast<int>(libcamera::CameraConfiguration::Adjusted) == 1, "sys.rs: CONFIG_ADJUSTED");
static_assert(static_cast<int>(libcamera::Request::RequestComplete) == 1, "sys.rs: REQUEST_STATUS_COMPLETE");
static_assert(static_cast<int>(libcamera::Request::RequestCancelled) == 2, "sys.rs: REQUEST_STATUS_CANCELLED");
static_assert(static_cast<int>(libcamera::Request::ReuseBuffers) == 1, "sys.rs: REUSE_BUFFERS");
static_assert(static_cast<int>(libcamera::ControlTypeBool) == 1, "sys.rs: CONTROL_TYPE_BOOL");
static_assert(static_cast<int>(libcamera::ControlTypeInteger32) == 5, "sys.rs: CONTROL_TYPE_INT32");
static_assert(static_cast<int>(libcamera::ControlTypeInteger64) == 6, "sys.rs: CONTROL_TYPE_INT64");
static_assert(static_cast<int>(libcamera::ControlTypeFloat) == 7, "sys.rs: CONTROL_TYPE_FLOAT");

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
