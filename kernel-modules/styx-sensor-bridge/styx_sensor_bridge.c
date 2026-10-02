// SPDX-License-Identifier: GPL-2.0
/*
 * Styx sensor bridge.
 *
 * A generic V4L2 subdevice that stands in for a camera sensor driver. The
 * sensor itself is driven from userspace (Styx) over i2c-dev; this module
 * only describes the CSI-2 link to the receiver, carries the pad format and
 * timing controls userspace sets, and forwards the receiver's stream on/off
 * requests to userspace as V4L2 events, waiting (bounded) for userspace to
 * acknowledge that the sensor started or stopped. It never touches the I2C
 * bus. See PROTOCOL.md.
 */

#include <linux/clk.h>
#include <linux/completion.h>
#include <linux/i2c.h>
#include <linux/jiffies.h>
#include <linux/module.h>
#include <linux/mod_devicetable.h>
#include <linux/of.h>
#include <linux/platform_device.h>
#include <linux/property.h>
#include <linux/regulator/consumer.h>
#include <linux/sched.h>
#include <linux/spinlock.h>

#include <media/v4l2-async.h>
#include <media/v4l2-ctrls.h>
#include <media/v4l2-event.h>
#include <media/v4l2-fwnode.h>
#include <media/mipi-csi2.h>
#include <media/v4l2-subdev.h>

#include "styx_sensor_bridge.h"

/* How long a stop waits for an acknowledgement when the stopping process exits. */
#define STYX_EXITING_WAIT_MS 50

/*
 * rp1-cfe (Raspberry Pi 6.12) oopses when the sensor's s_stream(1) fails while
 * its front end is unused: its error path stops CSI-2 channel -1
 * (cfe_stop_channel(node, true) with fe_csi2_channel = -1). By default a failed
 * start is therefore not reported to the receiver; see
 * STYX_BRIDGE_STATE_START_FAILED.
 */
static bool report_start_errors;
module_param(report_start_errors, bool, 0644);
MODULE_PARM_DESC(report_start_errors,
		 "Return failed starts to the receiver (default: no, report them in STYX_CID_STREAM_STATE)");

#define STYX_MAX_CODES		32
#define STYX_MAX_SUPPLIES	8
#define STYX_DEFAULT_TIMEOUT_MS	1000
#define STYX_EVENT_QUEUE_DEPTH	4

/* Pads: the image, and embedded data when the device tree asks for it. */
#define STYX_PAD_IMAGE		0
#define STYX_PAD_EMBEDDED	1
#define STYX_EMBEDDED_MAX_WIDTH	65536
#define STYX_EMBEDDED_MAX_LINES	16

/* Accepted when the device tree gives no styx,mbus-codes. */
static const u32 styx_default_codes[] = {
	MEDIA_BUS_FMT_SBGGR10_1X10, MEDIA_BUS_FMT_SGBRG10_1X10,
	MEDIA_BUS_FMT_SGRBG10_1X10, MEDIA_BUS_FMT_SRGGB10_1X10,
	MEDIA_BUS_FMT_SBGGR12_1X12, MEDIA_BUS_FMT_SGBRG12_1X12,
	MEDIA_BUS_FMT_SGRBG12_1X12, MEDIA_BUS_FMT_SRGGB12_1X12,
	MEDIA_BUS_FMT_SBGGR8_1X8, MEDIA_BUS_FMT_SGBRG8_1X8,
	MEDIA_BUS_FMT_SGRBG8_1X8, MEDIA_BUS_FMT_SRGGB8_1X8,
	MEDIA_BUS_FMT_SBGGR14_1X14, MEDIA_BUS_FMT_SGBRG14_1X14,
	MEDIA_BUS_FMT_SGRBG14_1X14, MEDIA_BUS_FMT_SRGGB14_1X14,
	MEDIA_BUS_FMT_SBGGR16_1X16, MEDIA_BUS_FMT_SGBRG16_1X16,
	MEDIA_BUS_FMT_SGRBG16_1X16, MEDIA_BUS_FMT_SRGGB16_1X16,
	MEDIA_BUS_FMT_Y8_1X8, MEDIA_BUS_FMT_Y10_1X10, MEDIA_BUS_FMT_Y12_1X12,
	MEDIA_BUS_FMT_Y14_1X14, MEDIA_BUS_FMT_Y16_1X16,
};

struct styx_bridge {
	struct device *dev;
	struct v4l2_subdev sd;
	struct media_pad pads[2];
	unsigned int num_pads;

	struct v4l2_ctrl_handler hdl;
	struct v4l2_ctrl *link_freq;
	struct v4l2_ctrl *pixel_rate;
	struct v4l2_ctrl *hblank;
	struct v4l2_ctrl *vblank;
	struct v4l2_ctrl *timeout;
	struct v4l2_ctrl *power;

	/* From the device tree. */
	u32 codes[STYX_MAX_CODES];
	unsigned int num_codes;
	u32 min_width, min_height, max_width, max_height;
	s64 *link_freqs;
	unsigned int num_link_freqs;
	unsigned int data_lanes;
	unsigned int bus_flags;
	const char *sensor_name;
	int i2c_bus;
	u32 i2c_address;
	/* Embedded data pad (styx,embedded-data): default size and data type. */
	u32 emb_width, emb_lines;
	u8 emb_dt;

	struct clk *clk;
	struct regulator_bulk_data supplies[STYX_MAX_SUPPLIES];
	unsigned int num_supplies;
	bool powered;

	/* Serialises s_stream. */
	struct mutex stream_lock;
	/* Protects seq, pending_seq, waiting, ack_status. */
	spinlock_t ack_lock;
	struct completion ack_done;
	u32 seq;
	u32 pending_seq;
	bool waiting;
	int ack_status;

	atomic_t state;
	atomic_t listeners;
};

