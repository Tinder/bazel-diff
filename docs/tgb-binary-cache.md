# TGB-style binary cache format for `serve`: assessment and design note

Status: **deferred, design recorded**. Revisit when `serve` cache entries or per-request compare
time become a problem at monorepo scale (see "When to build it").

Uber's Tango stores its target graph in TGB (Tango Graph Binary), a columnar format whose
headline result is 10x smaller blobs and 7–14x faster compares on a 2.8M-node graph. The speedup
comes from one idea: nodes are sorted, hashes live in one raw fixed-stride column, and an unchanged
run of nodes is proven equal with a single `bytes.Equal` plus bisection on mismatch. This note
measures what that idea would buy bazel-diff, which already stores far less per node than Tango,
and records a design that fits bazel-diff's hash files so it can be built when it pays off.

Reference: `docs/tgb/tgb.md` and `internal/tgbdiff/compare.go` in uber/tango at
`41480a27e50d3a86dc25cd4d773bbe1b60474cdb` (the commit pinned by `tools/perf/tango_compare.py`).

## What bazel-diff stores and how it compares today

A hash file (and a `serve` cache entry, which is the same JSON) is one object mapping each label to
`"<kind>#<sha256 hex>~<sha256 hex>"`, optionally wrapped as `{"hashes": ..., "metadata": ...}` when
it carries `moduleGraphJson`, `dependencyFingerprint` or `depEdges` (`src/model.rs`).

`serve` has no in-memory cache of parsed entries. Every request re-reads both entries from disk,
parses each into a `serde_json::Value`, re-inserts every label into a `BTreeMap<String, TargetHash>`
(three `String`s per node), and `impacted_targets` then allocates two more strings per node via
`TargetHash::identity` to compare them (`src/server.rs` `get_hashes_locked`, `src/model.rs`
`impacted_targets`).

## Measurements

Synthetic hash files with monorepo-shaped labels (about 90 bytes each, 5 source files, 2 rules and
1 generated file per package), 30 changed nodes, release build, this container.

Size per entry:

| Entries | JSON | JSON gzip -6 | Columnar estimate, two 32-byte hashes | Columnar, 16-byte hashes |
|---:|---:|---:|---:|---:|
| 100k | 21.2 MB (212 B/node) | 8.4 MB | 6.6 MB (66 B/node) | 3.4 MB |
| 1M | 212 MB | 84 MB | 66 MB | 34 MB |
| 2.8M (extrapolated) | 595 MB | 235 MB | 185 MB | 95 MB |

Tango's 2.8M-node TGB was 52 MB with 8-byte hashes and about 86 MB at its production 20-byte
stride. bazel-diff would carry two SHA-256 digests per node, so 97% of a bazel-diff columnar entry
is the two hash columns and the size win over JSON is 3.2x, not 10x. Against a gzipped JSON entry
the win is only 1.27x. Size alone does not justify a new format.

`depEdges` (with `--trackDeps`) is where JSON is wasteful: 77 bytes per edge, about 190 B per node
on top of the hashes. gzip already takes that to 4 B per edge; a delta-coded CSR column would be
under 1 B per edge.

Time per compare, 1M entries (one `get-impacted-targets`, equivalent to one `serve` request on a
local cache hit):

| Phase | Today | Notes |
|---|---:|---|
| Read both files | 0.25 s | 424 MB |
| Parse both files | 6.3 s | `serde_json::Value` then re-insert into `BTreeMap` |
| Compare, `impacted_targets` | 0.75 s | two `identity()` string allocations per node |
| Compare, same loop without allocations | 0.30 s | `&str` field comparison |
| Compare, bisecting byte compare over one aligned run | 0.022 s | what the columnar format enables |
| Peak RSS | 1.46 GB | |

The compare-only speedup (0.75 s to 0.022 s, 34x; 14x against the allocation-free loop) matches
Tango's claim. The larger prize is the parse: with a format that can be compared without decoding,
the 6.3 s and 1.5 GB per request disappear. At 2.8M nodes today's path is roughly 18 s and 4 GB per
request pair, which is the point at which this stops being academic.

## When to build it

