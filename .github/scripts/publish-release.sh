#!/usr/bin/env bash
set -euo pipefail

if find release-assets -maxdepth 1 -type f -name '*-unsigned*' -print -quit | grep -q .; then
  echo 'Unsigned artifacts cannot be published in a tagged release:' >&2
  find release-assets -maxdepth 1 -type f -name '*-unsigned*' -print >&2
  exit 1
fi

expected=(
  zmux-linux-x86_64.deb
  zmux-linux-x86_64.tar.gz
  zmux-macos-aarch64.zip
  zmux-windows-x86_64.msi
)
mapfile -t actual < <(find release-assets -maxdepth 1 -type f \
  \( -name '*.deb' -o -name '*.tar.gz' -o -name '*.zip' -o -name '*.msi' \) \
  -printf '%f\n' | sort)
if [[ "${actual[*]}" != "${expected[*]}" ]]; then
  echo 'Tagged release artifacts do not match the required platform filenames.' >&2
  printf 'Expected: %s\n' "${expected[*]}" >&2
  printf 'Actual:   %s\n' "${actual[*]}" >&2
  exit 1
fi

assets=()
for name in "${expected[@]}"; do
  asset="release-assets/$name"
  [[ -s "$asset" ]] || {
    printf 'Release artifact is missing or empty: %s\n' "$asset" >&2
    exit 1
  }
  assets+=("$asset")
done
gh release create "$GITHUB_REF_NAME" "${assets[@]}" \
  --repo "$GITHUB_REPOSITORY" \
  --verify-tag \
  --title "zmux $GITHUB_REF_NAME" \
  --generate-notes
