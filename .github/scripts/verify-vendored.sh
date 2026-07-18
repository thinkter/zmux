#!/usr/bin/env bash
set -euo pipefail

broken=0
while IFS= read -r -d '' entry; do
  mode="${entry%% *}"
  path="${entry#*$'\t'}"
  if [[ "$mode" == 120000 && ! -e "$path" ]]; then
    printf 'Broken tracked symbolic link: %s -> %s\n' "$path" "$(readlink "$path")" >&2
    broken=1
  fi
done < <(git ls-files --stage -z)
((broken == 0)) || exit 1

shopt -s nullglob
for manifest in vendor/*/Cargo.toml; do
  crate_dir="${manifest%/Cargo.toml}"
  licenses=("$crate_dir"/LICENSE* "$crate_dir"/COPYING*)
  readable_license=false
  for license in "${licenses[@]}"; do
    if [[ -f "$license" && -r "$license" ]]; then
      readable_license=true
      break
    fi
  done
  if ! $readable_license; then
    printf 'Vendored crate has no readable license: %s\n' "$crate_dir" >&2
    exit 1
  fi
done
