#!/bin/sh
# Container entry for the serve-stress-alpine cron (.github/workflows/serve-stress-alpine.yml).
#
# Runs the hermetic serve stress harness (tools/serve_stress.py) inside an x86_64 Alpine (musl)
# container hard-capped at 8 GiB / few vCPUs, to surface performance choke points on a
# low-resource node: JVM startup + heap pressure for serve, `bazel query` server cost, git
# checkout latency, and OOM behavior that a 4-core/16 GiB glibc runner never shows.
#
# The host workflow stages everything this script needs into one directory (mounted here,
# path-independent -- the script locates it from $0):
#
#   <stage>/bazel-diff.jar            the //cli:bazel-diff_deploy.jar built on the glibc host
#   <stage>/bazel-glibc               the official bazel release binary matching .bazelversion
#   <stage>/.bazelversion             copied into fabricated workspaces (harness expects it)
#   <stage>/tools/serve_stress.py     the harness + its shared plumbing + this script
#   <stage>/tools/serve_harness.py
#
# Building bazel-diff *inside* Alpine would require a full glibc Bazel toolchain to work under
# musl -- the most fragile possible path -- so the jar is built on the host and only *run* here,
# on Alpine's own musl OpenJDK. The one genuinely glibc-bound piece is the `bazel` binary that
# serve shells out to for `bazel query`: official releases are glibc-linked and there is no
# official musl build. Two flavors are attempted, best first, each proven with a real probe
# query before the 30+ minute harness commits to it:
#
#   1. the official release binary under gcompat (glibc shim), with the server JVM redirected to
#      the musl OpenJDK via a system bazelrc (`startup --server_javabase=...`) -- this keeps the
#      bazel *version* identical to the other stress pipelines, so numbers compare cleanly;
#   2. a musl-native bazel from the Alpine package repos (version may lag; recorded as the
#      "flavor" in the footprint JSON so runs remain interpretable).
#
# Inputs (env): QUICK -- non-empty runs the reduced --quick profile.
# Outputs (written to <stage>): serve-stress-alpine-metrics.json, serve-stress-alpine-summary.md
# (harness), plus serve-stress-alpine-footprint.json and a footprint section appended to the
# summary (cgroup peak memory / OOM-kill counts -- the low-resource signal this pipeline exists
# to capture). Exit code is the harness's.
set -eu

STAGE=$(cd "$(dirname "$0")/.." && pwd)
echo "== stage: $STAGE"
cat /etc/alpine-release

# ------------------------------------------------------------------------------------------
# Packages: harness deps (python3, git + git daemon), the musl JDK that runs both the serve
# jar and (redirected) bazel server, and the glibc shim for the official bazel client.
# ------------------------------------------------------------------------------------------
apk add --no-cache python3 git git-daemon openjdk21-jdk gcompat libstdc++ libgcc

JAVA_HOME=/usr/lib/jvm/default-jvm
if [ ! -x "$JAVA_HOME/bin/java" ]; then
    JAVA_HOME=$(ls -d /usr/lib/jvm/java-21-openjdk* 2>/dev/null | head -1)
fi
[ -n "$JAVA_HOME" ] && [ -x "$JAVA_HOME/bin/java" ] || {
    echo "ERROR: no usable JDK under /usr/lib/jvm" >&2; exit 2; }
export JAVA_HOME
export PATH="$JAVA_HOME/bin:$PATH"
java -version 2>&1 | head -2

# ------------------------------------------------------------------------------------------
# The launcher the harness invokes (base.LAUNCHER = <stage>/bazel-bin/cli/bazel-diff, since
# REPO_ROOT is derived from serve_stress.py's location). A bazel-built java_binary launcher
# does not survive the host->container move (runfiles + embedded JDK paths), so a plain
# wrapper over the staged deploy jar stands in for it; --skip-build makes the harness use it.
# ------------------------------------------------------------------------------------------
mkdir -p "$STAGE/bazel-bin/cli"
cat > "$STAGE/bazel-bin/cli/bazel-diff" <<'EOF'
#!/bin/sh
exec java -jar "$(dirname "$0")/../../bazel-diff.jar" "$@"
EOF
chmod +x "$STAGE/bazel-bin/cli/bazel-diff"

# ------------------------------------------------------------------------------------------
# Pick a bazel that actually works on this musl userland (see flavor list in the header).
# ------------------------------------------------------------------------------------------

