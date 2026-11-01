#!/usr/bin/env bash
#
# enc-bench.sh — raw throughput of one VA hardware encoder at 1080p, with the
# GPU otherwise as idle as you leave it. No capture, no network: a pre-rolled
# NV12 source straight into the encoder, rate read from fpsdisplaysink. Answers
# "can this encoder do 240 fps at all?"
#
# Usage: enc-bench.sh CODEC [FRAMES] [EXTRA_ENC_PROPS...]
#   CODEC: h264 | h265 | av1  (vah264enc | vah265enc | vaav1enc)
#   e.g.  enc-bench.sh h264 1500 target-usage=7 b-frames=0
# Prints the fps measured by fpsdisplaysink (average over the run).
#
# Environment (all optional):
#   SRC=file.nv12  read raw 1920x1080 NV12 frames from that file (e.g. made with
#                  `ffmpeg -i clip.mp4 -pix_fmt nv12 -f rawvideo file.nv12`)
#                  instead of videotestsrc. Use it whenever the CPU is busy:
#                  videotestsrc is CPU-bound at ~180 fps on a loaded box and then
#                  caps every encoder at that (found 2026-11-01, #92), while a
#                  file read from the page cache costs next to nothing. FRAMES
#                  must not exceed the frames in the file * SRC_REPEAT. SRC_FPS
#                  (default 60) is the clip's own framerate (rate control only).
#   SRC_REPEAT=N   play the file N times in a row (default 1), so a short clip
#                  can give a run long enough that pipeline start-up does not
#                  dominate the average. Needs bash process substitution.
#   OUT=file       also write the encoded elementary stream to that file (for a
#                  PSNR/bitrate check against SRC, see the note below).
#
# Quality check recipe (what #46/#92 used):
#   SRC=ref.nv12 OUT=enc.h265 enc-bench.sh h265 360 target-usage=2 rate-control=vbr bitrate=8000 b-frames=0
#   ffmpeg -f hevc -i enc.h265 -f rawvideo -pixel_format nv12 -video_size 1920x1080 -framerate 60 -i ref.nv12 \
#     -lavfi "[0:v]format=nv12[a];[1:v]format=nv12[b];[a][b]psnr" -f null -
#   (-f h264 | hevc | obu for the other codecs; bitrate = file bytes * 8 / (frames / 60) )

set -uo pipefail
codec="${1:?h264|h265|av1}"
frames="${2:-1500}"
shift 2 2>/dev/null || shift $#
case "$codec" in
  h264) enc=vah264enc ;;
  h265) enc=vah265enc ;;
  av1) enc=vaav1enc ;;
  *) echo "unknown codec $codec" >&2; exit 2 ;;
esac
if [ -n "${SRC:-}" ]; then
    # A pipe gives short reads, so fdsrc's num-buffers would not count frames:
  # cut the stream to FRAMES frames with head -c instead and let EOS end it.
  exec 0< <(for _ in $(seq "${SRC_REPEAT:-1}"); do cat "$SRC"; done | head -c $((frames * 3110400)))
  src=(fdsrc fd=0 blocksize=3110400
       ! rawvideoparse format=nv12 width=1920 height=1080 framerate="${SRC_FPS:-60}/1")
else
  # videotestsrc is the CPU-side limit (~300+ fps for NV12 smpte on an idle
  # box); an identity with a queue decouples it a little. If the result is
  # suspiciously close to the source's own rate, raise the resolution of the
  # check, not the conclusion (or use SRC=).
  src=(videotestsrc num-buffers="$frames" pattern=smpte is-live=false
       ! video/x-raw,format=NV12,width=1920,height=1080,framerate=300/1)
fi
if [ -n "${OUT:-}" ]; then
  sink="filesink location=$OUT"
else
  sink=fakesink
fi
gst-launch-1.0 -v "${src[@]}" \
  ! queue max-size-buffers=8 \
  ! "$enc" "$@" \
  ! fpsdisplaysink video-sink="$sink" text-overlay=false sync=false signal-fps-measurements=true 9>&- 2>&1 \
  | tee "${BENCH_LOG:-/dev/null}" | grep -E "last-message" | tail -1 | sed "s/.*last-message = //"
