#!/bin/sh
# Builds macfix/build/rml-macfix.dylib (macOS only; needs the Xcode command
# line tools). Universal, because Wine runs as x86_64 under Rosetta.
# src/testcontrol.m is a development-only test control; remove it (and
# CoreGraphics/ImageIO) before building the app.
set -eu
cd "$(dirname "$0")"
mkdir -p build
clang -arch x86_64 -arch arm64 -mmacosx-version-min=11.0 -O2 -Wall -Wextra \
    -dynamiclib -framework Foundation -framework ColorSync \
    -framework CoreGraphics -framework ImageIO \
    -o build/rml-macfix.dylib src/macfix.m src/testcontrol.m
echo "built $(pwd)/build/rml-macfix.dylib"
