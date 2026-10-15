// Emulates the PipeWire format offer of xdg-desktop-portal-hyprland 1.4.1 (build_format/fixate_format
// copied from its src/shared/ScreencopyShared.cpp). Negotiation test only: buffers are fake memfds.
// usage: xdph-emu DMA_FMT SHM_FMT MODS(comma hex, "-" for none) [secs]
//   e.g. xdph-emu BGRx BGRx 0,200000028a01f04,ffffffffffffff 60      (LINEAR, an AMD DCC modifier, INVALID)
// Build: gcc -O1 -o xdph-emu xdph-emu.c -I/usr/include/pipewire-0.3 -I/usr/include/spa-0.2 -D_REENTRANT -lpipewire-0.3
// Run it INSIDE the headless rig only (`hs-rig.sh run -- ./xdph-emu ...`; the rig has no xdph, it
// serves a Video/Source node named "xdph-emu" on the rig's private PipeWire). Task #76: with a
// consumer whose DMA-BUF modifier list shares two or more entries with MODS, PipeWire hands the
// unfixed format to this producer, it fixates one and the consumer sees a SECOND Format param:
// pipewiresrc 1.4.2 answers that with "stream error: unhandled format", 1.6 and the patched
// plugin (client/packaging/patches) do not. Consume it with e.g.
//   gst-launch-1.0 pipewiresrc path=<node id> min-buffers=8 ! video/x-raw(memory:DMABuf),format=DMA_DRM,\
//     drm-format=(string){XR24:0x0200000028a01f04,XR24} ! fakesink sync=false
// (node id: `pw-dump | jq '.[] | select(.info.props["media.name"]=="xdph-emu").id'`, take the Node).
// For the AppImage's plugin run gst-launch-1.0 from the builder image with LD_LIBRARY_PATH and
// GST_PLUGIN_SYSTEM_PATH_1_0 pointing into the extracted AppImage (see client/packaging/README.md).
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <unistd.h>
#include <sys/mman.h>
#include <libdrm/drm_fourcc.h>
#include <spa/param/video/format-utils.h>
#include <spa/debug/format.h>
#include <spa/pod/dynamic.h>
#include <pipewire/pipewire.h>

#define W 1920
#define H 1080
#define FPS 240

static struct { const char *n; uint32_t v; } fmts[] = {
	{"BGRx", SPA_VIDEO_FORMAT_BGRx}, {"BGRA", SPA_VIDEO_FORMAT_BGRA},
	{"xRGB_210LE", SPA_VIDEO_FORMAT_xRGB_210LE}, {"ARGB_210LE", SPA_VIDEO_FORMAT_ARGB_210LE},
	{"xBGR_210LE", SPA_VIDEO_FORMAT_xBGR_210LE}, {"ABGR_210LE", SPA_VIDEO_FORMAT_ABGR_210LE},
	{"RGBx", SPA_VIDEO_FORMAT_RGBx}, {"RGBA", SPA_VIDEO_FORMAT_RGBA},
};
static uint32_t lookup(const char *n) {
	for (unsigned i = 0; i < SPA_N_ELEMENTS(fmts); i++) if (!strcmp(fmts[i].n, n)) return fmts[i].v;
	fprintf(stderr, "unknown format %s\n", n); exit(2);
}
static uint32_t strip_alpha(uint32_t f) {
	switch (f) {
	case SPA_VIDEO_FORMAT_BGRA: return SPA_VIDEO_FORMAT_BGRx;
	case SPA_VIDEO_FORMAT_ABGR: return SPA_VIDEO_FORMAT_xBGR;
	case SPA_VIDEO_FORMAT_RGBA: return SPA_VIDEO_FORMAT_RGBx;
	case SPA_VIDEO_FORMAT_ARGB: return SPA_VIDEO_FORMAT_xRGB;
	case SPA_VIDEO_FORMAT_ARGB_210LE: return SPA_VIDEO_FORMAT_xRGB_210LE;
	case SPA_VIDEO_FORMAT_ABGR_210LE: return SPA_VIDEO_FORMAT_xBGR_210LE;
	default: return SPA_VIDEO_FORMAT_UNKNOWN;
	}
}