static inline struct styx_bridge *to_bridge(struct v4l2_subdev *sd)
{
	return container_of(sd, struct styx_bridge, sd);
}

static unsigned int styx_code_bpp(u32 code)
{
	switch (code) {
	case MEDIA_BUS_FMT_SBGGR8_1X8: case MEDIA_BUS_FMT_SGBRG8_1X8:
	case MEDIA_BUS_FMT_SGRBG8_1X8: case MEDIA_BUS_FMT_SRGGB8_1X8:
	case MEDIA_BUS_FMT_Y8_1X8:
		return 8;
	case MEDIA_BUS_FMT_SBGGR12_1X12: case MEDIA_BUS_FMT_SGBRG12_1X12:
	case MEDIA_BUS_FMT_SGRBG12_1X12: case MEDIA_BUS_FMT_SRGGB12_1X12:
	case MEDIA_BUS_FMT_Y12_1X12:
		return 12;
	case MEDIA_BUS_FMT_SBGGR14_1X14: case MEDIA_BUS_FMT_SGBRG14_1X14:
	case MEDIA_BUS_FMT_SGRBG14_1X14: case MEDIA_BUS_FMT_SRGGB14_1X14:
	case MEDIA_BUS_FMT_Y14_1X14:
		return 14;
	case MEDIA_BUS_FMT_SBGGR16_1X16: case MEDIA_BUS_FMT_SGBRG16_1X16:
	case MEDIA_BUS_FMT_SGRBG16_1X16: case MEDIA_BUS_FMT_SRGGB16_1X16:
	case MEDIA_BUS_FMT_Y16_1X16:
		return 16;
	default:
		return 10;
	}
}

/* CSI-2 data type of a raw or mono bus code (by bit depth). */
static u8 styx_code_dt(u32 code)
{
	switch (styx_code_bpp(code)) {
	case 8:
		return MIPI_CSI2_DT_RAW8;
	case 12:
		return MIPI_CSI2_DT_RAW12;
	case 14:
		return MIPI_CSI2_DT_RAW14;
	case 16:
		return MIPI_CSI2_DT_RAW16;
	default:
		return MIPI_CSI2_DT_RAW10;
	}
}

static bool styx_code_supported(struct styx_bridge *b, u32 code)
{
	unsigned int i;

	for (i = 0; i < b->num_codes; i++)
		if (b->codes[i] == code)
			return true;
	return false;
}

/* ------------------------------------------------------------------------
 * Power: the supplies and clock named in the device tree, if any.
 */

static int styx_bridge_set_power(struct styx_bridge *b, bool on)
{
	int ret;

	if (on == b->powered)
		return 0;

	if (!on && atomic_read(&b->state) != STYX_BRIDGE_STATE_IDLE)
		return -EBUSY;

	if (on) {
		ret = regulator_bulk_enable(b->num_supplies, b->supplies);
		if (ret)
			return ret;
		ret = clk_prepare_enable(b->clk);
		if (ret) {
			regulator_bulk_disable(b->num_supplies, b->supplies);
			return ret;
		}
	} else {
		clk_disable_unprepare(b->clk);
		regulator_bulk_disable(b->num_supplies, b->supplies);
	}
	b->powered = on;
	return 0;
}

/* ------------------------------------------------------------------------
 * Stream handshake with userspace.
 */

/* Completes a pending wait with @status. Caller holds ack_lock. */
static void styx_bridge_finish_locked(struct styx_bridge *b, int status)
{
	if (!b->waiting)
		return;
	b->waiting = false;
	b->ack_status = status;
	complete(&b->ack_done);
}

static void styx_bridge_abort(struct styx_bridge *b, int status)
{
	unsigned long flags;

	spin_lock_irqsave(&b->ack_lock, flags);
	styx_bridge_finish_locked(b, status);
	spin_unlock_irqrestore(&b->ack_lock, flags);
}

static int styx_bridge_ack(struct styx_bridge *b, u64 value)
{
	u32 seq = lower_32_bits(value);
	u32 status = upper_32_bits(value);
	unsigned long flags;
	int ret = 0;

	/* Sequence 0 is never used: lets the default value be written. */
	if (!seq)
		return 0;

	spin_lock_irqsave(&b->ack_lock, flags);
	if (!b->waiting || seq != b->pending_seq)
		ret = -ESTALE;
	else
		styx_bridge_finish_locked(b, status ? -(int)min(status, 4095U) : 0);
	spin_unlock_irqrestore(&b->ack_lock, flags);

	return ret;
}

static void styx_bridge_fill_event(struct styx_bridge *b, u32 action, u32 seq,
				   u32 timeout_ms, struct v4l2_event *ev)
{
	struct styx_bridge_stream_event *p = (void *)ev->u.data;
	struct v4l2_subdev_state *state;
	struct v4l2_mbus_framefmt *fmt;
	s32 link_index;

	BUILD_BUG_ON(sizeof(*p) != sizeof(ev->u.data));

	memset(ev, 0, sizeof(*ev));
	ev->type = STYX_BRIDGE_EVENT_STREAM;
	ev->id = 0;

	p->version = STYX_BRIDGE_PROTOCOL_VERSION;
	p->action = action;
	p->sequence = seq;
	p->timeout_ms = timeout_ms;

	link_index = v4l2_ctrl_g_ctrl(b->link_freq);
	if (link_index >= 0 && link_index < b->num_link_freqs)
		p->link_freq = b->link_freqs[link_index];
	p->pixel_rate = v4l2_ctrl_g_ctrl_int64(b->pixel_rate);
	p->hblank = v4l2_ctrl_g_ctrl(b->hblank);
	p->vblank = v4l2_ctrl_g_ctrl(b->vblank);
	p->data_lanes = b->data_lanes;
	if (!(b->bus_flags & V4L2_MBUS_CSI2_NONCONTINUOUS_CLOCK))
		p->flags |= STYX_BRIDGE_FLAG_CONTINUOUS_CLOCK;

	state = v4l2_subdev_lock_and_get_active_state(&b->sd);
	fmt = v4l2_subdev_state_get_format(state, STYX_PAD_IMAGE);
	p->code = fmt->code;
	p->width = fmt->width;
	p->height = fmt->height;
	v4l2_subdev_unlock_state(state);
}

