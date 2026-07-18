#!/usr/bin/env bash
set -euo pipefail

desktop_file="packaging/linux/io.github.thinkter.zmux.desktop"
deb_root="dist/deb-root"
archive_root="dist/zmux-linux-x86_64"
zmux_version="$(cargo metadata --locked --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "zmux") | .version')"

desktop-file-validate "$desktop_file"
mkdir -p \
  "$deb_root/DEBIAN" \
  "$deb_root/usr/bin" \
  "$deb_root/usr/share/applications" \
  "$deb_root/usr/share/doc/zmux"
install -m 755 target/release/zmux "$deb_root/usr/bin/zmux"
install -m 644 "$desktop_file" "$deb_root/usr/share/applications/io.github.thinkter.zmux.desktop"
install -m 755 packaging/linux/postinst "$deb_root/DEBIAN/postinst"
install -m 755 packaging/linux/postrm "$deb_root/DEBIAN/postrm"
install -m 644 LICENSE "$deb_root/usr/share/doc/zmux/copyright"

mkdir -p debian
install -m 644 packaging/linux/control debian/control
install -m 644 packaging/linux/changelog debian/changelog
dpkg-shlibdeps -Tdist/zmux.substvars -e"$deb_root/usr/bin/zmux"
dpkg-gencontrol \
  -cpackaging/linux/control \
  -Tdist/zmux.substvars \
  -pzmux \
  -v"$zmux_version" \
  -P"$deb_root"
dpkg-deb --root-owner-group --build "$deb_root" dist/zmux-linux-x86_64.deb
dpkg-deb --info dist/zmux-linux-x86_64.deb

# The tarball is the distro-neutral artifact. It intentionally contains the
# console-capable `zmux` binary; `zmux-gui` only differs on Windows, where it
# opts into the GUI subsystem.
mkdir -p "$archive_root"
install -m 755 target/release/zmux "$archive_root/zmux"
install -m 644 LICENSE "$archive_root/LICENSE"
install -m 644 README.md "$archive_root/README.md"
install -m 644 "$desktop_file" "$archive_root/io.github.thinkter.zmux.desktop"
tar -C dist -czf dist/zmux-linux-x86_64.tar.gz zmux-linux-x86_64
tar -tzf dist/zmux-linux-x86_64.tar.gz

sudo dpkg --install dist/zmux-linux-x86_64.deb
test "$(command -v zmux)" = /usr/bin/zmux
desktop-file-validate /usr/share/applications/io.github.thinkter.zmux.desktop
test "$(grep '^Exec=' /usr/share/applications/io.github.thinkter.zmux.desktop)" = 'Exec=zmux'
dbus-run-session -- xvfb-run -a sh -eu -c '
  gtk-launch io.github.thinkter.zmux >"$RUNNER_TEMP/zmux-linux-smoke.log" 2>&1 &
  launcher=$!
  trap '\''pkill -x zmux >/dev/null 2>&1 || true; kill "$launcher" >/dev/null 2>&1 || true'\'' EXIT
  attempt=0
  while [ "$attempt" -lt 40 ]; do
    if pgrep -x zmux >/dev/null; then
      sleep 1
      pgrep -x zmux >/dev/null && exit 0
      cat "$RUNNER_TEMP/zmux-linux-smoke.log" >&2
      echo "Packaged Linux application exited during launch smoke test" >&2
      exit 1
    fi
    attempt=$((attempt + 1))
    sleep 0.25
  done
  cat "$RUNNER_TEMP/zmux-linux-smoke.log" >&2
  echo "Packaged Linux application did not stay running" >&2
  exit 1
'
