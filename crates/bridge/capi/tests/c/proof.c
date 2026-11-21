/*
 * The minimal C caller of styx.h: the header compiles as C, its structs are the library's sizes,
 * and a C program links the library, opens a camera, reads frames in order, sees an error as a
 * status, and closes. Everything deeper (controls, planes, descriptors, metadata, counters) is
 * checked through the same entry points from Rust, in tests/c_proof.rs.
 *
 *   proof local   <lemnos socket>   <camera>    in process
 *   proof service <control socket>  <camera>    over the camera service's sockets
 */
#include <inttypes.h>
#include <stdio.h>
#include <string.h>

#include "styx.h"

/* The Rust test passes the sizes of the Rust structs; a header that drifts fails to compile. */
#ifndef STYX_EXPECT_CONTROL_VALUE
#error "build with -DSTYX_EXPECT_* from the Rust test (sizes of the structs)"
#endif
_Static_assert(sizeof(styx_control_value) == STYX_EXPECT_CONTROL_VALUE, "styx_control_value size");
_Static_assert(sizeof(styx_open_options) == STYX_EXPECT_OPEN_OPTIONS, "styx_open_options size");
_Static_assert(sizeof(styx_frame_info) == STYX_EXPECT_FRAME_INFO, "styx_frame_info size");
_Static_assert(sizeof(styx_plane) == STYX_EXPECT_PLANE, "styx_plane size");
_Static_assert(sizeof(styx_counter_snapshot) == STYX_EXPECT_COUNTER_SNAPSHOT, "styx_counter_snapshot size");
_Static_assert(sizeof(styx_control_info) == STYX_EXPECT_CONTROL_INFO, "styx_control_info size");
_Static_assert(sizeof(styx_device_info) == STYX_EXPECT_DEVICE_INFO, "styx_device_info size");

#define FRAMES 30

static int failed(const char *what, styx_status status) {
    char text[256];
    size_t len = 0;
    (void)styx_last_error((uint8_t *)text, sizeof text - 1, &len);
    text[len < sizeof text ? len : sizeof text - 1] = 0;
    fprintf(stderr, "FAIL %s: %s (%d): %s\n", what, styx_status_name(status), (int)status, text);
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: proof local|service <socket> <camera>\n");
        return 2;
    }
    const uint8_t *path = (const uint8_t *)argv[2];
    const uint8_t *name = (const uint8_t *)argv[3];
    uint32_t major = 0, minor = 0;
    styx_status status = styx_version(&major, &minor);
    if (status != STYX_OK || major != STYX_ABI_MAJOR || minor != STYX_ABI_MINOR) {
        return failed("the header and the library agree on the version", status);
    }

    styx_source *source = NULL;
    status = strcmp(argv[1], "local") == 0 ? styx_source_local(path, strlen(argv[2]), &source)
                                           : styx_source_service(path, strlen(argv[2]), &source);
    if (status != STYX_OK) {
        return failed("open the source", status);
    }

    /* The error path: an unlisted camera is a status, and no handle. */
    styx_camera *camera = NULL;
    status = styx_camera_open(source, (const uint8_t *)"no-such-camera", 14, NULL, &camera);
    if (status != STYX_E_NOT_FOUND || camera != NULL) {
        return failed("an unlisted camera is not found", status);
    }

    styx_open_options options = {0, 0, 60000, 10000, 1000};
    status = styx_camera_open(source, name, strlen(argv[3]), &options, &camera);
    if (status != STYX_OK) {
        return failed("open the camera", status);
    }
    uint64_t first = 0, last = 0;
    for (int i = 0; i < FRAMES; i++) {
        styx_frame *frame = NULL;
        styx_frame_info info;
        status = styx_camera_next_frame(camera, 5000, &frame);
        if (status == STYX_OK) {
            status = styx_frame_describe(frame, &info);
        }
        if (status != STYX_OK || (i > 0 && info.id <= last)) {
            return failed("a frame, after the one before it", status);
        }
        first = i == 0 ? info.id : first;
        last = info.id;
        styx_frame_release(frame);
    }
    printf("PROOF %s frames=%d first_id=%" PRIu64 " last_id=%" PRIu64 " ordered=1\n", argv[1], FRAMES, first, last);
    styx_camera_close(camera);
    styx_source_close(source);
    return 0;
}