static struct spa_pod *fixate_format(struct spa_pod_builder *b, uint32_t format, uint64_t *modifier) {
	struct spa_pod_frame f[1];
	uint32_t noalpha = strip_alpha(format);
	spa_pod_builder_push_object(b, &f[0], SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat);
	spa_pod_builder_add(b, SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video), 0);
	spa_pod_builder_add(b, SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw), 0);
	if (modifier || noalpha == SPA_VIDEO_FORMAT_UNKNOWN)
		spa_pod_builder_add(b, SPA_FORMAT_VIDEO_format, SPA_POD_Id(format), 0);
	else
		spa_pod_builder_add(b, SPA_FORMAT_VIDEO_format, SPA_POD_CHOICE_ENUM_Id(3, format, format, noalpha), 0);
	if (modifier) {
		spa_pod_builder_prop(b, SPA_FORMAT_VIDEO_modifier, SPA_POD_PROP_FLAG_MANDATORY);
		spa_pod_builder_long(b, *modifier);
	}
	spa_pod_builder_add(b, SPA_FORMAT_VIDEO_size, SPA_POD_Rectangle(&SPA_RECTANGLE(W, H)), 0);
	spa_pod_builder_add(b, SPA_FORMAT_VIDEO_framerate, SPA_POD_Fraction(&SPA_FRACTION(0, 1)), 0);
	spa_pod_builder_add(b, SPA_FORMAT_VIDEO_maxFramerate, SPA_POD_CHOICE_RANGE_Fraction(&SPA_FRACTION(FPS, 1), &SPA_FRACTION(1, 1), &SPA_FRACTION(FPS, 1)), 0);
	return spa_pod_builder_pop(b, &f[0]);
}

static struct spa_pod *build_format(struct spa_pod_builder *b, uint32_t format, uint64_t *modifiers, int modifier_count) {
	struct spa_pod_frame f[2];
	int i, c;
	uint32_t noalpha = strip_alpha(format);
	spa_pod_builder_push_object(b, &f[0], SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat);
	spa_pod_builder_add(b, SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video), 0);
	spa_pod_builder_add(b, SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw), 0);
	if (modifier_count > 0 || noalpha == SPA_VIDEO_FORMAT_UNKNOWN)
		spa_pod_builder_add(b, SPA_FORMAT_VIDEO_format, SPA_POD_Id(format), 0);
	else
		spa_pod_builder_add(b, SPA_FORMAT_VIDEO_format, SPA_POD_CHOICE_ENUM_Id(3, format, format, noalpha), 0);
	if (modifier_count > 0) {
		spa_pod_builder_prop(b, SPA_FORMAT_VIDEO_modifier, SPA_POD_PROP_FLAG_MANDATORY | SPA_POD_PROP_FLAG_DONT_FIXATE);
		spa_pod_builder_push_choice(b, &f[1], SPA_CHOICE_Enum, 0);
		for (i = 0, c = 0; i < modifier_count; i++) {
			spa_pod_builder_long(b, modifiers[i]);
			if (c++ == 0) spa_pod_builder_long(b, modifiers[i]);
		}
		spa_pod_builder_pop(b, &f[1]);
	}
	spa_pod_builder_add(b, SPA_FORMAT_VIDEO_size, SPA_POD_Rectangle(&SPA_RECTANGLE(W, H)), 0);
	spa_pod_builder_add(b, SPA_FORMAT_VIDEO_framerate, SPA_POD_Fraction(&SPA_FRACTION(0, 1)), 0);
	spa_pod_builder_add(b, SPA_FORMAT_VIDEO_maxFramerate, SPA_POD_CHOICE_RANGE_Fraction(&SPA_FRACTION(FPS, 1), &SPA_FRACTION(1, 1), &SPA_FRACTION(FPS, 1)), 0);
	return spa_pod_builder_pop(b, &f[0]);
}

static struct pw_thread_loop *loop;
static struct pw_stream *stream;
static uint32_t dma_fmt, shm_fmt;
static uint64_t mods[64];
static int n_mods;
static struct spa_video_info_raw vinfo;
static int stride;

static void build_all(const struct spa_pod **params, uint32_t *n, struct spa_pod_builder *b0, struct spa_pod_builder *b1) {
	*n = 0;
	if (n_mods > 0) params[(*n)++] = build_format(b0, dma_fmt, mods, n_mods);
	params[(*n)++] = build_format(b1, shm_fmt, NULL, 0);
}

static void on_state(void *d, enum pw_stream_state o, enum pw_stream_state s, const char *err) {
	printf("[emu] state %s -> %s %s\n", pw_stream_state_as_string(o), pw_stream_state_as_string(s), err ? err : "");
	fflush(stdout);
}

