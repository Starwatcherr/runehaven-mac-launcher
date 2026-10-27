#!/bin/sh
# Builds the app: the Mac helper without the development test control, the
# input fix DLL, then the app bundle with both inside (Contents/Resources).
# The DLL needs a MinGW-w64 compiler (Homebrew "mingw-w64"); without one, the
# last build in inputfix/build is used. Needs Rust and `cargo install tauri-cli`.
set -eu
cd "$(dirname "$0")/.."

OUT=build/app/rml-macfix.dylib macfix/build.sh

if command -v "${CC:-x86_64-w64-mingw32-gcc}" >/dev/null 2>&1; then
    inputfix/build.sh
elif [ -f inputfix/build/version.dll ]; then
    echo "note: no MinGW compiler found, using the existing inputfix/build/version.dll"
else
    echo "error: inputfix/build/version.dll is missing and no MinGW compiler is installed" >&2
    exit 1
fi

(cd crates/rml-app && cargo tauri build)
echo "app: $(pwd)/target/release/bundle/macos/Runehaven Mac Launcher.app"
