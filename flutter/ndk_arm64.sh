#!/usr/bin/env bash
set -euo pipefail

: "${ANDROID_NDK_HOME:=${ANDROID_NDK_ROOT:-}}"
: "${ANDROID_NDK_HOME:?Set ANDROID_NDK_HOME to the installed Android NDK}"
case "$(uname -s)" in
  Darwin) ndk_host=darwin-x86_64 ;;
  Linux) ndk_host=linux-x86_64 ;;
  *) echo "Use this Android build script on macOS or Linux" >&2; exit 1 ;;
esac
ndk_sysroot="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$ndk_host/sysroot"
[[ -f "$ndk_sysroot/usr/include/stdint.h" ]] || { echo "Missing NDK headers: $ndk_sysroot" >&2; exit 1; }
# Bindgen must use Android headers, not the build computer's libc headers.
export BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android="${BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android:-} --sysroot=$ndk_sysroot -I$ndk_sysroot/usr/include/aarch64-linux-android"
export ANDROID_NDK_HOME
cargo ndk --platform 21 --target aarch64-linux-android build --locked --release --features flutter,hwcodec