/*
 * Queues a stream event and waits for userspace to acknowledge it. Called
 * with stream_lock held and no other bridge lock, so the acknowledgement
 * (a control write on the subdev node) can run meanwhile.
 */
static int styx_bridge_request(struct styx_bridge *b, u32 action,
			       enum styx_bridge_stream_state transient)
{
	struct video_device *vdev = b->sd.devnode;
	unsigned long flags;
	struct v4l2_event ev;
	u32 timeout_ms, wait_ms, seq;
	long left;
	int ret;

	if (!vdev)
		return -ENODEV;
	if (!atomic_read(&b->listeners)) {
		dev_err(b->dev, "no userspace sensor driver is listening\n");
		return -ENOTCONN;
	}

	timeout_ms = v4l2_ctrl_g_ctrl(b->timeout);
	wait_ms = timeout_ms;
	/*
	 * A process killed while streaming stops the receiver from its exit path
	 * (releasing its mapped buffers), before its descriptors close: its own
	 * sensor driver, which would acknowledge, is already gone. Do not hold
	 * the receiver for the whole timeout; another process serving the bridge
	 * still has a short window to answer.
	 */
	if (current->flags & PF_EXITING)
		wait_ms = min_t(u32, timeout_ms, STYX_EXITING_WAIT_MS);

	spin_lock_irqsave(&b->ack_lock, flags);
	seq = ++b->seq;
	if (!seq)
		seq = ++b->seq;
	b->pending_seq = seq;
	b->waiting = true;
	b->ack_status = -ETIMEDOUT;
	reinit_completion(&b->ack_done);
	spin_unlock_irqrestore(&b->ack_lock, flags);

	atomic_set(&b->state, transient);
	styx_bridge_fill_event(b, action, seq, timeout_ms, &ev);
	v4l2_event_queue(vdev, &ev);

	left = wait_for_completion_killable_timeout(&b->ack_done,
						    msecs_to_jiffies(wait_ms));

	spin_lock_irqsave(&b->ack_lock, flags);
	if (b->waiting) {
		/* No acknowledgement: timed out or killed. */
		b->waiting = false;
		ret = left < 0 ? (int)left : -ETIMEDOUT;
	} else {
		ret = b->ack_status;
	}
	spin_unlock_irqrestore(&b->ack_lock, flags);

	if (ret)
		dev_err(b->dev, "%s request %u failed: %d\n",
			action == STYX_BRIDGE_ACTION_START ? "start" : "stop",
			seq, ret);
	return ret;
}

static int styx_bridge_start(struct styx_bridge *b)
{
	int ret;

	if (atomic_read(&b->state) != STYX_BRIDGE_STATE_IDLE)
		return -EBUSY;

	ret = styx_bridge_request(b, STYX_BRIDGE_ACTION_START,
				  STYX_BRIDGE_STATE_STARTING);
	if (ret && !READ_ONCE(report_start_errors)) {
		dev_warn(b->dev, "start failed (%d): reported to userspace only\n",
			 ret);
		atomic_set(&b->state, STYX_BRIDGE_STATE_START_FAILED);
		return 0;
	}
	if (ret) {
		atomic_set(&b->state, STYX_BRIDGE_STATE_IDLE);
		return ret;
	}

	v4l2_ctrl_grab(b->link_freq, true);
	v4l2_ctrl_grab(b->pixel_rate, true);
	atomic_set(&b->state, STYX_BRIDGE_STATE_STREAMING);
	return 0;
}

static int styx_bridge_stop(struct styx_bridge *b)
{
	int ret;

	/* The sensor never started: nothing to ask userspace. */
	if (atomic_cmpxchg(&b->state, STYX_BRIDGE_STATE_START_FAILED,
			   STYX_BRIDGE_STATE_IDLE) == STYX_BRIDGE_STATE_START_FAILED)
		return 0;
	if (atomic_read(&b->state) != STYX_BRIDGE_STATE_STREAMING)
		return 0;

	ret = styx_bridge_request(b, STYX_BRIDGE_ACTION_STOP,
				  STYX_BRIDGE_STATE_STOPPING);
	/* The receiver stops regardless; the stream is over either way. */
	atomic_set(&b->state, STYX_BRIDGE_STATE_IDLE);
	v4l2_ctrl_grab(b->link_freq, false);
	v4l2_ctrl_grab(b->pixel_rate, false);
	return ret;
}

/*
 * Switches the supplies and clock off when no userspace driver is left (its
 * process died) and the stream is idle, so a crash does not leave the sensor
 * powered. Runs when the last listener goes and when a stream ends without
 * one; whichever comes last does it.
 */
static void styx_bridge_orphan_power_off(struct styx_bridge *b)
{
	if (atomic_read(&b->listeners) ||
	    atomic_read(&b->state) != STYX_BRIDGE_STATE_IDLE)
		return;
	if (!v4l2_ctrl_g_ctrl(b->power))
		return;
	dev_info(b->dev, "no userspace sensor driver left: power off\n");
	if (v4l2_ctrl_s_ctrl(b->power, 0))
		dev_warn(b->dev, "power off failed\n");
}

