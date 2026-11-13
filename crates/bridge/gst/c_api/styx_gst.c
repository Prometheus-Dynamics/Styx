/*
 * The styxsrc element: a GstPushSrc that delivers Styx frames from the shared ring.
 *
 * Rust does the frame work (connecting to the producer, holding the frame, reconnecting with
 * backoff) and reaches this file through the `styx_gst_ops` table it passes at plugin init. This
 * file only turns each frame into a GstBuffer: the buffer's memory is the frame's own memory,
 * wrapped (memfd, mapped read-only) or exported as a dma-buf (`GstDmaBufAllocator`) when the frame
 * has a descriptor. Nothing is copied. The frame is released when GStreamer frees that memory.
 */
#include <gst/gst.h>
#include <gst/base/gstpushsrc.h>
#include <gst/video/video.h>
#ifdef STYX_HAVE_ALLOCATORS
#include <gst/allocators/allocators.h>
#endif
#include <stdint.h>
#include <string.h>
#include <unistd.h>

/* One frame as Rust describes it. `token` is Rust's; `release` gives it back. */
typedef struct styx_gst_frame {
  const uint8_t *data; /* the whole buffer, mapped; NULL when not mapped */
  size_t len;          /* bytes in the whole buffer */
  int fd;              /* a descriptor of the buffer, -1 when none */
  const char *format;  /* GStreamer video format name, e.g. "NV12" */
  uint32_t width, height;
  uint32_t fps_num, fps_den; /* 0/1 when variable */
  uint64_t pts_ns;
  uint32_t planes;
  size_t offset[4];
  int32_t stride[4];
  void *token;
} styx_gst_frame;

/* Rust's functions, handed over once at plugin init. */
typedef struct styx_gst_ops {
  void *(*open)(const char *socket);
  void (*close)(void *source);
  /* 1 when `out` holds a frame; 0 when none arrived within `timeout_ms`. */
  int (*next)(void *source, styx_gst_frame *out, uint32_t timeout_ms);
  void (*release)(void *token);
} styx_gst_ops;

static const styx_gst_ops *g_ops = NULL;
static GQuark g_token_quark = 0;

#define STYX_TYPE_SRC (styx_src_get_type())
#define STYX_SRC(obj) (G_TYPE_CHECK_INSTANCE_CAST((obj), STYX_TYPE_SRC, StyxSrc))

typedef struct _StyxSrc {
  GstPushSrc parent;
  gchar *socket;
  void *source;
  gint flushing;
  /* The caps last set, to set them again only when the format changes. */
  gchar *caps_format;
  uint32_t caps_width, caps_height, caps_fps_num, caps_fps_den;
#ifdef STYX_HAVE_ALLOCATORS
  GstAllocator *dmabuf;
#endif
} StyxSrc;

typedef struct _StyxSrcClass {
  GstPushSrcClass parent_class;
} StyxSrcClass;

G_DEFINE_TYPE(StyxSrc, styx_src, GST_TYPE_PUSH_SRC)

enum { PROP_0, PROP_SOCKET };

static GstStaticPadTemplate g_src_template = GST_STATIC_PAD_TEMPLATE(
    "src", GST_PAD_SRC, GST_PAD_ALWAYS, GST_STATIC_CAPS("video/x-raw"));

/* Runs when GStreamer frees the memory that holds a frame. */
static void styx_token_released(gpointer token) {
  if (g_ops && token) {
    g_ops->release(token);
  }
}

static void styx_src_set_property(GObject *object, guint id, const GValue *value,
                                  GParamSpec *pspec) {
  StyxSrc *src = STYX_SRC(object);
  switch (id) {
  case PROP_SOCKET:
    g_free(src->socket);
    src->socket = g_value_dup_string(value);
    break;
  default:
    G_OBJECT_WARN_INVALID_PROPERTY_ID(object, id, pspec);
  }
}

