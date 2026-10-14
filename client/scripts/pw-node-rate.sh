#!/usr/bin/env bash
#
# pw-node-rate.sh — independent probe of how fast a PipeWire screencast node
# really delivers buffers, with NO conversion or copy in the way: a second
# consumer on the same node, `pipewiresrc ! fakesink`, rate measured by
# fpsdisplaysink. Tells "the portal/compositor produces only N fps" apart from
# "our pipeline (videoconvert, copy out of a dma-buf, ...) limits to N fps".
#
# Usage: pw-node-rate.sh NODE_ID [SECS]
#   NODE_ID: from the sharing client's log line "portal granted node <id>".
# Prints fpsdisplaysink's per-second "current" rate and the negotiated caps.

set -uo pipefail
node="${1:?NODE_ID}"
secs="${2:-10}"
timeout "$secs" gst-launch-1.0 -v pipewiresrc path="$node" do-timestamp=true \
  ! fpsdisplaysink video-sink=fakesink text-overlay=false signal-fps-measurements=true sync=false 9>&- 2>&1 \
  | grep -E "last-message|caps = |ERROR|rror" | sed 's/.*last-message = //' | cut -c1-300 | head -${PROBE_LINES:-16}