static int styx_bridge_s_stream(struct v4l2_subdev *sd, int enable)
{
	struct styx_bridge *b = to_bridge(sd);
	int ret;

	mutex_lock(&b->stream_lock);
	ret = enable ? styx_bridge_start(b) : styx_bridge_stop(b);
	mutex_unlock(&b->stream_lock);
	if (enable)
		return ret;
	if (ret && (current->flags & PF_EXITING) && v4l2_ctrl_g_ctrl(b->power)) {
		/*
		 * The sensor driver died with the sensor streaming: power it off
		 * before the receiver closes its side, or the receiver does not
		 * see the next start (measured on rp1-cfe).
		 */
		dev_info(b->dev, "sensor driver exited while streaming: power off\n");
		if (v4l2_ctrl_s_ctrl(b->power, 0))
			dev_warn(b->dev, "power off failed\n");
	}
	styx_bridge_orphan_power_off(b);
	return ret;
}

/* ------------------------------------------------------------------------
 * Event subscription: count listeners so a start fails fast without one.
 */

static int styx_listener_add(struct v4l2_subscribed_event *sev,
			     unsigned int elems)
{
	struct styx_bridge *b = to_bridge(vdev_to_v4l2_subdev(sev->fh->vdev));

	atomic_inc(&b->listeners);
	return 0;
}

static void styx_listener_del(struct v4l2_subscribed_event *sev)
{
	struct styx_bridge *b = to_bridge(vdev_to_v4l2_subdev(sev->fh->vdev));

	if (atomic_dec_and_test(&b->listeners)) {
		styx_bridge_abort(b, -ENOTCONN);
		styx_bridge_orphan_power_off(b);
	}
}

static const struct v4l2_subscribed_event_ops styx_listener_ops = {
	.add = styx_listener_add,
	.del = styx_listener_del,
};

static int styx_bridge_subscribe_event(struct v4l2_subdev *sd,
				       struct v4l2_fh *fh,
				       struct v4l2_event_subscription *sub)
{
	switch (sub->type) {
	case STYX_BRIDGE_EVENT_STREAM:
		if (sub->id)
			return -EINVAL;
		return v4l2_event_subscribe(fh, sub, STYX_EVENT_QUEUE_DEPTH,
					    &styx_listener_ops);
	case V4L2_EVENT_CTRL:
		return v4l2_ctrl_subdev_subscribe_event(sd, fh, sub);
	default:
		return -EINVAL;
	}
}

/* ------------------------------------------------------------------------
 * Controls.
 */

static int styx_bridge_s_ctrl(struct v4l2_ctrl *ctrl)
{
	struct styx_bridge *b = container_of(ctrl->handler, struct styx_bridge,
					     hdl);

	switch (ctrl->id) {
	case STYX_CID_STREAM_ACK:
		return styx_bridge_ack(b, (u64)*ctrl->p_new.p_s64);
	case STYX_CID_POWER:
		return styx_bridge_set_power(b, ctrl->val);
	default:
		/* Timing controls only carry values for the receiver. */
		return 0;
	}
}

static int styx_bridge_g_volatile_ctrl(struct v4l2_ctrl *ctrl)
{
	struct styx_bridge *b = container_of(ctrl->handler, struct styx_bridge,
					     hdl);
	unsigned long flags;

	switch (ctrl->id) {
	case STYX_CID_STREAM_STATE:
		ctrl->val = atomic_read(&b->state);
		return 0;
	case STYX_CID_STREAM_SEQUENCE:
		spin_lock_irqsave(&b->ack_lock, flags);
		ctrl->val = (s32)b->seq;
		spin_unlock_irqrestore(&b->ack_lock, flags);
		return 0;
	default:
		return -EINVAL;
	}
}

static const struct v4l2_ctrl_ops styx_ctrl_ops = {
	.s_ctrl = styx_bridge_s_ctrl,
	.g_volatile_ctrl = styx_bridge_g_volatile_ctrl,
};

static const struct v4l2_ctrl_config styx_ctrl_ack = {
	.ops = &styx_ctrl_ops,
	.id = STYX_CID_STREAM_ACK,
	.name = "Styx Stream Ack",
	.type = V4L2_CTRL_TYPE_INTEGER64,
	.min = S64_MIN, .max = S64_MAX, .step = 1, .def = 0,
	.flags = V4L2_CTRL_FLAG_EXECUTE_ON_WRITE,
};

static const struct v4l2_ctrl_config styx_ctrl_state = {
	.ops = &styx_ctrl_ops,
	.id = STYX_CID_STREAM_STATE,
	.name = "Styx Stream State",
	.type = V4L2_CTRL_TYPE_INTEGER,
	.min = STYX_BRIDGE_STATE_IDLE, .max = STYX_BRIDGE_STATE_START_FAILED,
	.step = 1, .def = STYX_BRIDGE_STATE_IDLE,
	.flags = V4L2_CTRL_FLAG_READ_ONLY | V4L2_CTRL_FLAG_VOLATILE,
};

static const struct v4l2_ctrl_config styx_ctrl_timeout = {
	.ops = &styx_ctrl_ops,
	.id = STYX_CID_ACK_TIMEOUT_MS,
	.name = "Styx Ack Timeout ms",
	.type = V4L2_CTRL_TYPE_INTEGER,
	.min = 10, .max = 10000, .step = 1, .def = STYX_DEFAULT_TIMEOUT_MS,
};

static const struct v4l2_ctrl_config styx_ctrl_power = {
	.ops = &styx_ctrl_ops,
	.id = STYX_CID_POWER,
	.name = "Styx Sensor Power",
	.type = V4L2_CTRL_TYPE_BOOLEAN,
	.min = 0, .max = 1, .step = 1, .def = 0,
};

static const struct v4l2_ctrl_config styx_ctrl_sequence = {
	.ops = &styx_ctrl_ops,
	.id = STYX_CID_STREAM_SEQUENCE,
	.name = "Styx Stream Sequence",
	.type = V4L2_CTRL_TYPE_INTEGER,
	.min = S32_MIN, .max = S32_MAX, .step = 1, .def = 0,
	.flags = V4L2_CTRL_FLAG_READ_ONLY | V4L2_CTRL_FLAG_VOLATILE,
};

