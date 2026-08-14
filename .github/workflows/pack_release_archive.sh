#!/usr/bin/env bash

# Packs the release source archive exactly as it ships in a GitHub release
# (and therefore exactly as the Bazel Central Registry consumes it).
# Shared by release_prep.sh (release time) and bcr_consumer.yaml (CI check)
# so the archive the check verifies can never drift from the released one.

set -o errexit -o nounset -o pipefail

OUT=${1:?usage: pack_release_archive.sh <output-tar-gz>}

mkdir -p "$(dirname "$OUT")"
tar --exclude-vcs \
  --exclude=bazel-* \
  --exclude=target \
  --exclude=.github \
  --exclude=archives \
  --exclude=__pycache__ \
  --exclude='*.pyc' \
  -zcf "$OUT" .