static void styx_src_get_property(GObject *object, guint id, GValue *value, GParamSpec *pspec) {
  StyxSrc *src = STYX_SRC(object);
  switch (id) {
  case PROP_SOCKET:
    g_value_set_string(value, src->socket);
    break;
  default:
    G_OBJECT_WARN_INVALID_PROPERTY_ID(object, id, pspec);
  }
}

static gboolean styx_src_start(GstBaseSrc *base) {
  StyxSrc *src = STYX_SRC(base);
  if (!g_ops) {
    GST_ELEMENT_ERROR(src, LIBRARY, INIT, ("styx: plugin not initialised"), (NULL));
    return FALSE;
  }
  if (!src->socket) {
    GST_ELEMENT_ERROR(src, RESOURCE, SETTINGS, ("the socket property is not set"), (NULL));
    return FALSE;
  }
  src->source = g_ops->open(src->socket);
  g_atomic_int_set(&src->flushing, 0);
  return src->source != NULL;
}

static gboolean styx_src_stop(GstBaseSrc *base) {
  StyxSrc *src = STYX_SRC(base);
  if (src->source) {
    g_ops->close(src->source);
    src->source = NULL;
  }
  g_clear_pointer(&src->caps_format, g_free);
  return TRUE;
}

static gboolean styx_src_unlock(GstBaseSrc *base) {
  g_atomic_int_set(&STYX_SRC(base)->flushing, 1);
  return TRUE;
}

static gboolean styx_src_unlock_stop(GstBaseSrc *base) {
  g_atomic_int_set(&STYX_SRC(base)->flushing, 0);
  return TRUE;
}

/* Sets the element's caps when the frame's format, size or rate differs from the last. */
static void styx_src_update_caps(StyxSrc *src, const styx_gst_frame *f, GstVideoFormat format) {
  if (src->caps_format && strcmp(src->caps_format, f->format) == 0 &&
      src->caps_width == f->width && src->caps_height == f->height &&
      src->caps_fps_num == f->fps_num && src->caps_fps_den == f->fps_den) {
    return;
  }
  GstVideoInfo info;
  gst_video_info_set_format(&info, format, f->width, f->height);
  if (f->fps_num > 0 && f->fps_den > 0) {
    info.fps_n = f->fps_num;
    info.fps_d = f->fps_den;
  }
  GstCaps *caps = gst_video_info_to_caps(&info);
  gst_base_src_set_caps(GST_BASE_SRC(src), caps);
  gst_caps_unref(caps);
  g_free(src->caps_format);
  src->caps_format = g_strdup(f->format);
  src->caps_width = f->width;
  src->caps_height = f->height;
  src->caps_fps_num = f->fps_num;
  src->caps_fps_den = f->fps_den;
}

