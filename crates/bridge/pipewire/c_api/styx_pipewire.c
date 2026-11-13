/*
 * The PipeWire video source node: a Video/Source stream whose buffers are the producer's memfds.
 *
 * The node takes buffer i to be the producer's ring buffer i (Rust learns the descriptors from the
 * first frames each index carries, and passes them in). A frame in ring buffer i is queued as
 * PipeWire buffer i, so a consumer maps the producer's pages; nothing is copied. The frame is held
 * until PipeWire returns that buffer, and the producer cannot reuse the buffer meanwhile.
 *
 * The node outlives its producer. Every 2 ms tick the shim asks Rust (`poll`) whether a producer's
 * buffers are new. When they are, the stream is disconnected and connected again with them: its
 * memfds are new, so the consumers must learn new buffers. Frames are pulled on the same tick and
 * never block: a frame whose buffer is still with a consumer is released at once and counted busy.
 *
 * The shim copies every descriptor it keeps (it duplicates them), so Rust's buffers may go on
 * return from `poll` or from `styx_pw_run`.
 */
#include <pipewire/pipewire.h>
#include <spa/buffer/buffer.h>
#include <spa/param/buffers.h>
#include <spa/param/param.h>
#include <spa/param/video/format-utils.h>
#include <spa/param/video/raw.h>
#include <spa/pod/builder.h>
#include <spa/utils/result.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* One frame as Rust hands it over. `token` is Rust's (slot + 1); `release` gives it back. */
typedef struct styx_pw_frame {
  uint32_t index; /* the ring buffer the frame is in */
  uint32_t len;   /* bytes of the frame */
  uint32_t stride;
  void *token;
} styx_pw_frame;

/* The buffers of one producer, and the stream's format. fds and lens are per buffer; the shim
 * duplicates the fds. */
typedef struct styx_pw_config {
  const char *name;
  uint32_t fourcc;
  uint32_t width, height;
  uint32_t fps_num, fps_den;
  uint32_t buffers; /* the ring's buffer count */
  uint32_t stride;  /* row stride of plane 0 */
  const int *fds;   /* per buffer: the memfd */
  const uint32_t *lens;
} styx_pw_config;

/* Rust's functions. `next` never blocks: 1 when `out` holds a frame, 0 when none. */
typedef struct styx_pw_ops {
  /* Every tick: 1 when new buffers are ready (`out` holds them), 0 otherwise. */
  int (*poll)(void *ctx, styx_pw_config *out);
  int (*next)(void *ctx, styx_pw_frame *out);
  /* Gives a frame back. busy: the consumer still held its buffer, so the frame was dropped. */
  void (*release)(void *ctx, void *token, int busy);
  int (*running)(void *ctx);
} styx_pw_ops;

typedef struct node {
  styx_pw_config cfg; /* the current buffers; fds and lens are the shim's copies */
  int *fds;           /* duplicates of the producer's memfds, one per buffer */
  uint32_t *lens;
  uint32_t count;     /* the length of the per-buffer arrays below */
  const styx_pw_ops *ops;
  void *ctx;
  struct pw_main_loop *loop;
  struct pw_stream *stream;
  struct spa_source *timer;
  struct pw_buffer **pw; /* by ring buffer index; NULL until added */
  void **token;          /* the frame queued in each buffer, NULL when none */
  uint8_t *avail;        /* 1 when we hold the buffer and may fill it */
  uint32_t n_avail;
  uint32_t added;
  int failed;
} node;

static uint32_t spa_format_for(uint32_t fourcc) {
  /* The four-character codes, as Styx's FourCc numbers them (little-endian bytes). */
  switch (fourcc) {
  case 0x56595559: return SPA_VIDEO_FORMAT_YUY2;  /* YUYV */
  case 0x59565955: return SPA_VIDEO_FORMAT_UYVY;  /* UYVY */
  case 0x3231564e: return SPA_VIDEO_FORMAT_NV12;  /* NV12 */
  case 0x3132564e: return SPA_VIDEO_FORMAT_NV21;  /* NV21 */
  case 0x30323449: return SPA_VIDEO_FORMAT_I420;  /* I420 */
  case 0x32315559: return SPA_VIDEO_FORMAT_I420;  /* YU12 */
  case 0x32315659: return SPA_VIDEO_FORMAT_YV12;  /* YV12 */
  case 0x59455247: return SPA_VIDEO_FORMAT_GRAY8; /* GREY */
  case 0x33324752: return SPA_VIDEO_FORMAT_RGB;   /* RG24 */
  case 0x33324742: return SPA_VIDEO_FORMAT_BGR;   /* BG24 */
  case 0x41424752: return SPA_VIDEO_FORMAT_RGBA;  /* RGBA */
  case 0x41524742: return SPA_VIDEO_FORMAT_BGRA;  /* BGRA */
  default: return SPA_VIDEO_FORMAT_UNKNOWN;
  }
}

/* 1 when the format has a PipeWire raw video name. Rust checks it before it hands buffers over. */
int styx_pw_format_known(uint32_t fourcc) {
  return spa_format_for(fourcc) != SPA_VIDEO_FORMAT_UNKNOWN;
}

