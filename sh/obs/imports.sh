#!/usr/bin/env bash
# Fail when the built plugin still links FFmpeg.
#
# The source decodes through moq-video. libswscale, libavcodec, libavutil, and
# libswresample, and the symbols those libraries are called by, must not show up
# on the plugin itself. A dependency of libobs is not this plugin's link line,
# so this reads the plugin's own needed libraries and undefined symbols.
set -euo pipefail

plugin=${1:-}
if [[ -z "$plugin" || ! -f "$plugin" ]]; then
    echo "imports: pass the built obs-moq plugin" >&2
    exit 1
fi

dump=$(mktemp)
trap 'rm -f "$dump"' EXIT
if [[ "$(uname -s)" == Darwin ]]; then
    otool -L "$plugin" >"$dump"
    nm -u "$plugin" >>"$dump" 2>/dev/null || true
else
    objdump -p "$plugin" >"$dump"
    nm -u "$plugin" >>"$dump" 2>/dev/null || true
fi

# macOS nm prefixes undefined symbols with an underscore. Match the library
# names and the symbol prefixes either way.
if grep -E 'libswscale|libavcodec|libavutil|libswresample|(^|[[:space:]_])(sws_|avcodec_|av_frame_|av_image_|swr_)' "$dump"; then
    echo "imports: $plugin still references FFmpeg" >&2
    exit 1
fi
echo "imports: $plugin does not reference FFmpeg"
