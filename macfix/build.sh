#!/bin/sh
# Builds macfix/build/rml-macfix.dylib (macOS only; needs the Xcode command
# line tools). Universal, because Wine runs as x86_64 under Rosetta.
# RML_DEV=1 adds the development-only test control (src/testcontrol.m); the
# normal build, used for the app, leaves it out.
set -eu
cd "$(dirname "$0")"
mkdir -p build
SOURCES="src/macfix.m"
FRAMEWORKS="-framework Foundation -framework ColorSync"
DEFINES=""
if [ "${RML_DEV:-0}" = "1" ]; then
    SOURCES="$SOURCES src/testcontrol.m"
    FRAMEWORKS="$FRAMEWORKS -framework CoreGraphics -framework ImageIO"
    DEFINES="-DRML_TEST_CONTROL"
fi
# shellcheck disable=SC2086
clang -arch x86_64 -arch arm64 -mmacosx-version-min=11.0 -O2 -Wall -Wextra \
    -dynamiclib $FRAMEWORKS $DEFINES \
    -o build/rml-macfix.dylib $SOURCES
if [ -n "$DEFINES" ]; then
    echo "built $(pwd)/build/rml-macfix.dylib (development build with test control)"
else
    echo "built $(pwd)/build/rml-macfix.dylib"
fi
