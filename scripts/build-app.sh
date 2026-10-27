#!/bin/sh
# Builds the app: the Mac helper without the development test control, the
# input fix DLL, then the app bundle with both inside (Contents/Resources).
# The DLL needs a MinGW-w64 compiler (Homebrew "mingw-w64"). Without one the
# build stops, unless RML_PREBUILT_INPUTFIX=1 says to use the DLL already in
# inputfix/build (for example one from a CI run). Needs Rust and
# `cargo install tauri-cli`.
set -eu
cd "$(dirname "$0")/.."

# RML_DEV=0: the app never gets the development test control, whatever the shell has set.
RML_DEV=0 OUT=build/app/rml-macfix.dylib macfix/build.sh

if command -v "${CC:-x86_64-w64-mingw32-gcc}" >/dev/null 2>&1; then
    inputfix/build.sh
elif [ "${RML_PREBUILT_INPUTFIX:-0}" = "1" ] && [ -f inputfix/build/version.dll ]; then
    echo "note: no MinGW compiler; using the existing DLL as asked:"
    shasum -a 256 inputfix/build/version.dll
else
    echo "error: no MinGW compiler. Install Homebrew's mingw-w64, or set" >&2
    echo "       RML_PREBUILT_INPUTFIX=1 to use the existing inputfix/build/version.dll." >&2
    exit 1
fi

(cd crates/rml-app && cargo tauri build)
echo "app: $(pwd)/target/release/bundle/macos/Runehaven Mac Launcher.app"