/* Frees the per-buffer arrays and closes the shim's descriptors. */
static void free_buffers(node *n) {
  for (uint32_t i = 0; n->fds && i < n->count; i++) {
    if (n->fds[i] >= 0) {
      close(n->fds[i]);
    }
  }
  free(n->fds);
  free(n->lens);
  free(n->pw);
  free(n->token);
  free(n->avail);
  n->fds = NULL;
  n->lens = NULL;
  n->pw = NULL;
  n->token = NULL;
  n->avail = NULL;
  n->count = 0;
}

/* Takes a copy of `c`: its descriptors are duplicated and its lengths copied. 0 on success. */
static int adopt(node *n, const styx_pw_config *c) {
  if (c->buffers == 0) {
    return -1;
  }
  /* `count` is set only once every array exists: a remove event indexes them by it. */
  n->fds = malloc(sizeof *n->fds * c->buffers);
  n->lens = calloc(c->buffers, sizeof *n->lens);
  n->pw = calloc(c->buffers, sizeof *n->pw);
  n->token = calloc(c->buffers, sizeof *n->token);
  n->avail = calloc(c->buffers, sizeof *n->avail);
  if (!n->fds || !n->lens || !n->pw || !n->token || !n->avail) {
    free_buffers(n);
    return -1;
  }
  for (uint32_t i = 0; i < c->buffers; i++) {
    n->fds[i] = -1;
  }
  n->count = c->buffers;
  for (uint32_t i = 0; i < c->buffers; i++) {
    n->fds[i] = dup(c->fds[i]);
    if (n->fds[i] < 0) {
      return -1;
    }
    n->lens[i] = c->lens[i];
  }
  n->cfg = *c;
  n->cfg.fds = n->fds;
  n->cfg.lens = n->lens;
  n->n_avail = 0;
  n->added = 0;
  return 0;
}

/* Connects the stream with the current buffers. 0 on success, -2 for a format with no PipeWire
 * name, -3 when the connect fails. */
static int connect_stream(node *n) {
  uint32_t spa_format = spa_format_for(n->cfg.fourcc);
  if (spa_format == SPA_VIDEO_FORMAT_UNKNOWN) {
    return -2;
  }
  uint8_t pod_buf[2048];
  struct spa_pod_builder b = SPA_POD_BUILDER_INIT(pod_buf, sizeof pod_buf);
  struct spa_video_info_raw info;
  memset(&info, 0, sizeof info);
  info.format = spa_format;
  info.size.width = n->cfg.width;
  info.size.height = n->cfg.height;
  info.framerate.num = n->cfg.fps_num;
  info.framerate.denom = n->cfg.fps_den;
  const struct spa_pod *params[2];
  params[0] = spa_format_video_raw_build(&b, SPA_PARAM_EnumFormat, &info);

  struct spa_pod_frame frame;
  spa_pod_builder_push_object(&b, &frame, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers);
  spa_pod_builder_add(&b, SPA_PARAM_BUFFERS_buffers, "i", (int)n->cfg.buffers,
                      SPA_PARAM_BUFFERS_blocks, "i", 1, SPA_PARAM_BUFFERS_size, "i",
                      (int)n->lens[0], SPA_PARAM_BUFFERS_stride, "i", (int)n->cfg.stride,
                      SPA_PARAM_BUFFERS_dataType, "i", 1 << SPA_DATA_MemFd, NULL);
  params[1] = spa_pod_builder_pop(&b, &frame);

  if (pw_stream_connect(n->stream, PW_DIRECTION_OUTPUT, PW_ID_ANY, PW_STREAM_FLAG_ALLOC_BUFFERS,
                        params, 2) < 0) {
    return -3;
  }
  return 0;
}

/* PipeWire added buffer `buffer`: it is ring buffer n->added, and its memory is that buffer's
 * memfd. The buffer is ours to fill until we queue it. */
static void on_add_buffer(void *data, struct pw_buffer *buffer) {
  node *n = data;
  uint32_t index = n->added++;
  if (index >= n->count || !buffer->buffer || buffer->buffer->n_datas < 1) {
    n->failed = 1;
    return;
  }
  struct spa_data *d = &buffer->buffer->datas[0];
  if (!(d->type & (1u << SPA_DATA_MemFd))) {
    fprintf(stderr, "styx-pipewire: a consumer does not take memfd buffers\n");
    n->failed = 1;
    return;
  }
  int fd = dup(n->fds[index]);
  if (fd < 0) {
    n->failed = 1;
    return;
  }
  d->type = SPA_DATA_MemFd;
  d->flags = SPA_DATA_FLAG_READABLE;
  d->fd = fd;
  d->mapoffset = 0;
  d->maxsize = n->lens[index];
  d->data = NULL;
  buffer->user_data = (void *)(uintptr_t)index;
  n->pw[index] = buffer;
}