# A trivial query in a scratch module proves the client binary runs AND its server JVM comes
# up -- the two distinct ways a glibc bazel can die on musl. Also proves the loading phase
# (package parsing) works, which is all `serve` asks of bazel. The server is shut down after
# so probe servers never linger into the measured phases.
probe_bazel() {
    pd=/tmp/bazel-probe/ws
    rm -rf /tmp/bazel-probe
    mkdir -p "$pd"
    printf 'module(name = "probe", version = "0.0.0")\n' > "$pd/MODULE.bazel"
    printf 'filegroup(name = "x")\n' > "$pd/BUILD.bazel"
    if (cd "$pd" && timeout 600 "$1" --output_user_root=/tmp/bazel-probe/root query //:x); then
        (cd "$pd" && timeout 60 "$1" --output_user_root=/tmp/bazel-probe/root shutdown) || true
        return 0
    fi
    return 1
}

BAZEL=""
FLAVOR=""

if [ -x "$STAGE/bazel-glibc" ]; then
    # The official client would exec its *embedded* (glibc) JDK for the server, which gcompat
    # generally cannot carry; the system bazelrc redirects the server to the musl JDK instead.
    # Startup options are honored from /etc/bazel.bazelrc, and this applies to every workspace
    # the serve subprocess queries in -- no per-checkout .bazelrc needed.
    printf 'startup --server_javabase=%s\n' "$JAVA_HOME" > /etc/bazel.bazelrc
    if probe_bazel "$STAGE/bazel-glibc"; then
        BAZEL="$STAGE/bazel-glibc"
        FLAVOR="official-glibc+gcompat"
    else
        echo "WARN: official (glibc) bazel does not run under gcompat; trying Alpine packages" >&2
    fi
fi

if [ -z "$BAZEL" ]; then
    for pkg in bazel8 bazel7; do
        apk add --no-cache "$pkg" >/dev/null 2>&1 || continue
        for bin in bazel bazel-8 bazel-7 bazel8 bazel7; do
            p=$(command -v "$bin" 2>/dev/null) || continue
            if probe_bazel "$p"; then
                BAZEL="$p"
                FLAVOR="apk-$pkg"
                break 2
            fi
        done
    done
fi

[ -n "$BAZEL" ] || {
    echo "ERROR: no working bazel on this musl userland (gcompat + Alpine packages both failed)" >&2
    exit 2
}
echo "== bazel flavor: $FLAVOR ($BAZEL)"
"$BAZEL" version 2>/dev/null | head -3 || true

# ------------------------------------------------------------------------------------------
# Run the harness. set +e so a failing (or OOM-mangled) run still yields the footprint data
# below -- when memory is the choke point, the failing run is the most interesting one.
# ------------------------------------------------------------------------------------------
set +e
python3 "$STAGE/tools/serve_stress.py" \
    --skip-build \
    --bazel "$BAZEL" \
    --metrics-out "$STAGE/serve-stress-alpine-metrics.json" \
    --summary-out "$STAGE/serve-stress-alpine-summary.md" \
    ${QUICK:+--quick}
rc=$?
set -e
echo "== harness exit code: $rc"

# ------------------------------------------------------------------------------------------
# Container resource footprint from the cgroup (v2) this container runs in: peak memory
# against the 8 GiB cap and the oom/oom_kill counters. memory.peak needs kernel >= 5.19;
# absent files degrade to null rather than failing the run.
# ------------------------------------------------------------------------------------------
python3 - "$STAGE" "$FLAVOR" "$rc" <<'EOF'
import json, os, sys

stage, flavor, rc = sys.argv[1], sys.argv[2], int(sys.argv[3])

def read(name):
    try:
        return open(os.path.join("/sys/fs/cgroup", name)).read().strip()
    except OSError:
        return None

events = {}
for line in (read("memory.events") or "").splitlines():
    key, _, val = line.partition(" ")
    events[key] = int(val)

def cpu_quota():
    # docker --cpus is a CFS quota, not an affinity mask, so os.cpu_count() still reports the
    # host's cores; the real limit is cgroup cpu.max ("<quota> <period>", quota "max" = none).
    raw = read("cpu.max") or ""
    quota, _, period = raw.partition(" ")
    try:
        return round(int(quota) / int(period), 2)
    except ValueError:
        return None

footprint = {
    "bazel_flavor": flavor,
    "harness_exit_code": rc,
    "cpu_quota": cpu_quota(),
    "cpus_visible": os.cpu_count(),
    "memory_max_bytes": read("memory.max"),
    "memory_peak_bytes": read("memory.peak"),
    "memory_current_bytes": read("memory.current"),
    "memory_events": events,
}
with open(os.path.join(stage, "serve-stress-alpine-footprint.json"), "w") as f:
    json.dump(footprint, f, indent=2)

def gib(v):
    try:
        return f"{int(v) / (1 << 30):.2f} GiB"
    except (TypeError, ValueError):
        return "n/a"

section = [
    "",
    "## Container footprint (Alpine x86_64, 8 GiB cap)",
    "",
    "| bazel flavor | cpu quota | memory.max | memory.peak | oom | oom_kill |",
    "|---|---|---|---|---|---|",
    f"| {flavor} | {cpu_quota() or 'none'} (of {os.cpu_count()})"
    f" | {gib(read('memory.max'))} | {gib(read('memory.peak'))}"
    f" | {events.get('oom', 0)} | {events.get('oom_kill', 0)} |",
    "",
]
# Append so it lands under the harness's summary; if the harness died before writing one,
# this creates the file so the CI step summary still shows the environment numbers.
with open(os.path.join(stage, "serve-stress-alpine-summary.md"), "a") as f:
    f.write("\n".join(section))
print("\n".join(section))
EOF

exit $rc
