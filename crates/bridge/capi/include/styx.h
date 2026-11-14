/*
 * styx.h: Styx's C surface, version 1.0 (STYX_ABI_MAJOR 1, STYX_ABI_MINOR 0).
 *
 * Cameras, controls and frames, in process (a Lemnos daemon on a socket, through styx-select) or
 * over the camera service's sockets (its control socket and each camera's frame socket).
 *
 * Rules every call follows:
 *  - Every function returns a styx_status: STYX_OK (0) or a negative STYX_E_* code.
 *    styx_last_error gives the message of this thread's last failure.
 *  - Results come back through out-pointers, written only when the call returns STYX_OK.
 *  - Handles are opaque. A NULL handle, a NULL out-pointer, a NULL buffer with a length, a
 *    non-UTF-8 name, an unknown enum value and an out-of-range index are refused with a status.
 *    A dangling or foreign non-NULL pointer is not detectable: passing one is undefined.
 *  - Text in: (pointer, length), not NUL-terminated. Text out: a caller buffer of `cap` bytes and
 *    a `size_t *len` that receives the full length; the buffer gets at most cap-1 bytes and a NUL
 *    when cap is not zero. A caller finds a cut text by comparing *len with cap.
 *  - No call panics across this boundary: a panic is STYX_E_PANIC.
 *  - Frames are borrowed until styx_frame_release. Plane pointers and the descriptor's fd are
 *    valid only until then. Hold a few frames at a time: the service cuts off a socket reader that
 *    holds more than four.
 *  - Threads: a source may be used from any thread. A camera or a frame is used from one thread
 *    at a time (no call on a camera runs while another call on it runs); a frame may be released
 *    from any thread once no other call is using it.
 *  - Time: frame timestamps are CLOCK_MONOTONIC nanoseconds. Frame rates are milli-frames per
 *    second.
 *
 * Compile with: cc -std=c11 -I include prog.c libstyx_capi.a -lpthread -ldl -lm
 */
#ifndef STYX_H
#define STYX_H

#include <stddef.h>
#include <stdint.h>

#define STYX_ABI_MAJOR 1
#define STYX_ABI_MINOR 0

