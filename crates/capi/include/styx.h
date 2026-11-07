/*
 * Styx C surface, version 1.0: cameras, streams, frames and controls.
 *
 * Every call returns an int32_t status: 0 success, -1 to -4095 a negated Linux errno,
 * LEMNOS_E_* (-4096 and below) the shared codes, and a positive value a count where a call
 * says so. Call styx_check_version first with STYX_VERSION_INIT.
 *
 * Output structs start with struct_size: set it to sizeof the struct before the call. Cameras
 * are named by their index in the list styx_camera_count probed; streams and frames by
 * handles. A frame handle holds the frame (its buffer stays out of the capture's queue) until
 * styx_frame_release: release frames promptly. Taking and releasing frames allocates nothing
 * once as many have been held as are held at once.
 *
 * Link one form per process: libstyx_capi.a (static) or libstyx_capi.so.1 (shared).
 *
 * Kept in step with crates/capi/src/surface.rs; the crate's tests check this header's sizes,
 * offsets and layout hash.
 */
#ifndef STYX_H
#define STYX_H

#include <stddef.h>
#include <stdint.h>

/* Status codes, handles, buffer descriptions, the handshake and message sinks. */
#include "lemnos/abi.h"

#ifdef __cplusplus
extern "C" {
#endif

#define STYX_ABI_MAJOR 1
#define STYX_ABI_MINOR 0
#define STYX_LAYOUT_HASH 0x5c370710333db30eULL

#define STYX_VERSION_INIT \
    { sizeof(LemnosAbiVersion), STYX_ABI_MAJOR, STYX_ABI_MINOR, STYX_LAYOUT_HASH }

/* What timestamp_ns counts on. */
#define STYX_CLOCK_UNKNOWN 0
#define STYX_CLOCK_MONOTONIC 1
#define STYX_CLOCK_BOOTTIME 2
#define STYX_CLOCK_REALTIME 3
#define STYX_CLOCK_STREAM 4

/* StyxFrameInfo.flags. */
#define STYX_FRAME_DMABUF 1u /* styx_frame_buffer describes it */
#define STYX_FRAME_HOST 2u   /* styx_frame_plane maps it */

/* StyxControlValue.kind. */
#define STYX_CONTROL_NONE 0
#define STYX_CONTROL_BOOL 1
#define STYX_CONTROL_INT 2
#define STYX_CONTROL_UINT 3
#define STYX_CONTROL_FLOAT 4

/* Zero in a field: any. width and height: the largest frame wanted, both or neither. */
typedef struct StyxStreamRequest {
    uint32_t struct_size;
    uint32_t fourcc;
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t flags; /* none yet: 0 */
} StyxStreamRequest;

typedef struct StyxFrameInfo {
    uint32_t struct_size;
    uint32_t fourcc;
    uint32_t width;
    uint32_t height;
    uint32_t plane_count;
    uint32_t clock; /* STYX_CLOCK_* */
    uint32_t flags; /* STYX_FRAME_* */
    uint32_t reserved;
    uint64_t sequence; /* frames this stream delivered before this one */
    uint64_t timestamp_ns;
} StyxFrameInfo;

/* integer for BOOL, INT and UINT; real for FLOAT. */
typedef struct StyxControlValue {
    uint32_t struct_size;
    uint32_t kind;
    int64_t integer;
    double real;
} StyxControlValue;

LemnosAbiVersion styx_version(void);
int32_t styx_check_version(const LemnosAbiVersion *caller);
LemnosFeatures styx_features(void);

/* Probes the cameras: their count (a positive status). */
int32_t styx_camera_count(void);
/* Adds a synthetic camera (a moving test pattern) for tests without hardware: its index (a
 * non-negative status). */
int32_t styx_camera_add_virtual(const char *name, uint32_t fourcc, uint32_t width,
                                uint32_t height, uint32_t fps);
/* NUL-terminated into name (capacity bytes); *len its length without the NUL.
 * LEMNOS_E_BUFFER_TOO_SMALL (nothing copied, *len written) when it does not fit. */
int32_t styx_camera_name(uint32_t index, char *name, size_t capacity, size_t *len);

int32_t styx_stream_open(uint32_t index, const StyxStreamRequest *request, LemnosHandle *stream,
                         LemnosMessageSink sink);
/* Frames not released stay valid until they are. */
int32_t styx_stream_close(LemnosHandle stream);
/* -ETIMEDOUT when no frame came within timeout_ms, -EPIPE when the stream ended. info may be
 * NULL. */
int32_t styx_stream_next(LemnosHandle stream, uint32_t timeout_ms, LemnosHandle *frame,
                         StyxFrameInfo *info);

/* The frame's dma-bufs; fds borrowed, valid until styx_frame_release. -EOPNOTSUPP for a frame
 * in host memory only. */
int32_t styx_frame_buffer(LemnosHandle frame, LemnosBufferDesc *desc);
/* A plane's bytes in this process, valid until styx_frame_release. */
int32_t styx_frame_plane(LemnosHandle frame, uint32_t plane, const uint8_t **data, uint64_t *len,
                         uint64_t *stride);
int32_t styx_frame_release(LemnosHandle frame);

int32_t styx_control_get(LemnosHandle stream, uint32_t id, StyxControlValue *value,
                         LemnosMessageSink sink);
int32_t styx_control_set(LemnosHandle stream, uint32_t id, const StyxControlValue *value,
                         LemnosMessageSink sink);

#ifdef __cplusplus
}
#endif

#endif /* STYX_H */