static int styx_bridge_init_controls(struct styx_bridge *b)
{
	struct v4l2_ctrl_handler *hdl = &b->hdl;
	struct v4l2_fwnode_device_properties props;
	u64 bits = styx_code_bpp(b->codes[0]);
	s64 def_rate;
	int ret;

	ret = v4l2_fwnode_device_parse(b->dev, &props);
	if (ret)
		return ret;

	v4l2_ctrl_handler_init(hdl, 12);

	b->link_freq = v4l2_ctrl_new_int_menu(hdl, &styx_ctrl_ops,
					      V4L2_CID_LINK_FREQ,
					      b->num_link_freqs - 1, 0,
					      b->link_freqs);

	/* Default pixel rate: the first link frequency, DDR, all lanes. */
	def_rate = div64_u64((u64)b->link_freqs[0] * 2 * b->data_lanes, bits);
	b->pixel_rate = v4l2_ctrl_new_std(hdl, &styx_ctrl_ops,
					  V4L2_CID_PIXEL_RATE, 1, S64_MAX, 1,
					  max_t(s64, def_rate, 1));
	b->hblank = v4l2_ctrl_new_std(hdl, &styx_ctrl_ops, V4L2_CID_HBLANK,
				      0, S32_MAX, 1, 0);
	b->vblank = v4l2_ctrl_new_std(hdl, &styx_ctrl_ops, V4L2_CID_VBLANK,
				      0, S32_MAX, 1, 0);

	v4l2_ctrl_new_custom(hdl, &styx_ctrl_ack, NULL);
	v4l2_ctrl_new_custom(hdl, &styx_ctrl_state, NULL);
	b->timeout = v4l2_ctrl_new_custom(hdl, &styx_ctrl_timeout, NULL);
	b->power = v4l2_ctrl_new_custom(hdl, &styx_ctrl_power, NULL);
	v4l2_ctrl_new_custom(hdl, &styx_ctrl_sequence, NULL);

	v4l2_ctrl_new_fwnode_properties(hdl, &styx_ctrl_ops, &props);

	if (hdl->error) {
		ret = hdl->error;
		v4l2_ctrl_handler_free(hdl);
		return ret;
	}

	/* Userspace owns the timing: the standard pixel rate is read-only. */
	b->pixel_rate->flags &= ~V4L2_CTRL_FLAG_READ_ONLY;

	b->sd.ctrl_handler = hdl;
	return 0;
}

/* ------------------------------------------------------------------------
 * Pad operations.
 */

static void styx_bridge_fill_fmt(struct styx_bridge *b,
				 struct v4l2_mbus_framefmt *fmt, u32 code,
				 u32 width, u32 height)
{
	memset(fmt, 0, sizeof(*fmt));
	fmt->code = styx_code_supported(b, code) ? code : b->codes[0];
	fmt->width = clamp(width, b->min_width, b->max_width);
	fmt->height = clamp(height, b->min_height, b->max_height);
	fmt->field = V4L2_FIELD_NONE;
	fmt->colorspace = V4L2_COLORSPACE_RAW;
	fmt->ycbcr_enc = V4L2_YCBCR_ENC_601;
	fmt->quantization = V4L2_QUANTIZATION_FULL_RANGE;
	fmt->xfer_func = V4L2_XFER_FUNC_NONE;
}

static void styx_bridge_fill_emb_fmt(struct v4l2_mbus_framefmt *fmt,
				     u32 width, u32 lines)
{
	memset(fmt, 0, sizeof(*fmt));
	fmt->code = MEDIA_BUS_FMT_SENSOR_DATA;
	fmt->width = clamp(width, 1U, (u32)STYX_EMBEDDED_MAX_WIDTH);
	fmt->height = clamp(lines, 1U, (u32)STYX_EMBEDDED_MAX_LINES);
	fmt->field = V4L2_FIELD_NONE;
}

static int styx_bridge_init_state(struct v4l2_subdev *sd,
				  struct v4l2_subdev_state *state)
{
	struct styx_bridge *b = to_bridge(sd);

	styx_bridge_fill_fmt(b, v4l2_subdev_state_get_format(state, 0),
			     b->codes[0], b->max_width, b->max_height);
	if (b->num_pads > STYX_PAD_EMBEDDED)
		styx_bridge_fill_emb_fmt(
			v4l2_subdev_state_get_format(state, STYX_PAD_EMBEDDED),
			b->emb_width, b->emb_lines);
	return 0;
}

static int styx_bridge_enum_mbus_code(struct v4l2_subdev *sd,
				      struct v4l2_subdev_state *state,
				      struct v4l2_subdev_mbus_code_enum *code)
{
	struct styx_bridge *b = to_bridge(sd);

	if (code->pad == STYX_PAD_EMBEDDED && b->num_pads > STYX_PAD_EMBEDDED) {
		if (code->index)
			return -EINVAL;
		code->code = MEDIA_BUS_FMT_SENSOR_DATA;
		return 0;
	}
	if (code->pad || code->index >= b->num_codes)
		return -EINVAL;
	code->code = b->codes[code->index];
	return 0;
}

static int styx_bridge_enum_frame_size(struct v4l2_subdev *sd,
				       struct v4l2_subdev_state *state,
				       struct v4l2_subdev_frame_size_enum *fse)
{
	struct styx_bridge *b = to_bridge(sd);

