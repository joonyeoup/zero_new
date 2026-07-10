#!/bin/sh
# Stub of the TV's tizenscreenshot binary for local testing: "captures" by
# copying the bundled sample PNG to the fixed output path (same contract as
# the real binary). Output path via $SCREENSHOT_OUTPUT or first argument.
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${SCREENSHOT_OUTPUT:-${1:-/tmp/screenshot.png}}"
cp "$HERE/assets/sample_screen.png" "$OUT"
