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
assets="$(mktemp -d "$PWD/dist/macos-assets.XXXXXX")"
swift scripts/macos-assets.swift "$assets"
iconutil -c icns "$assets/AppIcon.iconset" -o "$app/Contents/Resources/AppIcon.icns"
swiftc -swift-version 5 -O -target "$arch-apple-macos12.0" -framework AppKit -framework WebKit apps/macos-host/main.swift -o "$app/Contents/MacOS/CodexTaskbar"
cp prototypes/fluid-front-reference.html prototypes/details-card-reference.html prototypes/settings-layout-reference.html prototypes/taskbar-visual-contract.js "$app/Contents/Resources/"
cp docs/macos-testing.md "$app/Contents/Resources/测试说明.md"
# 临时签名仅满足本机代码完整性要求，不是 Developer ID，也不声称 Apple 已公证。
codesign --force --sign - "$app/Contents/MacOS/codex-taskbar-engine"
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
plutil -lint "$app/Contents/Info.plist"
name="codex-taskbar-macos-$arch-test"
ditto -c -k --sequesterRsrc --keepParent "$app" "dist/$name.zip"
# Finder 拖放安装：链接指向用户自己的 /Applications，而非构建机目录。
ln -s /Applications "$stage/Applications"
mkdir -p "$stage/.background"
cp "$assets/installer-background.png" "$stage/.background/"
cp "$app/Contents/Resources/AppIcon.icns" "$stage/.VolumeIcon.icns"
mount="$(mktemp -d "$PWD/dist/macos-mount.XXXXXX")"
rw="$assets/installer-rw.dmg"
hdiutil create -volname 'Codex Taskbar' -fs HFS+ -srcfolder "$stage" -format UDRW "$rw"
hdiutil attach "$rw" -mountpoint "$mount" -nobrowse
trap 'hdiutil detach "$mount" >/dev/null 2>&1 || true' EXIT
SetFile -a C "$mount"
osascript scripts/macos-dmg-layout.applescript "$mount"
test "$(readlink "$mount/Applications")" = /Applications
test -s "$mount/.DS_Store"
test -s "$mount/Codex Taskbar.app/Contents/Resources/AppIcon.icns"
sleep 2
screencapture -x "dist/smoke-installer-$arch.png"
hdiutil detach "$mount"
trap - EXIT
hdiutil convert "$rw" -format UDZO -o "dist/$name.dmg"
# 校验最终只读镜像，防止只验证 staging 却遗漏最终打包内容。
hdiutil attach "dist/$name.dmg" -mountpoint "$mount" -nobrowse -readonly
trap 'hdiutil detach "$mount" >/dev/null 2>&1 || true' EXIT
ls -la "$mount"
test "$(readlink "$mount/Applications")" = /Applications || { echo 'DMG Applications link invalid' >&2; exit 1; }
test -s "$mount/.DS_Store" || { echo 'DMG Finder layout missing' >&2; exit 1; }
test -s "$mount/.VolumeIcon.icns" || { echo 'DMG volume icon missing' >&2; exit 1; }
codesign --verify --deep --strict "$mount/Codex Taskbar.app"
hdiutil detach "$mount"
trap - EXIT
shasum -a 256 "dist/$name.zip" "dist/$name.dmg" > "dist/SHA256SUMS-macos-$arch.txt"
printf '%s\n' "$app" > dist/macos-app-path.txt