Any of these on a production `serve` deployment:

* Local-disk entries above about 100 MB, or S3 or Nexus backfills of an entry taking longer than
  the compare they unblock.
* `profile=true` reporting `cacheReadMillis` in the seconds, or request latency on a warm cache
  dominated by parsing rather than `bazel query`.
* Memory limits forcing fewer concurrent requests than CPU allows.

Below those thresholds, the cheaper steps in the next section give most of the benefit.

## Cheaper steps first (no format change)

All four landed together with this note. Measured on the same 1M-entry pair, one
`get-impacted-targets` (two parses plus the compare) went from 4.8 s and 1.46 GB peak to 2.5 s and
0.96 GB; the compare alone from 0.75 s to 0.30 s. In `serve`, a request whose revisions are already
parsed skips both parses outright. In order of payoff per line of code:

1. **Keep parsed entries in memory in `serve`.** Nearly every request shares a `from` revision
   (the main branch). An LRU of `Arc<HashFileData>` keyed by cache key, bounded by
   `--cacheMemoryMax` (default 512 MB of serialized size, `0` disables), removes one of the two
   parses from almost every request and both from repeated queries. A memory hit still refreshes
   the on-disk entry's modification time, so the disk pruner does not expire a hot entry.
2. **Parse directly.** `HashFileData` deserializes with a `serde` visitor straight into the final
   map, no intermediate `Value`, and hashes are a `Digest` that holds a SHA-256 as 32 raw bytes
   (anything that is not 64 lowercase hex digits stays text and round-trips unchanged). The hex
   decode is a single table-driven pass; it was 42% of parse instructions as two passes.
3. **Compare without allocating.** `TargetHash::same_as` compares kind and both digests by value
   in place of the two formatted `identity()` strings per node, and `filter_and_sort_labels`
   ranks each label once instead of cloning it inside the sort.
4. **Compress the shared tier.** Entries are published to S3 or Nexus as fast-level gzip under
   `<key>.json.gz`; a read tries that name first and falls back to the plain `<key>.json` an older
   release wrote, so a mixed fleet keeps sharing during an upgrade. The local-disk tier stays plain
   so `cache_entries` sizing and `/metrics` are unchanged.

Steps 1 through 3 are independent of the binary format and remain useful after it lands, because
the binary reader still needs a label-level view for `explain`, distances and module changes.

## Design: `BDH1`, a columnar hash-file container

Modelled on TGB v3 but reduced to what bazel-diff stores. Not a graph format: it has no attributes,
tags or rule types beyond the three-valued `kind`.

### Ordering

Nodes are sorted by label **string** (the order `BTreeMap<String, _>` already produces and the
order `generate-hashes` writes). Tango sorts by the `(package, name)` tuple to keep its package
column monotonic; bazel-diff has no package column, so string order is simpler and avoids the
mispairing bug Tango documents in §6.1. Both files in a compare must use the same order, which the
reader checks via the header.

### Container

```
Header (64 B) | column blobs | column directory | footer (16 B)
```

Header: magic `BDH1`, format version, flags, `nodeCount`, `edgeCount`, `blockSize`, `hashBytes`
(32, or 16 if truncation is ever accepted), CRC32C. Footer: directory offset, directory CRC32C,
magic. Directory: per column `id, codec (0 raw, 1 zstd), offset, compressedSize, rawSize`.
Unknown column ids are skipped so columns can be added without a version bump. `crc32fast` is
already in `Cargo.lock`; `zstd` and `xxhash-rust` would be new crates and must be added to the
`bd_crates` universe in `MODULE.bazel` as well as `Cargo.toml`.

### Columns