static void on_param(void *d, uint32_t id, const struct spa_pod *param) {
	if (id != SPA_PARAM_Format || !param) return;
	uint8_t pb[3][2048];
	struct spa_pod_builder b0 = SPA_POD_BUILDER_INIT(pb[0], sizeof pb[0]);
	struct spa_pod_builder b1 = SPA_POD_BUILDER_INIT(pb[1], sizeof pb[1]);
	struct spa_pod_builder b2 = SPA_POD_BUILDER_INIT(pb[2], sizeof pb[2]);
	const struct spa_pod *params[6];
	uint32_t n;
	spa_format_video_raw_parse(param, &vinfo);
	printf("[emu] param_changed Format: format=%u size=%ux%u\n", vinfo.format, vinfo.size.width, vinfo.size.height);
	spa_debug_format(4, NULL, param);
	fflush(stdout);
	stride = vinfo.size.width * 4;
	const struct spa_pod_prop *pm = spa_pod_find_prop(param, NULL, SPA_FORMAT_VIDEO_modifier);
	uint32_t data_type = 1 << SPA_DATA_MemFd;
	if (pm) {
		data_type = 1 << SPA_DATA_DmaBuf;
		if (pm->flags & SPA_POD_PROP_FLAG_DONT_FIXATE) {
			const struct spa_pod *pod = &pm->value;
			uint32_t nm = SPA_POD_CHOICE_N_VALUES(pod) - 1;
			uint64_t *ms = (uint64_t *)SPA_POD_CHOICE_VALUES(pod) + 1;
			uint64_t pick = ms[0];
			printf("[emu] DONT_FIXATE with %u modifiers, fixating on 0x%lx\n", nm, (unsigned long)pick);
			params[0] = fixate_format(&b2, dma_fmt, &pick);
			build_all(params + 1, &n, &b0, &b1);
			pw_stream_update_params(stream, params, n + 1);
			fflush(stdout);
			return;
		}
	}
	params[0] = spa_pod_builder_add_object(&b0, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
		SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 32),
		SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
		SPA_PARAM_BUFFERS_size, SPA_POD_Int(stride * vinfo.size.height),
		SPA_PARAM_BUFFERS_stride, SPA_POD_Int(stride),
		SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(data_type));
	params[1] = spa_pod_builder_add_object(&b1, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
		SPA_PARAM_META_type, SPA_POD_Id(SPA_META_Header), SPA_PARAM_META_size, SPA_POD_Int(sizeof(struct spa_meta_header)));
	pw_stream_update_params(stream, params, 2);
}

static void on_add(void *d, struct pw_buffer *buffer) {
	struct spa_data *dd = buffer->buffer->datas;
	size_t sz = stride * vinfo.size.height;
	int fd = memfd_create("emu", MFD_CLOEXEC);
	if (ftruncate(fd, sz) < 0) return;
	if (dd[0].type & (1 << SPA_DATA_DmaBuf)) dd[0].type = SPA_DATA_DmaBuf; else {
		dd[0].type = SPA_DATA_MemFd; dd[0].flags = SPA_DATA_FLAG_READWRITE | SPA_DATA_FLAG_MAPPABLE;
	}
	dd[0].fd = fd; dd[0].mapoffset = 0; dd[0].maxsize = sz;
	dd[0].data = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
}

static void on_process(void *d) {
	struct pw_buffer *b = pw_stream_dequeue_buffer(stream);
	if (!b) return;
	struct spa_data *dd = b->buffer->datas;
	if (dd[0].data && dd[0].data != MAP_FAILED) memset(dd[0].data, rand(), 1 << 16);
	dd[0].chunk->offset = 0; dd[0].chunk->size = stride * vinfo.size.height; dd[0].chunk->stride = stride;
	pw_stream_queue_buffer(stream, b);
}

static const struct pw_stream_events ev = {
	PW_VERSION_STREAM_EVENTS, .state_changed = on_state, .param_changed = on_param,
	.add_buffer = on_add, .process = on_process,
};

int main(int argc, char **argv) {
	if (argc < 4) { fprintf(stderr, "usage\n"); return 2; }
	int secs = argc > 4 ? atoi(argv[4]) : 60;
	dma_fmt = lookup(argv[1]); shm_fmt = lookup(argv[2]);
	if (strcmp(argv[3], "-")) {
		char *s = strdup(argv[3]), *t;
		for (t = strtok(s, ","); t; t = strtok(NULL, ",")) mods[n_mods++] = strtoull(t, NULL, 16);
	}
	pw_init(&argc, &argv);
	loop = pw_thread_loop_new("xdph-emu", NULL);
	pw_thread_loop_lock(loop);
	pw_thread_loop_start(loop);
	stream = pw_stream_new_simple(pw_thread_loop_get_loop(loop), "xdph-emu",
		pw_properties_new(PW_KEY_MEDIA_CLASS, "Video/Source", PW_KEY_MEDIA_NAME, "xdph-emu", NULL), &ev, NULL);
	uint8_t pb[2][2048];
	struct spa_pod_builder b0 = SPA_POD_BUILDER_INIT(pb[0], sizeof pb[0]);
	struct spa_pod_builder b1 = SPA_POD_BUILDER_INIT(pb[1], sizeof pb[1]);
	const struct spa_pod *params[2];
	uint32_t n;
	build_all(params, &n, &b0, &b1);
	pw_stream_connect(stream, PW_DIRECTION_OUTPUT, PW_ID_ANY,
		PW_STREAM_FLAG_DRIVER | PW_STREAM_FLAG_ALLOC_BUFFERS, params, n);
	printf("[emu] node %u up (dma=%s shm=%s mods=%d)\n", pw_stream_get_node_id(stream), argv[1], argv[2], n_mods);
	fflush(stdout);
	pw_thread_loop_unlock(loop);
	sleep(secs);
	pw_thread_loop_lock(loop);
	pw_stream_destroy(stream);
	pw_thread_loop_unlock(loop);
	pw_thread_loop_stop(loop);
	pw_thread_loop_destroy(loop);
	return 0;
}
