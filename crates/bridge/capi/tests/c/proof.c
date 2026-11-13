/*
 * The C proof: cameras, controls and frames from C, through styx.h, over one path.
 *
 *   proof local   <lemnos socket>     in process: styx-select on the Lemnos daemon
 *   proof service <control socket>    over the socket: the camera service's control and frame sockets
 *
 * Opens the named camera (argv[3]), sets its exposure from a frame a few frames ahead, checks that
 * the exposure lands, then reads frames and checks their ids run in order. Prints one PROOF line
 * with the numbers. Exits non-zero on the first failed check.
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "styx.h"

/* The Rust test passes the sizes it computes from the Rust structs; a header that drifts from
 * them fails to compile here. */
#ifndef STYX_EXPECT_CONTROL_VALUE
#error "build with -DSTYX_EXPECT_* from the Rust test (sizes of the structs)"
#endif
_Static_assert(sizeof(styx_control_value) == STYX_EXPECT_CONTROL_VALUE, "styx_control_value size");
_Static_assert(sizeof(styx_open_options) == STYX_EXPECT_OPEN_OPTIONS, "styx_open_options size");
_Static_assert(sizeof(styx_frame_info) == STYX_EXPECT_FRAME_INFO, "styx_frame_info size");
_Static_assert(sizeof(styx_plane) == STYX_EXPECT_PLANE, "styx_plane size");
_Static_assert(sizeof(styx_counter_snapshot) == STYX_EXPECT_COUNTER_SNAPSHOT, "styx_counter_snapshot size");

#define FRAMES 200
#define WAIT_MS 5000

static void fail(const char *what, styx_status status) {
    char buf[512];
    size_t len = 0;
    (void)styx_last_error((uint8_t *)buf, sizeof buf, &len);
    fprintf(stderr, "FAIL %s: %s (%d): %s\n", what, styx_status_name(status), (int)status, buf);
    exit(1);
}

static void must(styx_status status, const char *what) {
    if (status != STYX_OK) {
        fail(what, status);
    }
}

static void check(int ok, const char *what) {
    if (!ok) {
        fprintf(stderr, "FAIL check: %s\n", what);
        exit(1);
    }
}

/* The next frame, or NULL on timeout. Any other failure ends the proof. */
static styx_frame *next_frame(styx_camera *camera, uint32_t timeout_ms) {
    styx_frame *frame = NULL;
    styx_status status = styx_camera_next_frame(camera, timeout_ms, &frame);
    if (status == STYX_E_TIMEOUT) {
        return NULL;
    }
    must(status, "styx_camera_next_frame");
    return frame;
}

