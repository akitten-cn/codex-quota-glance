#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
test "$(uname -s)" = Darwin || { echo '此脚本必须在 macOS 构建机执行' >&2; exit 1; }
arch="$(uname -m)"
case "$arch" in arm64) target=aarch64-apple-darwin;; x86_64) target=x86_64-apple-darwin;; *) exit 1;; esac
export MACOSX_DEPLOYMENT_TARGET=12.0
cargo build --release --locked --target "$target" --package codex-taskbar
mkdir -p dist
# 每次使用新 staging，不删除已有安装包或用户文件。
stage="$(mktemp -d "$PWD/dist/macos-stage.XXXXXX")"
app="$stage/Codex Taskbar.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "target/$target/release/codex-taskbar" "$app/Contents/MacOS/codex-taskbar-engine"
cp apps/macos-host/Info.plist "$app/Contents/Info.plist"
swiftc -swift-version 5 -O -target "$arch-apple-macos12.0" -framework AppKit -framework WebKit apps/macos-host/main.swift -o "$app/Contents/MacOS/CodexTaskbar"
cp prototypes/fluid-front-reference.html prototypes/details-card-reference.html prototypes/settings-layout-reference.html prototypes/taskbar-visual-contract.js "$app/Contents/Resources/"
cp docs/macos-testing.md "$stage/测试说明.md"
# 临时签名仅满足本机代码完整性要求，不是 Developer ID，也不声称 Apple 已公证。
codesign --force --sign - "$app/Contents/MacOS/codex-taskbar-engine"
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
plutil -lint "$app/Contents/Info.plist"
name="codex-taskbar-macos-$arch-test"
ditto -c -k --sequesterRsrc --keepParent "$app" "dist/$name.zip"
hdiutil create -volname 'Codex Taskbar Test' -srcfolder "$stage" -ov -format UDZO "dist/$name.dmg"
shasum -a 256 "dist/$name.zip" "dist/$name.dmg" > "dist/SHA256SUMS-macos-$arch.txt"
printf '%s\n' "$app" > dist/macos-app-path.txt
