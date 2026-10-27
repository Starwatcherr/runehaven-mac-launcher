#!/bin/sh
# Builds macfix/build/rml-macfix.dylib (macOS only; needs the Xcode command
# line tools). Universal, because Wine runs as x86_64 under Rosetta.
# RML_DEV=1 adds the development-only test control (src/testcontrol.m); the
# normal build, used for the app, leaves it out. OUT sets the output file
# (default build/rml-macfix.dylib).
set -eu
cd "$(dirname "$0")"
OUT="${OUT:-build/rml-macfix.dylib}"
mkdir -p "$(dirname "$OUT")"
SOURCES="src/macfix.m"
FRAMEWORKS="-framework Foundation -framework ColorSync"
DEFINES=""
if [ "${RML_DEV:-0}" = "1" ]; then
    case "$OUT" in
        build/app/*)
            echo "error: the app's helper (build/app) is never built with the test control" >&2
            exit 1 ;;
    esac
    SOURCES="$SOURCES src/testcontrol.m"
    FRAMEWORKS="$FRAMEWORKS -framework CoreGraphics -framework ImageIO"
    DEFINES="-DRML_TEST_CONTROL"
fi
# shellcheck disable=SC2086
clang -arch x86_64 -arch arm64 -mmacosx-version-min=11.0 -O2 -Wall -Wextra \
    -dynamiclib $FRAMEWORKS $DEFINES \
    -o "$OUT" $SOURCES
if [ -n "$DEFINES" ]; then
    echo "built $(pwd)/$OUT (development build with test control)"
else
    echo "built $(pwd)/$OUT"
fi