static styx_frame_info info_of(const styx_frame *frame) {
    styx_frame_info info;
    must(styx_frame_describe(frame, &info), "styx_frame_info");
    return info;
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: proof local|service <socket> <camera>\n");
        return 2;
    }
    const char *mode = argv[1];
    const char *path = argv[2];
    const char *wanted = argv[3];
    size_t path_len = strlen(path);
    size_t wanted_len = strlen(wanted);

    uint32_t major = 0, minor = 0;
    must(styx_version(&major, &minor), "styx_version");
    check(major == STYX_ABI_MAJOR && minor == STYX_ABI_MINOR, "the header and the library agree on the version");

    styx_source *source = NULL;
    if (strcmp(mode, "local") == 0) {
        must(styx_source_local((const uint8_t *)path, path_len, &source), "styx_source_local");
    } else if (strcmp(mode, "service") == 0) {
        must(styx_source_service((const uint8_t *)path, path_len, &source), "styx_source_service");
    } else {
        fprintf(stderr, "unknown mode %s\n", mode);
        return 2;
    }

    /* List, and find the camera by name. */
    size_t count = 0;
    must(styx_source_camera_count(source, &count), "styx_source_camera_count");
    check(count >= 1, "the source lists at least one camera");
    int found = 0;
    for (size_t i = 0; i < count && !found; i++) {
        char name[256];
        size_t len = 0;
        must(styx_source_camera_name(source, i, (uint8_t *)name, sizeof name, &len), "styx_source_camera_name");
        check(len < sizeof name, "a camera name fits the buffer");
        found = len == wanted_len && memcmp(name, wanted, len) == 0;
    }
    check(found, "the named camera is listed");
    printf("LISTED %s cameras=%zu\n", mode, count);

    /* A name that is not listed is refused, not opened. */
    styx_camera *camera = NULL;
    styx_status status = styx_camera_open(source, (const uint8_t *)"no-such-camera", 14, NULL, &camera);
    check(status == STYX_E_NOT_FOUND && camera == NULL, "an unlisted camera is not found");

    styx_open_options options = {0, 0, 60000, 10000, 1000};
    must(styx_camera_open(source, (const uint8_t *)wanted, wanted_len, &options, &camera), "styx_camera_open");

    /* Warm up: frames flow and their ids are in order. */
    uint64_t last_id = 0;
    for (int i = 0; i < 10; i++) {
        styx_frame *frame = next_frame(camera, WAIT_MS);
        check(frame != NULL, "a frame arrives within the wait");
        styx_frame_info info = info_of(frame);
        check(i == 0 || info.id > last_id, "warm-up ids rise");
        last_id = info.id;
        styx_frame_release(frame);
    }

    /* Exposure: set 5000 us from a frame a few ahead; it must show from there (within the sensor's
     * delay) and read back as set. */
    uint64_t target = last_id + 30;
    styx_control_value exposure = {STYX_VALUE_MANUAL, 5000};
    must(styx_camera_set_control(camera, (const uint8_t *)"exposure", 8, exposure, target), "set exposure");
    uint64_t seen_from = 0;
    int seen = 0;
    for (int i = 0; i < 400 && !seen; i++) {
        styx_frame *frame = next_frame(camera, WAIT_MS);
        check(frame != NULL, "a frame arrives while waiting for the exposure");
        styx_frame_info info = info_of(frame);
        check(info.id > last_id, "ids rise while waiting for the exposure");
        last_id = info.id;
        if (info.exposure_us == 5000) {
            seen = 1;
            seen_from = info.id;
        }
        styx_frame_release(frame);
    }
    check(seen, "exposure 5000 shows on a frame");
    check(seen_from >= target && seen_from <= target + 12, "exposure shows from the frame it was set for");
    styx_control_value read_back = {0, 0};
    must(styx_camera_get_control(camera, (const uint8_t *)"exposure", 8, &read_back), "get exposure");
    check(read_back.kind == STYX_VALUE_MANUAL && read_back.value == 5000, "exposure reads back as set");

    /* Frames: FRAMES of them, ids strictly in order, each with its planes and metadata. */
    uint64_t first = 0, prev = 0, gaps = 0;
    uint32_t memory_kind = 0;
    int descriptor = 0;
    uint32_t fps = 0;
    uint32_t fourcc = 0, width = 0, height = 0, planes = 0;
    size_t plane0_bytes = 0, meta_len = 0;
    uint32_t meta_fps = 0;
    for (int i = 0; i < FRAMES; i++) {
        styx_frame *frame = next_frame(camera, WAIT_MS);
        check(frame != NULL, "a frame arrives");
        styx_frame_info info = info_of(frame);
        if (i == 0) {
            first = info.id;
            fourcc = info.fourcc;
            width = info.width;
            height = info.height;
            planes = info.planes;
        } else {
            check(info.id > prev, "frame ids are strictly in order");
            gaps += info.id - prev - 1;
        }
        prev = info.id;
        fps = info.fps_milli;
        check(info.width == width && info.height == height, "frame size is steady");
        check(info.exposure_us >= 1, "frames carry an exposure");

        if (i == 0) {
            int32_t fd = -1;
            uint64_t offset = 0, dma_len = 0;
            uint32_t kind = 0;
            styx_status fds = styx_frame_fd(frame, &fd, &offset, &dma_len, &kind);
            if (fds == STYX_OK) {
                check(fd >= 0 && dma_len > 0 && kind == info.memory_kind, "a descriptor names the buffer");
                descriptor = 1;
                memory_kind = kind;
            } else {
                check(fds == STYX_E_UNSUPPORTED && info.memory_kind == STYX_MEMORY_HOST, "host memory has no descriptor");
                memory_kind = info.memory_kind;
            }
        }
        styx_plane plane = {NULL, 0, 0};
        must(styx_frame_plane(frame, 0, &plane), "styx_frame_plane 0");
        check(plane.data != NULL && plane.len > 0 && plane.stride > 0, "plane 0 has bytes and a stride");
        plane0_bytes = plane.len;
        styx_plane past = {NULL, 0, 0};
        check(styx_frame_plane(frame, info.planes, &past) == STYX_E_NOT_FOUND, "a plane past the last is not found");

        /* The record whole in a buffer of exactly its length, and again in a larger one: the two
         * must agree (a buffer that holds it whole is not cut). */
        /* The record's size comes from the library; a buffer of that size holds it whole, and a
         * buffer one byte short is refused with its length still reported. */
        size_t need = 0;
        must(styx_frame_metadata_size(&need), "styx_frame_metadata_size");
        check(need == STYX_METADATA_BYTES, "the header's metadata length is the library's");
        uint8_t *blob = malloc(need);
        check(blob != NULL, "a buffer for the metadata record");
        size_t blen = 0;
        must(styx_frame_metadata(frame, blob, need, &blen), "styx_frame_metadata");
        check(blen == need, "the metadata record is the library's length");
        size_t short_len = 0;
        check(styx_frame_metadata(frame, blob, need - 1, &short_len) == STYX_E_INVALID && short_len == need,
              "a short buffer is refused and the length reported");
        meta_len = blen;
        meta_fps = info.achieved_fps_milli;
        free(blob);
        styx_frame_release(frame);
    }
    check(prev > first, "the ids moved on");

    /* The counters decoder: a short record and a record that is not this layout are refused by
     * status (a valid record is in the Rust tests). */
    styx_counter_snapshot counters;
    check(styx_counters_read((const uint8_t *)"short", 5, &counters) == STYX_E_INVALID, "a short counters record is invalid");
    uint8_t junk[714];
    memset(junk, 0xab, sizeof junk);
    check(styx_counters_read(junk, sizeof junk, &counters) == STYX_E_PROTOCOL, "a counters record of another layout is refused");

    uint64_t copies = 0, copy_bytes = 0;
    must(styx_copies(&copies, &copy_bytes), "styx_copies");
    check(copies == 0 && copy_bytes == 0, "no frame bytes were copied");

    printf("PROOF %s frames=%d first_id=%" PRIu64 " last_id=%" PRIu64 " ordered=1 gaps=%" PRIu64
           " exposure_set_from=%" PRIu64 " exposure_seen_from=%" PRIu64 " exposure_read_back=%u"
           " fps_milli=%u fourcc=0x%08x size=%ux%u planes=%u plane0_bytes=%zu meta_bytes=%zu"
           " memory_kind=%u descriptor=%d meta_fps_milli=%u copies=%" PRIu64 " copy_bytes=%" PRIu64 "\n",
           mode, FRAMES, first, prev, gaps, target, seen_from, read_back.value, fps, fourcc, width, height,
           planes, plane0_bytes, meta_len, memory_kind, descriptor, meta_fps, copies, copy_bytes);

    styx_camera_close(camera);
    styx_source_close(source);
    return 0;
}
