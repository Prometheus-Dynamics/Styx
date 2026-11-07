/* A C consumer of the Styx surface: a synthetic camera, five frames, controls refused cleanly. */
#include <stdio.h>
#include <string.h>

#include <errno.h>

#include "styx.h"

#define FOURCC(a, b, c, d) \
    ((uint32_t)(a) | ((uint32_t)(b) << 8) | ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))

static void sink(void *context, const uint8_t *text, size_t len) {
    (void)context;
    fprintf(stderr, "styx: %.*s\n", (int)len, (const char *)text);
}

int main(void) {
    const LemnosAbiVersion version = STYX_VERSION_INIT;
    if (styx_check_version(&version) != 0) {
        puts("version refused");
        return 1;
    }
    int32_t camera = styx_camera_add_virtual("c-camera", FOURCC('R', 'G', '2', '4'), 320, 180, 60);
    if (camera < 0) {
        printf("add_virtual: %d\n", camera);
        return 1;
    }
    char name[32];
    size_t len = 0;
    if (styx_camera_name((uint32_t)camera, name, sizeof name, &len) != 0 ||
        strcmp(name, "c-camera") != 0) {
        puts("name");
        return 1;
    }
    StyxStreamRequest request = {sizeof(StyxStreamRequest), FOURCC('R', 'G', '2', '4'), 320, 180, 0, 0};
    LemnosMessageSink messages = {NULL, sink};
    LemnosHandle stream = 0;
    int32_t status = styx_stream_open((uint32_t)camera, &request, &stream, messages);
    if (status != 0) {
        printf("open: %d\n", status);
        return 1;
    }
    int frames = 0;
    for (int tries = 0; tries < 50 && frames < 5; tries++) {
        LemnosHandle frame = 0;
        StyxFrameInfo info;
        memset(&info, 0, sizeof info);
        info.struct_size = sizeof info;
        status = styx_stream_next(stream, 200, &frame, &info);
        if (status == -ETIMEDOUT) {
            continue;
        }
        if (status != 0) {
            printf("next: %d\n", status);
            return 1;
        }
        const uint8_t *data = NULL;
        uint64_t bytes = 0, stride = 0;
        if (info.width != 320 || info.height != 180 ||
            styx_frame_plane(frame, 0, &data, &bytes, &stride) != 0 || data == NULL ||
            stride < 320 * 3 || bytes < stride * 179 + 320 * 3) {
            puts("frame");
            return 1;
        }
        if (styx_frame_release(frame) != 0 || styx_frame_release(frame) != LEMNOS_E_STALE_HANDLE) {
            puts("release");
            return 1;
        }
        frames++;
    }
    styx_stream_close(stream);
    printf("ok frames=%d\n", frames);
    return frames == 5 ? 0 : 1;
}
