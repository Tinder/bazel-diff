#!/usr/bin/env bash

# Asserts that a published Linux binary is statically linked, i.e. that it
# carries no dependency on the glibc of the runner that built it. Shared by the
# `release-artifacts` job in ci.yaml (per-PR check) and the `rust-binaries` job
# in release.yaml (before the asset is uploaded), so a regression cannot reach a
# release without failing a PR first.
#
# The build flag that makes this true is --config=release-musl (see .bazelrc).
# Losing it does not break the build -- it silently produces a glibc-linked
# binary -- which is exactly why this is asserted rather than assumed.

set -o errexit -o nounset -o pipefail

BINARY=${1:?usage: assert_static_binary.sh <binary>}

echo "Checking $BINARY"
# -L because bazel-bin/release/<asset> is a symlink into the output base, and
# `file` would otherwise describe the link rather than the binary.
file -L "$BINARY"

# Both "statically linked" and "static-pie linked" are acceptable; a dynamically
# linked binary says "dynamically linked (uses shared libs)" instead.
if ! file -L "$BINARY" | grep -q "static"; then
  echo "FAIL: $BINARY is not statically linked" >&2
  exit 1
fi

# The definitive check: a dynamic loader (PT_INTERP) is what ties a binary to
# the host's libc, and NEEDED entries are the libraries it would load.
if readelf --program-headers "$BINARY" | grep -q "INTERP"; then
  echo "FAIL: $BINARY requests a dynamic loader" >&2
  readelf --program-headers "$BINARY" | grep -A 2 "INTERP" >&2
  exit 1
fi

if readelf --dynamic "$BINARY" 2>/dev/null | grep -q "NEEDED"; then
  echo "FAIL: $BINARY has shared library dependencies" >&2
  readelf --dynamic "$BINARY" | grep "NEEDED" >&2
  exit 1
fi

# Versioned glibc symbols (GLIBC_2.34 and friends) are the failure this whole
# exercise is about: they make a binary refuse to start on older distributions.
if strings -a "$BINARY" | grep -q "GLIBC_"; then
  echo "FAIL: $BINARY references versioned glibc symbols" >&2
  exit 1
fi

# Nothing above proves the binary runs, only that it is self-contained.
"$BINARY" --version

echo "OK: $BINARY is statically linked"
