#!/usr/bin/env bash
# Host-side driver: stages artifacts, builds the Linux benchmark image, and runs
# the cold-vs-warm benchmark + local-driver orchestrator inside a container.
#
# Prereqs (run from repo root):
#   make release_rust_binary_linux          # amd64: bazel-bin/release/bazel-diff-rust-linux-amd64
#   make release_rust_binary_linux_arm64    # arm64: bazel-bin/release/bazel-diff-rust-linux-arm64
#   (cd tools/firecracker && GOOS=linux GOARCH="$ARCH" go build -o /tmp/bazel-diff-snap-linux .)
#
# Usage:
#   tools/firecracker/bench/run_docker_bench.sh [PKGS] [ITERS]
set -euo pipefail

PKGS=${1:-11500}
ITERS=${2:-2}
ARCH=${ARCH:-arm64}            # docker host arch (arm64 on Apple Silicon)
REPO_ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
BENCH_DIR="$REPO_ROOT/tools/firecracker/bench"

# The statically linked musl binary for the container's arch (see .bazelrc's
# release-musl / release-musl-arm64 configs); the Bazel-derived asset name uses
# amd64/arm64, matching the docker arch names.
BIN="${BAZEL_DIFF_BIN:-$REPO_ROOT/bazel-bin/release/bazel-diff-rust-linux-$ARCH}"
SNAP="${SNAP:-/tmp/bazel-diff-snap-linux-$ARCH}"
[ -f "$BIN" ] || { echo "missing $BIN — run: make release_rust_binary_linux (amd64) or make release_rust_binary_linux_arm64 (arm64)"; exit 1; }
[ -f "$SNAP" ] || { echo "missing $SNAP — cross-compile the go binary first"; exit 1; }

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp -L "$BIN" "$STAGE/bazel-diff"
cp "$SNAP" "$STAGE/bazel-diff-snap"
cp "$BENCH_DIR/Dockerfile" "$BENCH_DIR/gen_project.py" \
   "$BENCH_DIR/bench.py" "$BENCH_DIR/run_in_container.sh" "$STAGE/"

RESULTS=${RESULTS:-"$REPO_ROOT/.bench-results"}
mkdir -p "$RESULTS"

echo "=== building image (arch=$ARCH) ==="
docker build --build-arg BAZELISK_ARCH="$ARCH" -t bazel-diff-bench "$STAGE"

echo "=== running benchmark: PKGS=$PKGS ITERS=$ITERS ==="
docker run --rm \
    -e PKGS="$PKGS" -e ITERS="$ITERS" \
    -v "$RESULTS:/results" \
    bazel-diff-bench

echo "=== results in $RESULTS ==="
cat "$RESULTS/target_count.txt" 2>/dev/null || true
cat "$RESULTS/report.json" 2>/dev/null || true