| Id | Column | Codec | Contents |
|---:|---|---|---|
| 1 | `LABEL_DICT` | zstd | Front-coded sorted labels, restart point every 1024 entries |
| 2 | `KIND` | zstd | Per node: 0 none, 1 SourceFile, 2 GeneratedFile, 3 Rule, 4+ dict id into `KIND_DICT` |
| 3 | `KIND_DICT` | zstd | Rare kinds only |
| 4 | `HASH` | raw | Per node: `hashBytes` bytes of the transitive hash |
| 5 | `DIRECT_HASH` | raw | Per node: `hashBytes` bytes of the direct hash |
| 6 | `BLOCK_DIGEST` | raw | Per block: xxhash64 over the block's NUL-separated label bytes |
| 7 | `BLOCK_START` | zstd | Per block: uvarint delta of first node index |
| 8 | `DEG` | zstd | Per node: uvarint out-degree (only with `--trackDeps`) |
| 9 | `DEPS` | zstd | Delta-coded dep node indices: first dep zigzag-relative to the node, rest as gaps |
| 10 | `DEP_EXTERNAL` | zstd | Deps whose label is not a node, as dict ids (bazel-diff dep edges may name labels outside the hash map) |
| 11 | `META` | zstd | Length-prefixed `moduleGraphJson`, `dependencyFingerprint` |

`HASH` and `DIRECT_HASH` are two columns, not one interleaved 64-byte stride, so the common
"did anything change" question touches only `HASH`; `DIRECT_HASH` is read for the changed nodes
when classifying Direct versus Indirect impact.

Block boundaries are content-defined exactly as in TGB (cut after node `i` when
`xxhash64(label_i) & (blockSize-1) == 0`, forced cut at `8 * blockSize`), so an inserted label
disturbs one block. Block size 512.

### Compare

1. **Align.** Walk both `BLOCK_DIGEST` arrays; equal digests align a block without decoding
   labels. Decode and merge-join labels only in unmatched blocks. Output: aligned runs plus inserted
   and deleted node indices. Deleted nodes are never impacted (as today); inserted ones are Direct.
2. **Hash compare.** For each aligned run one slice equality over `HASH`; bisect on mismatch
   (`O(k log(n/k))` for `k` changes). Nodes whose `HASH` differs are impacted; compare their
   `DIRECT_HASH` bytes to classify Direct versus Indirect. Also compare `KIND` for the run, since
   today a kind change with equal hashes counts as impacted (`TargetHash::identity` includes kind).
3. **Decode labels once per changed node** through the dictionary restart table, then apply the
   existing `kind_rank` ordering, `targetType` filter and `//external:` exclusion.
4. **Distances and `explain`** need deps for changed nodes only: build the reverse CSR from `DEPS`
   (three counting-sort passes as in TGB §6.4) and run the existing BFS on indices, or fall back to
   materializing `HashFileData` for the first implementation.

Phase 2 is the measured 0.022 s on 1M nodes; phase 1 is a few block decodes for a typical PR.

### Integration points

* `HashFileData::read` and `from_slice` sniff the magic and dispatch, so `get-impacted-targets`,
  `explain` and the server accept either format without flags. The binary reader exposes the same
  `hashes` view lazily for callers that still need a map (module changes, `explain`), and a direct
  compare entry point that `impacted_targets` uses when both sides are binary.
* `serve` writes `<key>.bdh` and falls back to `<key>.json` on read so a rolling upgrade never
  misses. The format is folded into the cache fingerprint token the same way the blob-id seeding
  token was in #520, so old and new servers never serve each other's entries.
* `generate-hashes` gains `--format json|bdh` (default json, the CLI contract stays byte-identical).
* `/metrics` `cache.sizeBytes` continues to work unchanged; entries are still files.
* `tools/perf/tango_compare.py` already times a "diff only" stage, so the win is measurable
  nightly without new harness work.

### Risks

* **Hash truncation.** Tango truncates to 20 bytes because SHA-1 is 20 bytes. Truncating SHA-256 to
  16 bytes would nearly halve the entry but changes the collision argument; keep 32 bytes unless
  size is the actual problem.
* **Two codecs to keep in lockstep.** Every change to what a hash file carries lands in both the
  JSON serializer and the column writer, with round-trip tests between them.
* **Coverage gate.** The repo enforces 90% line coverage; the codec needs property tests
  (encode, decode, compare against `impacted_targets` on random pairs) from the first commit.
* **Dep edges that are not nodes.** bazel-diff's `depEdges` can reference labels absent from
  `hashes`; TGB assumes a closed graph. Column 10 handles this but the reverse CSR must skip them.