	if (fse->pad == STYX_PAD_EMBEDDED && b->num_pads > STYX_PAD_EMBEDDED) {
		if (fse->index || fse->code != MEDIA_BUS_FMT_SENSOR_DATA)
			return -EINVAL;
		fse->min_width = 1;
		fse->max_width = STYX_EMBEDDED_MAX_WIDTH;
		fse->min_height = 1;
		fse->max_height = STYX_EMBEDDED_MAX_LINES;
		return 0;
	}
	if (fse->pad || fse->index || !styx_code_supported(b, fse->code))
		return -EINVAL;
	fse->min_width = b->min_width;
	fse->max_width = b->max_width;
	fse->min_height = b->min_height;
	fse->max_height = b->max_height;
	return 0;
}

static int styx_bridge_set_fmt(struct v4l2_subdev *sd,
			       struct v4l2_subdev_state *state,
			       struct v4l2_subdev_format *format)
{
	struct styx_bridge *b = to_bridge(sd);
	struct v4l2_mbus_framefmt *fmt;

	if (format->pad >= b->num_pads)
		return -EINVAL;
	if (format->which == V4L2_SUBDEV_FORMAT_ACTIVE &&
	    atomic_read(&b->state) != STYX_BRIDGE_STATE_IDLE)
		return -EBUSY;

	fmt = v4l2_subdev_state_get_format(state, format->pad);
	if (format->pad == STYX_PAD_EMBEDDED) {
		styx_bridge_fill_emb_fmt(fmt, format->format.width,
					 format->format.height);
		format->format = *fmt;
		return 0;
	}
	styx_bridge_fill_fmt(b, fmt, format->format.code, format->format.width,
			     format->format.height);
	format->format = *fmt;
	return 0;
}

static int styx_bridge_get_selection(struct v4l2_subdev *sd,
				     struct v4l2_subdev_state *state,
				     struct v4l2_subdev_selection *sel)
{
	struct styx_bridge *b = to_bridge(sd);
	struct v4l2_mbus_framefmt *fmt;

	if (sel->pad)
		return -EINVAL;

	switch (sel->target) {
	case V4L2_SEL_TGT_NATIVE_SIZE:
	case V4L2_SEL_TGT_CROP_BOUNDS:
	case V4L2_SEL_TGT_CROP_DEFAULT:
		sel->r = (struct v4l2_rect){ 0, 0, b->max_width, b->max_height };
		return 0;
	case V4L2_SEL_TGT_CROP:
		fmt = v4l2_subdev_state_get_format(state, 0);
		sel->r = (struct v4l2_rect){ 0, 0, fmt->width, fmt->height };
		return 0;
	default:
		return -EINVAL;
	}
}

static int styx_bridge_get_mbus_config(struct v4l2_subdev *sd,
				       unsigned int pad,
				       struct v4l2_mbus_config *cfg)
{
	struct styx_bridge *b = to_bridge(sd);

	memset(cfg, 0, sizeof(*cfg));
	cfg->type = V4L2_MBUS_CSI2_DPHY;
	cfg->bus.mipi_csi2.num_data_lanes = b->data_lanes;
	cfg->bus.mipi_csi2.flags = b->bus_flags;
	return 0;
}

/*
 * One CSI-2 stream per source pad (virtual channel 0): the image with the data
 * type of its bus code, embedded data with the device tree's data type. The
 * Raspberry Pi 6.12 rp1-cfe asks this per sensor pad and needs exactly one
 * entry; it links the second source pad to its embedded data channel.
 */
static int styx_bridge_get_frame_desc(struct v4l2_subdev *sd, unsigned int pad,
				      struct v4l2_mbus_frame_desc *fd)
{
	struct styx_bridge *b = to_bridge(sd);
	struct v4l2_mbus_frame_desc_entry *e = &fd->entry[0];
	struct v4l2_subdev_state *state;
	struct v4l2_mbus_framefmt *fmt;

	if (pad >= b->num_pads)
		return -EINVAL;

	memset(fd, 0, sizeof(*fd));
	fd->type = V4L2_MBUS_FRAME_DESC_TYPE_CSI2;
	fd->num_entries = 1;

	state = v4l2_subdev_lock_and_get_active_state(sd);
	fmt = v4l2_subdev_state_get_format(state, pad);
	e->stream = 0;
	e->pixelcode = fmt->code;
	e->bus.csi2.vc = 0;
	if (pad == STYX_PAD_EMBEDDED) {
		e->flags = V4L2_MBUS_FRAME_DESC_FL_LEN_MAX;
		e->length = fmt->width * fmt->height;
		e->bus.csi2.dt = b->emb_dt;
	} else {
		e->bus.csi2.dt = styx_code_dt(fmt->code);
	}
	v4l2_subdev_unlock_state(state);
	return 0;
}

static const struct v4l2_subdev_core_ops styx_core_ops = {
	.subscribe_event = styx_bridge_subscribe_event,
	.unsubscribe_event = v4l2_event_subdev_unsubscribe,
};

static const struct v4l2_subdev_video_ops styx_video_ops = {
	.s_stream = styx_bridge_s_stream,
};

static const struct v4l2_subdev_pad_ops styx_pad_ops = {
	.enum_mbus_code = styx_bridge_enum_mbus_code,
	.enum_frame_size = styx_bridge_enum_frame_size,
	.get_fmt = v4l2_subdev_get_fmt,
	.set_fmt = styx_bridge_set_fmt,
	.get_selection = styx_bridge_get_selection,
	.get_mbus_config = styx_bridge_get_mbus_config,
	.get_frame_desc = styx_bridge_get_frame_desc,
};

static const struct v4l2_subdev_ops styx_subdev_ops = {
	.core = &styx_core_ops,
	.video = &styx_video_ops,
	.pad = &styx_pad_ops,
};

static const struct v4l2_subdev_internal_ops styx_internal_ops = {
	.init_state = styx_bridge_init_state,
};

/* ------------------------------------------------------------------------
 * sysfs: where userspace finds the sensor.
 */

static ssize_t sensor_name_show(struct device *dev,
				struct device_attribute *attr, char *buf)
{
	struct styx_bridge *b = dev_get_drvdata(dev);

