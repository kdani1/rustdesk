#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"
MODE="${1:---verify}"
case "$MODE" in --build-only|--verify|--logs|--debug) ;; *) echo "Usage: $0 [--build-only|--verify|--logs|--debug]" >&2; exit 2;; esac
: "${VCPKG_ROOT:?Set VCPKG_ROOT to a populated vcpkg checkout}"
for tool in cargo flutter pod; do command -v "$tool" >/dev/null || { echo "Missing build prerequisite: $tool" >&2; exit 1; }; done
[[ -f flutter/lib/generated_bridge.dart ]] || { echo 'Generate bridge bindings first; see .github/workflows/bridge.yml' >&2; exit 1; }
APP="$ROOT_DIR/flutter/build/macos/Build/Products/Release/RustDesk.app"
# Only stop a prior build from this checkout. Preserve the installed stable app.
if [[ "$MODE" != --build-only ]]; then
  pids="$(pgrep -f "^${APP}/Contents/MacOS/RustDesk$" || true)"
  if [[ -n "$pids" ]]; then kill $pids; fi
fi
MACOSX_DEPLOYMENT_TARGET=10.14 cargo build --locked --release --features flutter --lib --bin service
cp target/release/liblibrustdesk.dylib target/release/librustdesk.dylib
(
  cd flutter
  FLUTTER_XCODE_ARCHS="$(uname -m)" FLUTTER_XCODE_ONLY_ACTIVE_ARCH=YES flutter build macos --release
)
cp target/release/service "$APP/Contents/MacOS/service"
# Local development signature; this is not an Apple-notarized release.
codesign --force --deep --sign - --entitlements flutter/macos/Runner/Release.entitlements "$APP"
codesign --verify --deep --strict "$APP"
[[ "$MODE" != --build-only ]] || exit 0
open -n "$APP"
case "$MODE" in
  --verify) sleep 2; pgrep -f "^${APP}/Contents/MacOS/RustDesk" >/dev/null ;;
  --logs) /usr/bin/log stream --info --predicate 'process == "RustDesk"' ;;
  --debug) echo 'Attach LLDB to the process only after granting debugger permissions.'; pgrep -f "^${APP}/Contents/MacOS/RustDesk" ;;
esac