static GstFlowReturn styx_src_create(GstPushSrc *push, GstBuffer **out) {
  StyxSrc *src = STYX_SRC(push);
  styx_gst_frame f;
  for (;;) {
    if (g_atomic_int_get(&src->flushing)) {
      return GST_FLOW_FLUSHING;
    }
    if (!src->source) {
      return GST_FLOW_ERROR;
    }
    if (g_ops->next(src->source, &f, 50)) {
      break;
    }
  }
  GstVideoFormat format = gst_video_format_from_string(f.format);
  if (format == GST_VIDEO_FORMAT_UNKNOWN || f.planes == 0 || f.planes > 4) {
    g_ops->release(f.token);
    GST_ELEMENT_ERROR(src, STREAM, FORMAT, ("frame format %s is not supported", f.format), (NULL));
    return GST_FLOW_ERROR;
  }
  styx_src_update_caps(src, &f, format);

  GstMemory *memory = NULL;
#ifdef STYX_HAVE_ALLOCATORS
  if (f.fd >= 0) {
    if (!src->dmabuf) {
      src->dmabuf = gst_dmabuf_allocator_new();
    }
    int fd = dup(f.fd);
    if (fd >= 0) {
      /* The allocator takes the descriptor; the memory maps it when a CPU reads it. */
      memory = gst_dmabuf_allocator_alloc(src->dmabuf, fd, f.len);
    }
  }
#endif
  if (!memory) {
    memory = gst_memory_new_wrapped(GST_MEMORY_FLAG_READONLY, (gpointer)f.data, f.len, 0, f.len,
                                    NULL, NULL);
  }
  if (!memory) {
    g_ops->release(f.token);
    return GST_FLOW_ERROR;
  }
  if (!g_token_quark) {
    g_token_quark = g_quark_from_static_string("styx-frame-token");
  }
  /* The frame lives as long as this memory does. */
  gst_mini_object_set_qdata(GST_MINI_OBJECT(memory), g_token_quark, f.token, styx_token_released);

  GstBuffer *buffer = gst_buffer_new();
  gst_buffer_append_memory(buffer, memory);
  gsize offsets[4];
  gint strides[4];
  for (uint32_t i = 0; i < f.planes; i++) {
    offsets[i] = f.offset[i];
    strides[i] = f.stride[i];
  }
  gst_buffer_add_video_meta_full(buffer, GST_VIDEO_FRAME_FLAG_NONE, format, f.width, f.height,
                                 f.planes, offsets, strides);
  GST_BUFFER_PTS(buffer) = f.pts_ns;
  if (f.fps_num > 0 && f.fps_den > 0) {
    GST_BUFFER_DURATION(buffer) = gst_util_uint64_scale(GST_SECOND, f.fps_den, f.fps_num);
  }
  *out = buffer;
  return GST_FLOW_OK;
}

static void styx_src_finalize(GObject *object) {
  StyxSrc *src = STYX_SRC(object);
  g_free(src->socket);
  g_free(src->caps_format);
#ifdef STYX_HAVE_ALLOCATORS
  g_clear_object(&src->dmabuf);
#endif
  G_OBJECT_CLASS(styx_src_parent_class)->finalize(object);
}

static void styx_src_class_init(StyxSrcClass *klass) {
  GObjectClass *gobject = G_OBJECT_CLASS(klass);
  GstElementClass *element = GST_ELEMENT_CLASS(klass);
  GstBaseSrcClass *base = GST_BASE_SRC_CLASS(klass);
  GstPushSrcClass *push = GST_PUSH_SRC_CLASS(klass);

  gobject->set_property = styx_src_set_property;
  gobject->get_property = styx_src_get_property;
  gobject->finalize = styx_src_finalize;
  g_object_class_install_property(
      gobject, PROP_SOCKET,
      g_param_spec_string("socket", "Socket",
                          "Unix socket of the Styx producer (the shared ring)", NULL,
                          G_PARAM_READWRITE | GST_PARAM_MUTABLE_READY | G_PARAM_STATIC_STRINGS));

  gst_element_class_set_static_metadata(element, "Styx frame source", "Source/Video",
                                        "Frames from a Styx camera over the shared ring, "
                                        "without copying",
                                        "Styx");
  gst_element_class_add_static_pad_template(element, &g_src_template);

  base->start = styx_src_start;
  base->stop = styx_src_stop;
  base->unlock = styx_src_unlock;
  base->unlock_stop = styx_src_unlock_stop;
  push->create = styx_src_create;
}

static void styx_src_init(StyxSrc *src) {
  gst_base_src_set_live(GST_BASE_SRC(src), TRUE);
  gst_base_src_set_format(GST_BASE_SRC(src), GST_FORMAT_TIME);
  g_atomic_int_set(&src->flushing, 0);
}

/* Called by the plugin's init with Rust's table. Registers `styxsrc`. */
gboolean styx_gst_plugin_init(GstPlugin *plugin, const styx_gst_ops *ops) {
  g_ops = ops;
  return gst_element_register(plugin, "styxsrc", GST_RANK_NONE, STYX_TYPE_SRC);
}
