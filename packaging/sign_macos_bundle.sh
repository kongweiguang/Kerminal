#!/usr/bin/env bash
# @author kongweiguang
# 显式手动发布入口：在重建 DMG/updater archive 前对完整 app 内外顺序签名、公证并回读。
# 此脚本不导入证书或创建 Secret；调用者必须先准备隔离 keychain 和 notarytool profile。
set -euo pipefail

if [[ $# != 1 || "$1" != *.app || ! -d "$1/Contents/MacOS" ]]; then
  echo 'usage: sign_macos_bundle.sh /absolute/path/Kerminal.app' >&2
  exit 2
fi
: "${APPLE_SIGNING_IDENTITY:?Set the Developer ID Application identity}"
: "${APPLE_NOTARY_PROFILE:?Set the preconfigured notarytool keychain profile}"
bundle="$1"
for name in kerminal-plugin-host kerminal-plugin kerminal; do
  executable="$bundle/Contents/MacOS/$name"
  [[ -f "$executable" && ! -L "$executable" ]] || { echo "Missing bundle executable: $name" >&2; exit 1; }
  codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$executable"
  codesign --verify --strict --verbose=2 "$executable"
done
# 迁移 helper 也是应用包内的 Mach-O，不依赖 --deep 签名替我们找到它。
while IFS= read -r -d '' executable; do
  if file "$executable" | grep -q 'Mach-O'; then
    codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$executable"
  fi
done < <(find "$bundle/Contents/Resources" -type f -print0)
codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$bundle"
codesign --verify --deep --strict --verbose=2 "$bundle"
archive="$(mktemp -d)/Kerminal-notary.zip"
trap 'rm -f "$archive"; rmdir "$(dirname "$archive")"' EXIT
ditto -c -k --keepParent "$bundle" "$archive"
xcrun notarytool submit "$archive" --keychain-profile "$APPLE_NOTARY_PROFILE" --wait
xcrun stapler staple "$bundle"
xcrun stapler validate "$bundle"
spctl --assess --type execute --verbose=2 "$bundle"