	return sysfs_emit(buf, "%s\n", b->sensor_name);
}
static DEVICE_ATTR_RO(sensor_name);

static ssize_t i2c_bus_show(struct device *dev, struct device_attribute *attr,
			    char *buf)
{
	struct styx_bridge *b = dev_get_drvdata(dev);

	return sysfs_emit(buf, "%d\n", b->i2c_bus);
}
static DEVICE_ATTR_RO(i2c_bus);

static ssize_t i2c_address_show(struct device *dev,
				struct device_attribute *attr, char *buf)
{
	struct styx_bridge *b = dev_get_drvdata(dev);

	return sysfs_emit(buf, "0x%02x\n", b->i2c_address);
}
static DEVICE_ATTR_RO(i2c_address);

static ssize_t clock_frequency_show(struct device *dev,
				    struct device_attribute *attr, char *buf)
{
	struct styx_bridge *b = dev_get_drvdata(dev);

	return sysfs_emit(buf, "%lu\n", b->clk ? clk_get_rate(b->clk) : 0UL);
}
static DEVICE_ATTR_RO(clock_frequency);

static ssize_t stream_state_show(struct device *dev,
				 struct device_attribute *attr, char *buf)
{
	struct styx_bridge *b = dev_get_drvdata(dev);

	return sysfs_emit(buf, "%d\n", atomic_read(&b->state));
}
static DEVICE_ATTR_RO(stream_state);

static struct attribute *styx_bridge_attrs[] = {
	&dev_attr_sensor_name.attr,
	&dev_attr_i2c_bus.attr,
	&dev_attr_i2c_address.attr,
	&dev_attr_clock_frequency.attr,
	&dev_attr_stream_state.attr,
	NULL,
};
ATTRIBUTE_GROUPS(styx_bridge);

/* ------------------------------------------------------------------------
 * Probe.
 */

static int styx_bridge_parse_endpoint(struct styx_bridge *b)
{
	struct v4l2_fwnode_endpoint vep = { .bus_type = V4L2_MBUS_CSI2_DPHY };
	struct fwnode_handle *ep;
	int ret;

	ep = fwnode_graph_get_next_endpoint(dev_fwnode(b->dev), NULL);
	if (!ep)
		return dev_err_probe(b->dev, -EINVAL, "no endpoint\n");

	ret = v4l2_fwnode_endpoint_alloc_parse(ep, &vep);
	fwnode_handle_put(ep);
	if (ret)
		return dev_err_probe(b->dev, ret, "bad endpoint\n");

	if (!vep.nr_of_link_frequencies ||
	    !vep.bus.mipi_csi2.num_data_lanes) {
		ret = dev_err_probe(b->dev, -EINVAL,
				    "endpoint needs data-lanes and link-frequencies\n");
		goto out;
	}

	b->link_freqs = devm_kmemdup(b->dev, vep.link_frequencies,
				     vep.nr_of_link_frequencies * sizeof(s64),
				     GFP_KERNEL);
	if (!b->link_freqs) {
		ret = -ENOMEM;
		goto out;
	}
	b->num_link_freqs = vep.nr_of_link_frequencies;
	b->data_lanes = vep.bus.mipi_csi2.num_data_lanes;
	b->bus_flags = vep.bus.mipi_csi2.flags;
out:
	v4l2_fwnode_endpoint_free(&vep);
	return ret;
}

static int styx_bridge_parse_formats(struct styx_bridge *b)
{
	struct device *dev = b->dev;
	u32 size[2];
	int n;

	n = device_property_count_u32(dev, "styx,mbus-codes");
	if (n > 0) {
		if (n > STYX_MAX_CODES)
			return dev_err_probe(dev, -EINVAL, "too many mbus codes\n");
		device_property_read_u32_array(dev, "styx,mbus-codes",
					       b->codes, n);
		b->num_codes = n;
	} else {
		memcpy(b->codes, styx_default_codes, sizeof(styx_default_codes));
		b->num_codes = ARRAY_SIZE(styx_default_codes);
	}

	b->max_width = 8192;
	b->max_height = 8192;
	if (!device_property_read_u32_array(dev, "styx,max-size", size, 2)) {
		b->max_width = size[0];
		b->max_height = size[1];
	}
	b->min_width = 16;
	b->min_height = 16;
	if (!device_property_read_u32_array(dev, "styx,min-size", size, 2)) {
		b->min_width = size[0];
		b->min_height = size[1];
	}
	if (!b->max_width || !b->max_height || b->min_width > b->max_width ||
	    b->min_height > b->max_height)
		return dev_err_probe(dev, -EINVAL, "bad styx,min/max-size\n");

	b->num_pads = 1;
	if (!device_property_read_u32_array(dev, "styx,embedded-data", size, 2)) {
		u32 dt = MIPI_CSI2_DT_EMBEDDED_8B;

		if (!size[0] || size[0] > STYX_EMBEDDED_MAX_WIDTH || !size[1] ||
		    size[1] > STYX_EMBEDDED_MAX_LINES)
			return dev_err_probe(dev, -EINVAL, "bad styx,embedded-data\n");
		device_property_read_u32(dev, "styx,embedded-data-type", &dt);
		if (dt > 0x3f)
			return dev_err_probe(dev, -EINVAL, "bad styx,embedded-data-type\n");
		b->emb_width = size[0];
		b->emb_lines = size[1];
		b->emb_dt = dt;
		b->num_pads = 2;
	}
	return 0;
}