static void on_remove_buffer(void *data, struct pw_buffer *buffer) {
  node *n = data;
  uintptr_t index = (uintptr_t)buffer->user_data;
  if (index >= n->count) {
    return;
  }
  if (n->token[index]) {
    n->ops->release(n->ctx, n->token[index], 0);
    n->token[index] = NULL;
  }
  if (n->avail[index]) {
    n->avail[index] = 0;
    n->n_avail--;
  }
  n->pw[index] = NULL;
}

/* The producer's buffers changed: disconnect the stream (its buffers go, and the consumers
 * with them), then connect it again with the new ones. The frames of the old buffers were dropped
 * on the Rust side before `poll` returned, so the tokens are cleared here, not released. */
static int reconfigure(node *n, const styx_pw_config *c) {
  memset(n->avail, 0, n->count);
  memset(n->token, 0, sizeof *n->token * n->count);
  n->n_avail = 0;
  pw_stream_disconnect(n->stream);
  free_buffers(n);
  if (adopt(n, c) != 0) {
    return -1;
  }
  return connect_stream(n);
}

static void on_timer(void *data, uint64_t expirations) {
  node *n = data;
  (void)expirations;
  if (!n->ops->running(n->ctx)) {
    pw_main_loop_quit(n->loop);
    return;
  }
  styx_pw_config next;
  if (n->ops->poll(n->ctx, &next)) {
    if (reconfigure(n, &next) != 0) {
      n->failed = 1;
      pw_main_loop_quit(n->loop);
      return;
    }
  }
  /* Buffers PipeWire gives back are free: the consumer is done with the frame in them. */
  struct pw_buffer *b;
  while ((b = pw_stream_dequeue_buffer(n->stream)) != NULL) {
    uintptr_t index = (uintptr_t)b->user_data;
    if (index >= n->count) {
      continue;
    }
    if (n->token[index]) {
      n->ops->release(n->ctx, n->token[index], 0);
      n->token[index] = NULL;
    }
    if (!n->avail[index]) {
      n->avail[index] = 1;
      n->n_avail++;
    }
  }
  /* Only pull a frame when a buffer is free for it. */
  while (n->n_avail > 0) {
    styx_pw_frame f;
    if (!n->ops->next(n->ctx, &f)) {
      break;
    }
    if (f.index >= n->count || !n->avail[f.index] || !n->pw[f.index]) {
      n->ops->release(n->ctx, f.token, 1);
      continue;
    }
    struct pw_buffer *b2 = n->pw[f.index];
    struct spa_data *d = &b2->buffer->datas[0];
    d->chunk->offset = 0;
    d->chunk->size = f.len;
    d->chunk->stride = (int32_t)f.stride;
    d->chunk->flags = 0;
    n->token[f.index] = f.token;
    n->avail[f.index] = 0;
    n->n_avail--;
    pw_stream_queue_buffer(n->stream, b2);
  }
}

static const struct pw_stream_events stream_events = {
    PW_VERSION_STREAM_EVENTS,
    .add_buffer = on_add_buffer,
    .remove_buffer = on_remove_buffer,
};

/* Runs the node until `ops->running` says stop. Returns 0 when it ran, negative when it could
 * not start (-1 memory or loop, -2 format, -3 connect) or could not take a producer's new
 * buffers (-4). */
int styx_pw_run(const styx_pw_config *cfg, const styx_pw_ops *ops, void *ctx) {
  node n;
  memset(&n, 0, sizeof n);
  n.ops = ops;
  n.ctx = ctx;
  int rc = -2;
  if (spa_format_for(cfg->fourcc) == SPA_VIDEO_FORMAT_UNKNOWN) {
    return rc;
  }
  rc = -1;
  if (adopt(&n, cfg) != 0) {
    goto out;
  }
  pw_init(NULL, NULL);
  n.loop = pw_main_loop_new(NULL);
  if (!n.loop) {
    goto out;
  }
  struct pw_loop *loop = pw_main_loop_get_loop(n.loop);
  struct pw_properties *props = pw_properties_new(
      PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, "Source", PW_KEY_MEDIA_CLASS,
      "Video/Source", PW_KEY_MEDIA_ROLE, "Camera", PW_KEY_NODE_NAME, cfg->name,
      PW_KEY_NODE_DESCRIPTION, cfg->name, NULL);
  n.stream = pw_stream_new_simple(loop, cfg->name, props, &stream_events, &n);
  if (!n.stream) {
    goto out;
  }
  rc = connect_stream(&n);
  if (rc != 0) {
    goto out;
  }
  n.timer = pw_loop_add_timer(loop, on_timer, &n);
  struct timespec period = {0, 2 * 1000 * 1000};
  pw_loop_update_timer(loop, n.timer, &period, &period, false);
  rc = 0;
  pw_main_loop_run(n.loop);
  if (n.failed) {
    rc = -4;
  }

out:
  if (n.stream) {
    pw_stream_destroy(n.stream);
  }
  if (n.loop) {
    pw_main_loop_destroy(n.loop);
  }
  if (n.token) {
    for (uint32_t i = 0; i < n.count; i++) {
      if (n.token[i]) {
        ops->release(ctx, n.token[i], 0);
        n.token[i] = NULL;
      }
    }
  }
  free_buffers(&n);
  return rc;
}
