#!/usr/bin/env bash
#
# enc-bench.sh — raw throughput of one VA hardware encoder at 1080p, with the
# GPU otherwise as idle as you leave it. No capture, no network: a pre-rolled
# NV12 source (videotestsrc, pattern=smpte) straight into the encoder, rate
# read from fpsdisplaysink. Answers "can this encoder do 240 fps at all?"
#
# Usage: enc-bench.sh CODEC [FRAMES] [EXTRA_ENC_PROPS...]
#   CODEC: h264 | h265 | av1  (vah264enc | vah265enc | vaav1enc)
#   e.g.  enc-bench.sh h264 1500 target-usage=7 b-frames=0
# Prints the fps measured by fpsdisplaysink (average over the run).

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
# videotestsrc is the CPU-side limit (~300+ fps for NV12 smpte); an identity
# with a queue decouples it a little. If the result is suspiciously close to
# the source's own rate, raise the resolution of the check, not the conclusion.
gst-launch-1.0 -v videotestsrc num-buffers="$frames" pattern=smpte is-live=false \
  ! video/x-raw,format=NV12,width=1920,height=1080,framerate=300/1 \
  ! queue max-size-buffers=8 \
  ! "$enc" "$@" \
  ! fpsdisplaysink video-sink=fakesink text-overlay=false sync=false signal-fps-measurements=true 9>&- 2>&1 \
  | grep -E "last-message" | tail -1 | sed 's/.*last-message = //'
