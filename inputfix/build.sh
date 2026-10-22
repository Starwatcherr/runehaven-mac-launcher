#!/bin/sh
# Builds inputfix/version.dll for 64-bit Windows (runs under Wine).
# Needs a MinGW-w64 cross compiler: x86_64-w64-mingw32-gcc (Linux packages,
# Homebrew "mingw-w64") or llvm-mingw's x86_64-w64-mingw32-clang.
set -eu
cd "$(dirname "$0")"
CC="${CC:-x86_64-w64-mingw32-gcc}"
mkdir -p build
"$CC" -O2 -Wall -Wextra -std=c11 -D_WIN32_WINNT=0x0A00 -DWINVER=0x0A00 \
    -shared -s -static-libgcc -Wl,--exclude-all-symbols -Wl,--no-insert-timestamp \
    -o build/version.dll src/proxy.c src/inputfix.c src/version.def -luser32 -lkernel32
echo "built $(pwd)/build/version.dll"