static int styx_bridge_parse_resources(struct styx_bridge *b)
{
	struct device *dev = b->dev;
	const char *names[STYX_MAX_SUPPLIES];
	struct device_node *bus_np;
	struct i2c_adapter *adap;
	int n, i, ret;

	b->sensor_name = "sensor";
	device_property_read_string(dev, "styx,sensor-name", &b->sensor_name);

	b->i2c_bus = -1;
	bus_np = of_parse_phandle(dev->of_node, "styx,i2c-bus", 0);
	if (bus_np) {
		adap = of_find_i2c_adapter_by_node(bus_np);
		of_node_put(bus_np);
		if (!adap)
			return -EPROBE_DEFER;
		b->i2c_bus = i2c_adapter_id(adap);
		put_device(&adap->dev);
	}
	device_property_read_u32(dev, "styx,i2c-address", &b->i2c_address);

	b->clk = devm_clk_get_optional(dev, NULL);
	if (IS_ERR(b->clk))
		return dev_err_probe(dev, PTR_ERR(b->clk), "clock\n");

	n = device_property_string_array_count(dev, "styx,supply-names");
	if (n > 0) {
		if (n > STYX_MAX_SUPPLIES)
			return dev_err_probe(dev, -EINVAL, "too many supplies\n");
		device_property_read_string_array(dev, "styx,supply-names",
						  names, n);
		for (i = 0; i < n; i++)
			b->supplies[i].supply = names[i];
		b->num_supplies = n;
		ret = devm_regulator_bulk_get(dev, n, b->supplies);
		if (ret)
			return dev_err_probe(dev, ret, "supplies\n");
	}
	return 0;
}

static int styx_bridge_probe(struct platform_device *pdev)
{
	struct device *dev = &pdev->dev;
	struct styx_bridge *b;
	int ret;

	b = devm_kzalloc(dev, sizeof(*b), GFP_KERNEL);
	if (!b)
		return -ENOMEM;
	b->dev = dev;
	mutex_init(&b->stream_lock);
	spin_lock_init(&b->ack_lock);
	init_completion(&b->ack_done);
	atomic_set(&b->state, STYX_BRIDGE_STATE_IDLE);
	platform_set_drvdata(pdev, b);

	ret = styx_bridge_parse_endpoint(b);
	if (ret)
		return ret;
	ret = styx_bridge_parse_formats(b);
	if (ret)
		return ret;
	ret = styx_bridge_parse_resources(b);
	if (ret)
		return ret;

	v4l2_subdev_init(&b->sd, &styx_subdev_ops);
	b->sd.internal_ops = &styx_internal_ops;
	b->sd.dev = dev;
	b->sd.owner = THIS_MODULE;
	b->sd.flags |= V4L2_SUBDEV_FL_HAS_DEVNODE | V4L2_SUBDEV_FL_HAS_EVENTS;
	b->sd.entity.function = MEDIA_ENT_F_CAM_SENSOR;
	snprintf(b->sd.name, sizeof(b->sd.name), "%s %s", b->sensor_name,
		 dev_name(dev));

	ret = styx_bridge_init_controls(b);
	if (ret)
		return dev_err_probe(dev, ret, "controls\n");

	b->pads[STYX_PAD_IMAGE].flags = MEDIA_PAD_FL_SOURCE;
	b->pads[STYX_PAD_EMBEDDED].flags = MEDIA_PAD_FL_SOURCE;
	ret = media_entity_pads_init(&b->sd.entity, b->num_pads, b->pads);
	if (ret)
		goto err_ctrls;

	ret = v4l2_subdev_init_finalize(&b->sd);
	if (ret)
		goto err_entity;

	/*
	 * Not v4l2_async_register_subdev_sensor(): it registers the subdev on
	 * behalf of v4l2-fwnode, which then becomes sd->owner, so neither the
	 * receiver's binding nor an open subdev node pinned this module and
	 * rmmod while streaming freed the s_stream the receiver calls next
	 * (an oops, measured). Registered as ours, the module is pinned while
	 * a receiver is bound and while the node is open. (The sensor variant
	 * only adds lens and flash links, which the bridge has none of.)
	 */
	ret = v4l2_async_register_subdev(&b->sd);
	if (ret)
		goto err_state;

	dev_info(dev, "bridge for %s: %u lanes, %u link frequencies, i2c %d-%04x%s\n",
		 b->sensor_name, b->data_lanes, b->num_link_freqs, b->i2c_bus,
		 b->i2c_address,
		 b->num_pads > 1 ? ", embedded data pad" : "");
	return 0;

err_state:
	v4l2_subdev_cleanup(&b->sd);
err_entity:
	media_entity_cleanup(&b->sd.entity);
err_ctrls:
	v4l2_ctrl_handler_free(&b->hdl);
	return ret;
}

static void styx_bridge_remove(struct platform_device *pdev)
{
	struct styx_bridge *b = platform_get_drvdata(pdev);

	styx_bridge_abort(b, -ENODEV);
	v4l2_async_unregister_subdev(&b->sd);
	v4l2_subdev_cleanup(&b->sd);
	media_entity_cleanup(&b->sd.entity);
	v4l2_ctrl_handler_free(&b->hdl);
	if (b->powered) {
		clk_disable_unprepare(b->clk);
		regulator_bulk_disable(b->num_supplies, b->supplies);
	}
	mutex_destroy(&b->stream_lock);
}

static const struct of_device_id styx_bridge_of_match[] = {
	{ .compatible = "styx,sensor-bridge" },
	{ }
};
MODULE_DEVICE_TABLE(of, styx_bridge_of_match);

static struct platform_driver styx_bridge_driver = {
	.probe = styx_bridge_probe,
	.remove = styx_bridge_remove,
	.driver = {
		.name = "styx-sensor-bridge",
		.of_match_table = styx_bridge_of_match,
		.dev_groups = styx_bridge_groups,
	},
};
module_platform_driver(styx_bridge_driver);

MODULE_DESCRIPTION("Styx generic camera sensor bridge (sensor driven from userspace)");
MODULE_AUTHOR("Styx contributors");
MODULE_LICENSE("GPL");