#ifdef __cplusplus
extern "C" {
#endif

/* Statuses. */
typedef int32_t styx_status;
#define STYX_OK 0
#define STYX_E_INVALID (-1)     /* NULL or out-of-range argument, bad text, unknown enum value */
#define STYX_E_NOT_FOUND (-2)   /* no such camera, control or index; or no frame has arrived yet */
#define STYX_E_IO (-3)          /* a socket, the Lemnos daemon or a device call failed */
#define STYX_E_TIMEOUT (-4)     /* no frame within the timeout */
#define STYX_E_UNSUPPORTED (-5) /* this path or backend does not do it */
#define STYX_E_BUSY (-6)        /* the camera is not publishing frames (yet) */
#define STYX_E_PROTOCOL (-7)    /* the other end answered in a form this build does not know */
#define STYX_E_PANIC (-8)       /* an internal failure, caught at the boundary */
#define STYX_E_STOPPED (-9)     /* the camera stopped or faulted: open it again */
#define STYX_E_RANGE (-10)      /* a control value outside its range */
#define STYX_E_LAYOUT (-11)     /* a frame socket's shared ring or marks page is another layout:
                                   styx_layout_refusal gives which, found and expected */

/* The structure a STYX_E_LAYOUT refusal names (styx_layout_refusal's first value). */
#define STYX_LAYOUT_RING 1  /* the frame ring's header */
#define STYX_LAYOUT_MARKS 2 /* a reader's marks page */

/* Handles. Each is freed by its own close or release call. */
typedef struct styx_source styx_source;
typedef struct styx_camera styx_camera;
typedef struct styx_frame styx_frame;

/* Control values: kind STYX_VALUE_AUTO (value ignored), STYX_VALUE_MANUAL (value in the control's
 * unit), or STYX_VALUE_FLAG (value 0 or 1). Crops are not settable or readable here. */
#define STYX_VALUE_AUTO 0
#define STYX_VALUE_MANUAL 1
#define STYX_VALUE_FLAG 2

typedef struct styx_control_value {
    uint32_t kind;
    uint32_t value;
} styx_control_value;

/* Open options. Zero means the default (exposure 10000 us, gain 1000). The in-process path needs
 * fps_milli (frames per second in thousandths; the sensor mode must reach it). Width and height
 * are the smallest the mode may be (0 accepts any). Over the service, the service's config
 * decides and these are ignored. */
typedef struct styx_open_options {
    uint32_t width;
    uint32_t height;
    uint32_t fps_milli;
    uint32_t exposure_us;
    uint32_t gain_milli;
} styx_open_options;

/* The metadata record's length in this library (styx-controls WIRE_BYTES). styx_frame_metadata_size
 * reports the same number at run time: allocate from that, not from this define. */
#define STYX_METADATA_BYTES 378

/* Memory kinds, as styx_frame_info.memory_kind and styx_frame_fd's kind. */
#define STYX_MEMORY_HOST 0
#define STYX_MEMORY_MEMFD 1
#define STYX_MEMORY_DMABUF 2
#define STYX_MEMORY_GPU 3

/* A frame's identity, format and metadata. fourcc is the FourCC as a u32 (little-endian bytes,
 * for example 'R' 'G' 'G' 'B' in the low byte first). fps_milli is the achieved rate since the
 * camera opened (0 until two frames). exposure_us and analogue_gain_milli are the values the frame
 * was captured with. */
typedef struct styx_frame_info {
    uint64_t id;
    uint64_t timestamp_ns;
    uint32_t fourcc;
    uint32_t width;
    uint32_t height;
    uint32_t planes;
    uint32_t fps_milli;
    uint32_t exposure_us;
    uint32_t analogue_gain_milli;
    uint32_t memory_kind;
    uint32_t achieved_fps_milli; /* the producer's achieved rate from the metadata (0 if unknown) */
} styx_frame_info;

/* One plane: its bytes, length and row stride, valid until the frame is released. */
typedef struct styx_plane {
    const uint8_t *data;
    size_t len;
    size_t stride;
} styx_plane;

/* ---- Version and status ---------------------------------------------------------------- */

/* Thread-safe. */
styx_status styx_version(uint32_t *major, uint32_t *minor);

/* Thread-safe. A static string for a status, or NULL for an unknown status. */
const char *styx_status_name(styx_status status);

/* Thread-safe (per thread). Copies this thread's last failure message. */
styx_status styx_last_error(uint8_t *buf, size_t cap, size_t *len);

/* Thread-safe (per thread). After a STYX_E_LAYOUT failure: which structure (STYX_LAYOUT_RING or
   STYX_LAYOUT_MARKS), the layout value in the peer's memory and the one this build reads. Any
   other failure, or none, is STYX_E_NOT_FOUND. Each out-pointer may be NULL. */
styx_status styx_layout_refusal(int32_t *which, uint64_t *found, uint64_t *expected);

/* ---- Sources --------------------------------------------------------------------------- */

/* The in-process source: cameras on the Lemnos daemon at the socket path (len bytes, not NUL-
 * terminated). Not thread-safe: one call on a source at a time. */
styx_status styx_source_local(const uint8_t *path, size_t len, styx_source **out);

/* The service source: the cameras styx-service serves on its control socket path. Not thread-
 * safe: one call on a source at a time. */
styx_status styx_source_service(const uint8_t *path, size_t len, styx_source **out);

/* Frees a source (NULL is a no-op). Cameras opened from it stay open. Not thread-safe. */
void styx_source_close(styx_source *source);

/* The number of cameras now listed. Lists the source (a connection, one listing). Not thread-safe. */
styx_status styx_source_camera_count(const styx_source *source, size_t *count);

/* Camera `index`'s name. Each call lists again. Not thread-safe. */
styx_status styx_source_camera_name(const styx_source *source, size_t index, uint8_t *buf,
                                    size_t cap, size_t *len);

/* ---- Cameras --------------------------------------------------------------------------- */

/* Opens camera `name` (len bytes) from the source; options may be NULL. Opening waits up to ten
 * seconds for a camera that is not publishing yet. Not thread-safe with other calls on the same
 * source. */
styx_status styx_camera_open(const styx_source *source, const uint8_t *name, size_t len,
                             const styx_open_options *options, styx_camera **out);

/* Closes a camera: its stream stops and its device is released (NULL is a no-op). Not thread-
 * safe with other calls on the same camera. */
void styx_camera_close(styx_camera *camera);

/* Sets control `name` (as styx-controls names it: exposure, gain, frame_rate, and so on) to
 * `value`, applied from frame `frame` on. Not thread-safe with other calls on the same camera. */
styx_status styx_camera_set_control(styx_camera *camera, const uint8_t *name, size_t len,
                                    styx_control_value value, uint64_t frame);

/* Reads control `name`'s value in force on the camera's newest frame (on the service path, the
 * service's value for the newest frame). Not thread-safe with other calls on the same camera. */
styx_status styx_camera_get_control(styx_camera *camera, const uint8_t *name, size_t len,
                                    styx_control_value *out);

/* Waits up to timeout_ms for the next frame. On STYX_OK, *out is a frame to release. Returns
 * STYX_E_TIMEOUT when none came. Not thread-safe with other calls on the same camera. */
styx_status styx_camera_next_frame(styx_camera *camera, uint32_t timeout_ms, styx_frame **out);

/* ---- Frames ---------------------------------------------------------------------------- */

/* A frame's identity, format and metadata. Thread-safe with other frames; one thread per frame. */
styx_status styx_frame_describe(const styx_frame *frame, styx_frame_info *out);

/* Plane `index`. STYX_E_NOT_FOUND past the last plane; STYX_E_UNSUPPORTED when the memory is not
 * mapped for the CPU. Thread-safe with other frames. */
styx_status styx_frame_plane(const styx_frame *frame, uint32_t index, styx_plane *out);

/* The size of the metadata record: what styx_frame_metadata writes. Thread-safe. */
styx_status styx_frame_metadata_size(size_t *len);

/* The metadata record in styx-controls' wire form (STYX_METADATA_BYTES long, version 2). buf must
 * hold it whole, or the call is STYX_E_INVALID; *len receives the record's length in any case.
 * Thread-safe with other frames. */
styx_status styx_frame_metadata(const styx_frame *frame, uint8_t *buf, size_t cap, size_t *len);

/* The frame's descriptor, for a zero-copy import elsewhere: fd (borrowed, valid until the frame
 * is released), the buffer's offset and length in it, and its memory kind. STYX_E_UNSUPPORTED for
 * host memory, which has none. Thread-safe with other frames. */
styx_status styx_frame_fd(const styx_frame *frame, int32_t *fd, uint64_t *offset, uint64_t *len,
                          uint32_t *kind);

/* Gives the frame back (NULL is a no-op). Its planes and descriptor are invalid after this. */
void styx_frame_release(styx_frame *frame);

/* ---- Counters -------------------------------------------------------------------------- */

/* One camera's counters, decoded from styx-counters' record. Codes (plain integers):
 *   owner:   0 none, 1 native, 2 libcamera, 3 v4l2
 *   state:   0 absent, 1 streaming, 2 retrying, 3 backoff, 4 stopped, 5 off
 *   health:  0 unknown, 1 healthy, 2 degraded, 3 failed
 *   publish: 0 waiting, 1 publishing, 2 not publishable
 * drops[i] is the count for drop reason i: 0 pool_exhausted, 1 held_buffer, 2 ring_full,
 * 3 backend_error, 4 unattributed. The record's text fields (names and reasons) are not here. */
typedef struct styx_counter_snapshot {
    uint64_t seq;
    uint64_t taken_ns;
    uint32_t owner;
    uint32_t state;
    uint32_t health;
    uint32_t publish;
    uint64_t frames;
    uint64_t last_id;
    uint64_t last_timestamp_ns;
    uint64_t published;
    uint64_t ring_full;
    uint32_t readers;
    uint64_t drops[5];
    uint64_t copies;
    uint64_t copy_bytes;
    uint64_t age_count;
    uint64_t age_min_ns;
    uint64_t age_median_ns;
    uint64_t age_max_ns;
    uint32_t achieved_fps_milli;
    uint64_t window_span_ns;
    uint32_t window_frames;
    uint32_t requested_fps_milli;
    uint32_t next_delay_ms;
    uint32_t failures;
    uint64_t total_failures;
} styx_counter_snapshot;

/* Decodes one counters record of `len` bytes into out. STYX_E_INVALID for a NULL buffer, a
 * short record, or a NULL out; STYX_E_PROTOCOL for bytes that are not this layout (magic,
 * version, length, an unknown code). Thread-safe. */
styx_status styx_counters_read(const uint8_t *bytes, size_t len, styx_counter_snapshot *out);

/* ---- Copies --------------------------------------------------------------------------- */

/* The frame copies this process has made and their bytes: zero on the zero-copy paths. Thread-
 * safe. */
styx_status styx_copies(uint64_t *copies, uint64_t *bytes);

#ifdef __cplusplus
}
#endif

#endif /* STYX_H */
