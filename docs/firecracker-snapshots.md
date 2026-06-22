# RFC: Firecracker snapshots for instant bazel-diff starts

**Status:** Draft
**Audience:** bazel-diff maintainers / contributors
**Scope decided:** CLI hooks in the Kotlin tool + a Go orchestration tool, capturing a
*full warm Bazel server*, targeting *self-hosted CI* (we control the host kernel and CPU model).

---

## 1. Motivation

bazel-diff's own JVM CLI starts in well under a second. That is not where the time goes. The
canonical workflow ([`bazel-diff-example.sh`](../bazel-diff-example.sh)) is:

1. `bazel run :bazel-diff` — build the tool
2. `git checkout <prev>` → `generate-hashes` → runs `bazel query deps(//...:all-targets)`
3. `git checkout <final>` → `generate-hashes` → another `bazel query`
4. `get-impacted-targets` — cheap, pure JSON diff

The cost is the `bazel query` in steps 2/3, which forces:

- **Bazel server startup** (JVM warmup).
- **External-repo / bzlmod resolution + repository-cache fetch.** `BazelQueryService` even shells
  out to `bazel mod dump_repo_mapping` and `bazel mod show_repo`
  ([`BazelQueryService.kt`](../cli/src/main/kotlin/com/bazel_diff/bazel/BazelQueryService.kt)).
- **Full Skyframe graph load + package analysis** for `deps(//...)`.

On a large monorepo this is minutes per cold start. A Firecracker microVM snapshot lets us capture
that warm state once and restore it in ~sub-second, so the PR-time path re-analyzes only the changed
packages.

**Goal:** instant starts of bazel-diff by restoring a microVM whose Bazel server already has the
build graph loaded and external repos fetched.

---

## 2. Key architectural split

The Firecracker record/restore itself is a **host-level concern** — it talks to the Firecracker REST
API over a unix socket (optionally via `jailer`). It is *not* something the Kotlin CLI does. The work
therefore splits into two pieces:

| Piece | Where | Responsibility |
| --- | --- | --- |
| **CLI hooks** | Kotlin (`cli/`) | Make snapshots deterministic and *safe*: warm-then-signal, emit a cache key, bake base hashes. |
| **Orchestration tool** | Go (`tools/firecracker/`) | Boot/warm/snapshot and restore/checkout/run the microVM via the Firecracker API. |

Consume needs **no new bazel-diff command** — it is the existing `generate-hashes` +
`get-impacted-targets` run against base hashes baked into the snapshot.

---

## 3. Lifecycle

### Record (per base SHA — on merge to master, or nightly)

```
host: build read-only rootfs  ──►  boot Firecracker microVM (TAP net for fetch)
      (bazel + JDK + git + bazel-diff binary + workspace @ baseSHA)
 VM:  bazel-diff warmup  ──►  bazel query deps(//...) loads Skyframe + fetches externals
                          ──►  writes /snap/base_hashes.json + /snap/fingerprint.json
                          ──►  exits 0  = "safe to snapshot"
host: pause VM ──► snapshot {mem_file, vmstate} ──► freeze rootfs as backing image
      store keyed by  fingerprint + baseSHA
```

### Consume (per PR / target SHA — the hot path)

```
host: fingerprint(targetEnv) == snapshot.fingerprint?  ── no ──► fall back to cold run
                       │ yes
      restore microVM (COW overlay on disk, UFFD lazy memory load)   ~sub-second
 VM:  git checkout <target>  ──► warm server does INCREMENTAL re-analysis of changed pkgs
      bazel-diff generate-hashes        (fast — server already warm)
      bazel-diff get-impacted-targets  -sh /snap/base_hashes.json -fh <new> -o <out>
host: extract impacted targets ──► discard overlay
```

---

## 4. New CLI surface

Both new subcommands slot into the existing picocli `subcommands` list in
[`BazelDiff.kt`](../cli/src/main/kotlin/com/bazel_diff/cli/BazelDiff.kt) alongside
`GenerateHashesCommand` and `GetImpactedTargetsCommand`.

### 4.1 `bazel-diff warmup`

The record-side entrypoint. Effectively `generate-hashes` for the base revision, plus:

- Writes base hashes to a known path (`--base-hashes`, default `/snap/base_hashes.json`).
- Writes the fingerprint file (see §5).
- Exits `0` **only** once the query has completed and the server is warm + quiesced. The host
  watches for this clean exit as the "safe to snapshot" signal.

Implementation reuses `GenerateHashesCommand`'s plumbing; warmup is essentially generate-hashes with
metadata side-effects and a clear success contract.

### 4.2 `bazel-diff fingerprint`

Computes the snapshot **cache key** and writes it as JSON. Used both at record time (to tag the
snapshot) and at consume time (to validate a candidate snapshot before trusting it). See §5.

### 4.3 Consume

No new command. The orchestrator runs the existing `generate-hashes` for the target revision, then
`get-impacted-targets -sh /snap/base_hashes.json -fh <new>`.

---

## 5. Correctness — the cache key and the fail-safe

bazel-diff's core promise is *"an incorrect affected set is worse than none."* A restored snapshot
must produce **the same answer as a cold run**. Two layers of defense:

### 5.1 bazel-diff already re-hashes file content itself

`SourceFileHasher` reads and hashes source file contents independently of the Bazel server. So
*content* correctness does not depend on the warm server's incrementality — only the **graph
structure / rule attributes** returned by `bazel query` do, and Bazel's incremental analysis is the
trusted core there.

### 5.2 The fingerprint (cache key)

A snapshot is only safe to consume when the consuming environment matches the recording environment
on everything that could change the graph. The fingerprint is a hash over:

- **Bazel version** (already detected in `BazelQueryService.determineBazelVersion`).
- **`MODULE.bazel.lock`** (bzlmod resolution state).
- **`.bazelrc`** (and any imported rc files).
- **bazel-diff version** (`VersionProvider`).
- **The relevant flag set** — `--useCquery`, `cqueryCommandOptions`, `bazelCommandOptions`,
  `startupOptions`, `--includeTargetType`, `--targetType`, fine-grained external-repo config, etc.
  (anything that changes what `generate-hashes` queries or how it hashes).

**Fail-safe rule:** any fingerprint mismatch → do **not** use the snapshot; fall back to a cold run.
A stale snapshot is never silently trusted.

### 5.3 CI canary

Recommended: a periodic CI job that runs the *snapshot-consumed* result against a *cold* result for
the same revision pair and asserts set equality. This builds and maintains empirical trust and catches
any Bazel incremental-analysis edge case (env vars, repository-rule re-trigger conditions, untracked
files) before it reaches users.

---

## 6. Firecracker specifics (self-hosted)

Controlling the host makes several normally-hard issues tractable:

- **CPU model pinning.** Snapshots only restore on a matching microarchitecture. Pin the CI instance
  type or set a Firecracker CPU template. (This is exactly the constraint that the cloud-portability
  option would have made painful — out of scope here.)
- **Disk.** Read-only backing rootfs + a per-restore copy-on-write overlay so each consumed VM is
  isolated and disposable.
- **Memory.** Use diff snapshots + UFFD on-demand page loading; keep the mem file on local NVMe/tmpfs
  for fast restore.
- **Clock + network.** Resync the guest clock on resume (a known snapshot gotcha) and re-attach the
  TAP device. To make consume fully offline, pre-bake full git history into the rootfs so
  `git checkout <target>` needs no network.
- **Isolation.** Run under `jailer` in CI.

---

## 7. Snapshot store layout

Keyed by `fingerprint + baseSHA`:

```
<store>/<fingerprint>/<baseSHA>/
  mem_file            # guest memory image (diff snapshot)
  vmstate             # Firecracker microVM state
  rootfs.backing      # frozen read-only disk image
  base_hashes.json    # produced by `bazel-diff warmup`
  metadata.json       # fingerprint, baseSHA, bazel version, created-at, bazel-diff version
```

Consume resolves a snapshot by: matching fingerprint, then choosing a `baseSHA` that is an ancestor
of the target SHA (git merge-base), preferring the nearest ancestor to minimize incremental
re-analysis.

---

## 8. Orchestration tool (Go, `tools/firecracker/`)

Go chosen for the official `firecracker-go-sdk`, clean API access, and a static binary for CI. UX
mirrors `bazel-diff-example.sh` as the familiar entrypoint.

```
bazel-diff-snap record  --workspace <path> --base-sha <sha>  --store <dir> [firecracker opts]
bazel-diff-snap consume --workspace <path> --target-sha <sha> --store <dir> --out <impacted.txt>
```

- `record`: build/prepare rootfs → boot VM → run `bazel-diff warmup` + `fingerprint` → pause →
  snapshot → freeze rootfs → write store entry.
- `consume`: compute `fingerprint` for target env → resolve compatible snapshot (else cold fall-back)
  → restore (COW overlay, UFFD) → `git checkout` → `generate-hashes` → `get-impacted-targets` →
  extract → tear down.

---

## 9. Phasing

1. **CLI hooks** — `fingerprint` + `warmup` subcommands, pure Kotlin, fully unit-testable, no VM
   required. Lands value independently (the fingerprint is useful for any snapshot/caching scheme).
2. **Orchestration tool** — `tools/firecracker/` in Go: `record` / `consume`.
3. **Correctness canary + docs** — snapshot-vs-cold equality check in CI; README section.

---

## 10. Open questions

- **Quiescence detection.** How does `warmup` know the server is fully idle (no background Skyframe
  work) before exit? Likely "query returned + process exited 0" is sufficient since the query is
  synchronous, but worth validating.
- **Snapshot freshness policy.** How far back can a base SHA be before incremental re-analysis stops
  being worth it vs. cold? Needs measurement; drives record cadence (every merge vs. nightly).
- **rootfs build pipeline.** Reuse an existing base image + inject workspace, or build per-record?
  Affects record time and store size.
- **Flag-set canonicalization.** Exact list of flags that must enter the fingerprint vs. those that
  are snapshot-neutral — needs an explicit, reviewed allow/deny list to avoid both false mismatches
  (wasted cold runs) and false matches (incorrect results).
