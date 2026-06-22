# Firecracker snapshot harness for bazel-diff

Implementation + benchmarking harness for the Firecracker snapshot design
([`docs/firecracker-snapshots.md`](../../docs/firecracker-snapshots.md), PR #376).

The goal: **instant starts** of bazel-diff on large monorepos by restoring a
microVM whose Bazel server already has the build graph loaded and external repos
fetched, so the PR-time path only re-analyses changed packages.

This directory contains everything needed to build, validate, and run that:

| Piece | What it is | Runs on |
| --- | --- | --- |
| `bench/gen_project.py` | Synthetic large-Bazel-project generator (no external toolchains) | anywhere |
| `bench/bench.py` | Cold-vs-warm analysis-time benchmark (the addressable win) | anywhere |
| `bazel-diff fingerprint` / `warmup` | CLI hooks (Kotlin, in `//cli`) | anywhere |
| `bazel-diff-snap` (this Go module) | `record` / `consume` orchestrator | Linux+KVM (real), anywhere (local driver) |

## Why the cold-vs-warm benchmark proves the win

A restored snapshot ≈ a **warm** Bazel server (graph loaded, externals fetched).
So the per-PR analysis-time a snapshot can save is bounded by:

```
win = cold `generate-hashes`  -  warm `generate-hashes`
```

`bench.py` measures both on a generated workspace and asserts the warm output is
**byte-identical** to the cold output (the RFC's core correctness claim). On a
~15k-target synthetic graph this is already ~70% faster warm; on a real monorepo
(bzlmod resolution + minutes of cold start) the absolute win is far larger.

### Run the benchmark (any platform)

```bash
# 1. generate a large workspace with two revisions to diff
python3 bench/gen_project.py --out /tmp/bigproj --packages 3000 \
    --targets-per-package 4 --git
# -> prints {"base_sha": "...", "target_sha": "...", ...}

# 2. build bazel-diff
bazel run //:bazel-diff --script_path=/tmp/bazel_diff

# 3. benchmark cold vs warm
python3 bench/bench.py \
    --workspace /tmp/bigproj \
    --base-sha <base_sha> --target-sha <target_sha> \
    --bazel "$(which bazel)" --bazel-diff /tmp/bazel_diff \
    --iterations 3 --json /tmp/report.json
```

`gen_project.py` builds a **layered** genrule DAG: packages are partitioned into
`--layers` bands and a package depends on a few packages in the previous band.
This gives real depth + width and real source files for `SourceFileHasher` to
hash — all with **zero** external toolchains, so the cold path actually
re-analyses on every run and the benchmark is reproducible. Layering bounds the
graph *depth* (default 40), which matters at scale: bazel-diff hashes
transitively with a per-level dependency-path set, so a single N-deep chain would
cost O(N²) memory — a layered, modular graph (like a real monorepo) does not.

### Run on Linux at scale (Docker)

The whole flow above also runs in a Linux container — the actual CI target OS —
via [`bench/run_docker_bench.sh`](bench/run_docker_bench.sh):

```bash
bazel build //cli:bazel-diff_deploy.jar
(cd tools/firecracker && GOOS=linux GOARCH=arm64 go build -o /tmp/bazel-diff-snap-linux-arm64 .)
ARCH=arm64 SNAP=/tmp/bazel-diff-snap-linux-arm64 \
    tools/firecracker/bench/run_docker_bench.sh 11500 2   # ~150k targets, 2 iters
# results land in .bench-results/ (report.json, target_count.txt, impacted.txt)
```

The image bundles a JDK, bazelisk, git, the bazel-diff fat jar, and the Go
orchestrator. It does **not** run Firecracker itself — that needs `/dev/kvm`,
which Docker-for-Mac does not expose; real microVM record/consume runs on the
self-hosted Linux+KVM host.

## CLI hooks (`//cli`)

Two picocli subcommands implement RFC §4 (Phase 1, pure Kotlin, unit-tested):

- **`bazel-diff fingerprint`** — computes the snapshot cache key over the inputs
  that affect the build graph (bazel version, `MODULE.bazel.lock`, `.bazelrc`,
  bazel-diff version, flag set) and writes it as JSON. Used to decide whether a
  snapshot is safe to consume.
- **`bazel-diff warmup`** — the record-side entrypoint: runs `generate-hashes`
  for the base revision, writes `base_hashes.json` + `fingerprint.json` to known
  paths, and exits `0` only once the server is warm (the host's "safe to
  snapshot" signal). It *extends* `generate-hashes`, so the baked base hashes are
  byte-identical to a cold run.

## Orchestrator (`bazel-diff-snap`, this Go module)

Dependency-free (stdlib only): the Firecracker REST API is spoken over a unix
socket with `net/http`, so the tool builds as a static CI binary with no module
downloads. *(This is a deliberate deviation from the RFC's `firecracker-go-sdk`
suggestion — the API surface we need is small and a zero-dependency static binary
is simpler to ship to CI.)*

```bash
go build -o bazel-diff-snap .          # native
GOOS=linux GOARCH=amd64 go build .     # static Linux binary for CI
go test ./...                          # pure logic + API client, runs anywhere
```

### Drivers

- **`--driver local`** (default) — runs `warmup` / `generate-hashes` /
  `get-impacted-targets` directly on the host, no VM. Exercises the full
  store + fingerprint + resolve pipeline; works on macOS. This is what the unit
  tests and local end-to-end runs use.
- **`--driver firecracker`** — boots/snapshots/restores a real microVM via the
  Firecracker API. Requires Linux + `/dev/kvm`, a prepared kernel + rootfs base
  image (`bench/build_guest_image.sh`), and a host TAP (`bench/setup_tap.sh`) so
  the orchestrator can ssh into the guest. This is the production path (RFC §6).

  The guest gets a virtio-net device backed by the host TAP; the orchestrator
  bakes a static `ip=` directive into the kernel cmdline so the guest is
  reachable with no DHCP. Addressing defaults match the guest image's
  `fcnet-setup.sh` MAC→IP convention (MAC `06:00:AC:10:00:02` → `172.16.0.2/30`).
  TAP creation needs `CAP_NET_ADMIN`, so it is the operator's job (run
  `setup_tap.sh` once); the orchestrator stays privilege-free and only checks the
  TAP exists before a restore. Relevant flags:

  ```
  --kernel <path>      guest kernel (rootfs.base.ext4 must sit beside it)
  --guest-addr <u@h>   guest ssh address, e.g. root@172.16.0.2
  --tap-device <name>  host TAP backing the guest NIC, e.g. fc-tap0
  --guest-ip / --host-ip / --netmask / --guest-mac   static TAP addressing
  --guest-key <path>   ssh identity trusted by the guest
  --vcpus / --mem-mib  guest sizing
  ```

### Building the guest image

```bash
# 1. host TAP (once per host; needs root)
sudo tools/firecracker/bench/setup_tap.sh         # fc-tap0, host 172.16.0.1/30

# 2. kernel + rootfs.base.ext4 with JDK + bazel + git + bazel-diff + workspace
sudo -E OUT=/tmp/fc-image \
    BAZEL_DIFF_JAR=bazel-bin/cli/bazel-diff_deploy.jar \
    BAZEL_BIN=$(which bazelisk) WORKSPACE_SRC=/tmp/bigproj \
    SSH_PUBKEY=~/.ssh/fc_guest.pub \
    tools/firecracker/bench/build_guest_image.sh
```

The image switches sshd from systemd socket-activation to a standalone
always-running daemon — socket-activated sshd does not service connections
reliably after a snapshot restore.

### record / consume

```bash
# RECORD a snapshot for a base revision (on merge to master / nightly)
bazel-diff-snap record \
    --workspace /path/to/repo --base-sha <sha> \
    --store /snapshots --bazel "$(which bazel)" --bazel-diff /tmp/bazel_diff

# CONSUME on a PR: resolve a compatible snapshot, restore, diff
bazel-diff-snap consume \
    --workspace /path/to/repo --target-sha <sha> \
    --store /snapshots --out impacted.txt \
    --bazel "$(which bazel)" --bazel-diff /tmp/bazel_diff
# exit 0 = wrote impacted.txt | exit 2 = no compatible snapshot, run cold path | exit 1 = error
```

`consume` is **fail-safe** (RFC §5.2): it fingerprints the *target* environment
and only uses a snapshot whose fingerprint matches **and** whose base SHA is an
ancestor of the target (nearest ancestor wins, to minimise re-analysis). Any
mismatch → exit `2` so the caller runs the existing cold
[`bazel-diff-example.sh`](../../bazel-diff-example.sh) path. A stale snapshot is
never silently trusted.

### Snapshot store layout

```
<store>/<fingerprint>/<baseSHA>/
  mem_file          guest memory image (diff snapshot)   [firecracker driver]
  vmstate           Firecracker microVM state            [firecracker driver]
  rootfs.backing    frozen read-only disk image          [firecracker driver]
  base_hashes.json  produced by `bazel-diff warmup`
  fingerprint.json  the cache key + flag set
  metadata.json     fingerprint, baseSHA, versions, created-at
```

## Status

- [x] Synthetic generator + cold/warm benchmark — validates the analysis-time win
- [x] `fingerprint` + `warmup` CLI hooks (unit-tested)
- [x] Go orchestrator: store/resolve/fingerprint-match/API client (unit-tested),
      `local` driver end-to-end
- [x] `firecracker` driver: networking (TAP-backed NIC + static `ip=`) wired into
      record/consume, restore-time TAP precondition, unit-tested API client
- [x] Guest image + TAP build scripts (`bench/build_guest_image.sh`,
      `bench/setup_tap.sh`)
- [x] Real-microVM canary as a gated Go integration test (`fc_integration_test.go`,
      build tag `fcintegration`) — runs `fcDriver.record` + `consume` end-to-end
- [x] One-click CI workflow that builds the image, boots a real microVM, and
      asserts snapshot-consumed == cold:
      [`.github/workflows/firecracker-e2e.yml`](../../.github/workflows/firecracker-e2e.yml)
      (`workflow_dispatch`; runs on a mainline-kernel x86_64 runner with `/dev/kvm`)

### Validation notes (aarch64 / KVM)

The driver was exercised on real Firecracker + `/dev/kvm`. **What works:** microVM
boot, the full API sequence incl. `PUT /network-interfaces`, ssh into the guest
over the TAP, `PATCH /vm` pause, `PUT /snapshot/create`, and `PUT /snapshot/load`
+ resume — devices reconnect and the restored guest's *kernel* networking is live
(host↔guest ping and the TCP handshake to sshd both succeed).

**Known host limitation:** on a downstream 16 KB-page kernel (Raspberry Pi 5,
`rpi-2712`), the restored guest's *userspace* does not advance after resume
(sshd accepts the connection but never sends its banner), with both the 5.10 and
6.1 CI guest kernels — an aarch64 guest-timer-restore quirk below the driver, not
in the orchestrator's API sequence. Trigger the `Firecracker snapshot e2e`
workflow (a mainline-kernel, 4 KB-page x86_64 runner with `/dev/kvm`) to land the
green end-to-end run.
