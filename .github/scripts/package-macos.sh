#!/usr/bin/env bash
set -euo pipefail

app_bundle="dist/zmux.app"
bundle_id="io.github.thinkter.zmux"
zmux_version="$(awk '/^\[package\]/{in_package=1; next} in_package && /^version = / {sub(/^[^\"]*\"/, ""); sub(/\".*$/, ""); print; exit}' Cargo.toml)"

mkdir -p "$app_bundle/Contents/MacOS" "$app_bundle/Contents/Resources"
install -m 755 target/release/zmux "$app_bundle/Contents/MacOS/zmux"
install -m 644 LICENSE "$app_bundle/Contents/Resources/LICENSE"
install -m 644 packaging/icons/macos/zmux.icns "$app_bundle/Contents/Resources/zmux.icns"
cp packaging/macos/Info.plist "$app_bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $zmux_version" "$app_bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $zmux_version" "$app_bundle/Contents/Info.plist"

plutil -lint "$app_bundle/Contents/Info.plist"
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app_bundle/Contents/Info.plist")" = "$bundle_id"
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIconFile' "$app_bundle/Contents/Info.plist")" = 'zmux.icns'
test -s "$app_bundle/Contents/Resources/zmux.icns"

credentials=(
  APPLE_APP_PASSWORD
  APPLE_CERTIFICATE_BASE64
  APPLE_CERTIFICATE_PASSWORD
  APPLE_ID
  APPLE_SIGNING_IDENTITY
  APPLE_TEAM_ID
)
missing=()
for value in "${credentials[@]}"; do
  [[ -n "${!value}" ]] || missing+=("$value")
done
if [[ "$GITHUB_REF" == refs/tags/v* && ${#missing[@]} -gt 0 ]]; then
  printf 'Tagged macOS releases require signing credentials; missing: %s\n' "${missing[*]}" >&2
  exit 1
fi

if [[ "$GITHUB_REF" == refs/tags/v* ]]; then
  keychain="$RUNNER_TEMP/zmux-signing.keychain-db"
  certificate="$RUNNER_TEMP/zmux-signing.p12"
  keychain_password="$(openssl rand -hex 24)"
  printf '%s' "$APPLE_CERTIFICATE_BASE64" | base64 --decode > "$certificate"
  security create-keychain -p "$keychain_password" "$keychain"
  security set-keychain-settings -lut 21600 "$keychain"
  security unlock-keychain -p "$keychain_password" "$keychain"
  security import "$certificate" -P "$APPLE_CERTIFICATE_PASSWORD" -A -t cert -f pkcs12 -k "$keychain"
  security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain"
  security list-keychains -d user -s "$keychain"

  codesign --force --options runtime --sign "$APPLE_SIGNING_IDENTITY" \
    --identifier "$bundle_id" --timestamp "$app_bundle"
  codesign --verify --deep --strict --verbose=2 "$app_bundle"

  notarization_upload="$RUNNER_TEMP/zmux-notarization.zip"
  ditto -c -k --keepParent "$app_bundle" "$notarization_upload"
  xcrun notarytool submit "$notarization_upload" \
    --apple-id "$APPLE_ID" \
    --password "$APPLE_APP_PASSWORD" \
    --team-id "$APPLE_TEAM_ID" \
    --wait
  xcrun stapler staple "$app_bundle"
  xcrun stapler validate "$app_bundle"
  spctl --assess --type execute --verbose=4 "$app_bundle"
  archive="dist/zmux-macos-aarch64.zip"
else
  codesign --force --sign - --identifier "$bundle_id" --timestamp=none "$app_bundle"
  codesign --verify --deep --strict "$app_bundle"
  archive="dist/zmux-macos-aarch64-unsigned.zip"
fi

launch_log="$RUNNER_TEMP/zmux-macos-smoke.log"
"$app_bundle/Contents/MacOS/zmux" >"$launch_log" 2>&1 &
app_pid=$!
sleep 5
if ! kill -0 "$app_pid" >/dev/null 2>&1; then
  wait "$app_pid" || true
  cat "$launch_log" >&2
  echo "Packaged macOS application did not stay running" >&2
  exit 1
fi
kill "$app_pid"
wait "$app_pid" || true

# ditto preserves bundle metadata and a stapled notarization ticket.
ditto -c -k --keepParent "$app_bundle" "$archive"
