#!/bin/sh
# Build a release binary and wrap it in "Messenger Desktop.app".
set -e
cd "$(dirname "$0")"
# DIST=1: optimized, stripped build (profile "dist"), signed with the hardened
# runtime and a secure timestamp, plus a zip of the app.
PROFILE=release
if [ -n "$DIST" ]; then PROFILE=dist; fi
cargo build --profile "$PROFILE"
APP="target/Messenger Desktop.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# App icon. With Xcode: compile assets/AppIcon.icon (Icon Composer format,
# light + dark + tinted appearances) into Assets.car plus a fallback .icns.
# Without Xcode: render assets/icon.svg to a light-only .icns.
if xcrun --find actool >/dev/null 2>&1; then
  cargo run --release --quiet --example render_svg -- assets/bolt-light.svg assets/AppIcon.icon/Assets/bolt-light.png
  cargo run --release --quiet --example render_svg -- assets/bolt-dark.svg assets/AppIcon.icon/Assets/bolt-dark.png
  xcrun actool assets/AppIcon.icon --compile "$APP/Contents/Resources" --platform macosx \
    --minimum-deployment-target 11.0 --app-icon AppIcon \
    --output-partial-info-plist target/AppIcon-partial.plist >/dev/null
else
  cargo run --release --quiet --example make_icon -- target/AppIcon.iconset
  iconutil -c icns target/AppIcon.iconset -o "$APP/Contents/Resources/AppIcon.icns"
fi
cp "target/$PROFILE/messenger-rs" "$APP/Contents/MacOS/Messenger Desktop"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Messenger Desktop</string>
  <key>CFBundleIdentifier</key><string>dev.local.messenger-desktop</string>
  <key>CFBundleExecutable</key><string>Messenger Desktop</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleIconName</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
# Sign with a stable identity when one exists, so Keychain's "Always Allow"
# survives rebuilds (ad-hoc signatures change every build). Override with
# SIGN_IDENTITY=... ; falls back to ad-hoc signing.
IDENTITY="${SIGN_IDENTITY:-$(security find-identity -v -p codesigning | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' | head -1)}"
if [ -n "$DIST" ] && [ -n "$IDENTITY" ]; then
  codesign --force --deep --options runtime --timestamp -s "$IDENTITY" "$APP"
else
  codesign --force --deep -s "${IDENTITY:--}" "$APP"
fi
echo "Signed with: ${IDENTITY:-ad-hoc}"
# Sign the bare binary (used by debug-* commands) with the same identity and
# identifier, so it matches the Keychain grant and doesn't prompt either.
codesign --force -s "${IDENTITY:--}" --identifier dev.local.messenger-desktop target/release/messenger-rs
echo "Built $APP"
if [ -n "$DIST" ]; then
  rm -f "target/Messenger Desktop.zip"
  ditto -c -k --keepParent "$APP" "target/Messenger Desktop.zip"
  echo "Zipped target/Messenger Desktop.zip"
fi
