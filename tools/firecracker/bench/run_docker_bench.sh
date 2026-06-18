#!/usr/bin/env bash
# Host-side driver: stages artifacts, builds the Linux benchmark image, and runs
# the cold-vs-warm benchmark + local-driver orchestrator inside a container.
#
# Prereqs (run from repo root):
#   bazel build //cli:bazel-diff_deploy.jar
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

JAR="$REPO_ROOT/bazel-bin/cli/bazel-diff_deploy.jar"
SNAP="${SNAP:-/tmp/bazel-diff-snap-linux-$ARCH}"
[ -f "$JAR" ] || { echo "missing $JAR — run: bazel build //cli:bazel-diff_deploy.jar"; exit 1; }
[ -f "$SNAP" ] || { echo "missing $SNAP — cross-compile the go binary first"; exit 1; }

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp "$JAR" "$STAGE/bazel-diff.jar"
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
