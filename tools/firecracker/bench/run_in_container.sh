#!/usr/bin/env bash
# Entrypoint executed inside the Linux benchmark container.
#
# 1. generates a large Bazel project (PKGS packages, two git revisions)
# 2. pre-warms bazel (download toolchain + fetch BCR deps + populate repo cache)
#    so those one-time costs are NOT counted as analysis time
# 3. runs the cold-vs-warm benchmark (the addressable snapshot win)
# 4. runs the local-driver orchestrator record/consume end-to-end
#
# Outputs land in /results (bind-mounted from the host).
set -euo pipefail

PKGS=${PKGS:-11500}
TPP=${TPP:-4}
ITERS=${ITERS:-2}
WS=/work/proj
RESULTS=/results
mkdir -p "$RESULTS"

echo "=== generating project: ${PKGS} packages x ${TPP} targets/pkg ==="
python3 /opt/bench/gen_project.py --out "$WS" --packages "$PKGS" \
    --targets-per-package "$TPP" --git | tee "$RESULTS/gen.json"

BASE=$(python3 -c "import json;print(json.load(open('$RESULTS/gen.json'))['base_sha'])")
TARGET=$(python3 -c "import json;print(json.load(open('$RESULTS/gen.json'))['target_sha'])")

echo "=== pre-warm bazel (toolchain + BCR fetch + repo cache) ==="
( cd "$WS" && bazel version >/dev/null )
TGTS=$( cd "$WS" && bazel query 'deps(//...:all-targets)' 2>/dev/null | wc -l )
echo "TARGET_COUNT=$TGTS" | tee "$RESULTS/target_count.txt"

echo "=== cold-vs-warm benchmark (${ITERS} iters) ==="
python3 /opt/bench/bench.py \
    --workspace "$WS" --base-sha "$BASE" --target-sha "$TARGET" \
    --bazel "$(command -v bazel)" --bazel-diff /usr/local/bin/bazel-diff \
    --iterations "$ITERS" --json "$RESULTS/report.json"

echo "=== orchestrator: local driver record + consume ==="
bazel-diff-snap record --workspace "$WS" --base-sha "$BASE" \
    --store /work/store --bazel "$(command -v bazel)" --bazel-diff bazel-diff
bazel-diff-snap consume --workspace "$WS" --target-sha "$TARGET" \
    --store /work/store --out "$RESULTS/impacted.txt" \
    --bazel "$(command -v bazel)" --bazel-diff bazel-diff
echo "impacted_targets=$(wc -l < "$RESULTS/impacted.txt")" | tee "$RESULTS/impacted_count.txt"

echo "=== done; results in $RESULTS ==="
