#!/usr/bin/env bash
set -euo pipefail

binary=${1:?usage: assert_static_binary.sh <binary>}
elf=$(readelf --program-headers --dynamic "$binary")
if [[ "$elf" == *INTERP* || "$elf" == *NEEDED* ]]; then
  echo "$binary requires a dynamic loader or shared libraries" >&2
  exit 1
fi

echo "$binary is statically linked"
