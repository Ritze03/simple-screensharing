#!/usr/bin/env bash
#
# wlr-capture-rate.sh — how many frames per second does the compositor itself
# hand to a wlr-screencopy client for one output? Independent of the portal,
# PipeWire and this app's pipeline: `wf-recorder` records the output for SECS
# seconds with a hardware encoder and the frames in the file are counted.
#
# Usage: wlr-capture-rate.sh MONITOR [SECS] [OUTFILE]
# Refuses HDMI outputs. Prints frames / duration = fps. Run with the animation
# (animate.sh) on MONITOR, otherwise damage-driven capture yields ~0 fps.

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"

mon="${1:?MONITOR}"
secs="${2:-5}"
out="${3:-$CARGO_TARGET_DIR/wlr-capture-rate.mkv}"
sss_check_monitors "$mon" || exit 1
rm -f "$out"
timeout -s INT "$secs" wf-recorder -o "$mon" -f "$out" -c h264_vaapi -d /dev/dri/renderD128 9>&- >/dev/null 2>&1
sleep 1
frames="$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$out")"
echo "$mon: $frames frames in ~${secs}s via wlr-screencopy (wf-recorder) = $(awk -v f="$frames" -v s="$secs" 'BEGIN{printf "%.0f", f/s}') fps"
rm -f "$out"
