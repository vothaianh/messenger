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
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
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
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
# Sign with a stable identity when one exists, so Keychain's "Always Allow"
# survives rebuilds (ad-hoc signatures change every build). Override with
# SIGN_IDENTITY=... ; falls back to ad-hoc signing. Distribution builds use
# the "Developer ID Application" certificate (needed outside this Mac).
find_identity() { security find-identity -v -p codesigning | sed -n "s/.*\"\($1:[^\"]*\)\".*/\1/p" | head -1; }
DEV_IDENTITY="$(find_identity "Apple Development")"
if [ -n "$DIST" ]; then
  IDENTITY="${SIGN_IDENTITY:-$(find_identity "Developer ID Application")}"
  IDENTITY="${IDENTITY:-$DEV_IDENTITY}"
else
  IDENTITY="${SIGN_IDENTITY:-$DEV_IDENTITY}"
fi
if [ -n "$DIST" ] && [ -n "$IDENTITY" ]; then
  codesign --force --deep --options runtime --timestamp -s "$IDENTITY" "$APP"
else
  codesign --force --deep -s "${IDENTITY:--}" "$APP"
fi
echo "Signed with: ${IDENTITY:-ad-hoc} (version $VERSION)"
# Sign the bare binary (used by debug-* commands) with the development
# identity and the app's identifier, so it matches the Keychain grant.
if [ -f target/release/messenger-rs ]; then
  codesign --force -s "${DEV_IDENTITY:--}" --identifier dev.local.messenger-desktop target/release/messenger-rs
fi
echo "Built $APP"
if [ -n "$DIST" ]; then
  ZIP="target/Messenger Desktop.zip"
  rm -f "$ZIP"
  ditto -c -k --keepParent "$APP" "$ZIP"
  # Notarize when credentials were saved once with
  #   xcrun notarytool store-credentials messenger-notary --apple-id ... --team-id ...
  # (NOTARY_PROFILE picks another name), then staple the ticket and re-zip.
  NOTARY_PROFILE="${NOTARY_PROFILE:-messenger-notary}"
  case "$IDENTITY" in
    "Developer ID Application:"*)
      if xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
        xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait
        xcrun stapler staple "$APP"
        rm -f "$ZIP"
        ditto -c -k --keepParent "$APP" "$ZIP"
        echo "Notarized and stapled"
      else
        echo "Not notarized: no notarytool profile \"$NOTARY_PROFILE\" (see bundle.sh)"
      fi
      ;;
  esac
  echo "Zipped $ZIP"
fi
