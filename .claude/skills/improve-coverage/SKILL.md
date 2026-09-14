---
name: improve-coverage
description: Use when you need to raise main-source line coverage in the bazel-diff repo, write tests for an under-covered Rust module, fix a CI failure on the 90% coverage gate, or pick the highest-leverage files to test next. Triggers on requests like "the coverage gate is failing, fix it", "write tests for X", "we need more coverage", or "what should I test to get to 90%".
---

# Improving coverage to clear the 90% gate

bazel-diff enforces a 90% main-source line-coverage gate on every PR (see [coverage-status](../coverage-status/SKILL.md) for the inspection side). When the gate fails or you want to raise the bar, the workflow is: pick the worst-covered files, write small focused unit tests, re-run the gate locally before pushing.

## 1. Pick the right files to target

Run `make coverage` (or check the latest CI artifact) and look at the top of the sorted table. Prioritise files by **uncovered-lines-per-test-effort**, not by lowest percentage:

- **Highest-leverage**: small, pure functions at low coverage (label normalisation, path handling, converters) — one short `#[test]` usually moves the needle without much code.
- **Highest absolute gain**: large modules with moderate coverage (`src/hash.rs`, `src/server.rs`) — closing a small percentage gap covers many lines.
- **Lowest leverage**: error paths that need fault injection (a Bazel subprocess failing mid-stream, an S3 request erroring) — reach for them only once the cheap lines are gone.

## 2. Write the test

Existing tests follow a consistent shape:

- Unit tests live in a `#[cfg(test)] mod tests` at the bottom of the module they cover (`src/*.rs`). They run as `//src:rust_tests`; tests of `main.rs` itself run as `//src:cli_tests`.
- Plain `assert!`/`assert_eq!`; no assertion crate.
- Build proto inputs (`Target`, `Rule`, `Attribute`) by hand with the generated structs rather than parsing text — see the helpers already in `src/hash.rs`'s test module.
- Anything that needs a filesystem uses `tempfile::tempdir()`; nothing in a unit test shells out to Bazel. Behaviour that needs a real Bazel belongs in `tests/e2e/` (and does **not** count toward the coverage number, because the e2e suite runs the binary as a subprocess).

## 3. No BUILD edit needed

`//src:bazel_diff_lib` globs `src/**/*.rs`, so a new `#[test]` is picked up by `//src:rust_tests` automatically. A new e2e case needs `make regen-e2e` instead (see [tools/e2e/README.md](../../../tools/e2e/README.md)).

## 4. Verify locally before pushing

```bash
bazel test //src:rust_tests --test_arg=<test_name_substring>   # one-off run of the new test
make coverage                                                   # full gate
```

`bazel coverage` also enforces per-target minimums (90% on `//src:rust_tests` scoped to `src/`), so a target can fail the coverage run even when the repo-wide number passes; the test log carries the per-file breakdown.

## 5. Things that don't work / aren't worth attempting

- **`fn main()` in `src/main.rs`** — exits the process; the argument parsing it delegates to is what `//src:cli_tests` covers. Stays uncovered; the threshold tolerates it.
- **Bazel/git subprocess failure branches** in `src/bazel.rs` and `src/server.rs` — need a real failing subprocess. Cover them from an e2e case if the behaviour matters; don't chase them for the number.
- **Match arms for proto enum values Bazel does not emit** — only reachable with a hand-forged discriminator. Skip.

## When the gate fails on a flake, not on a coverage drop

If the CI threshold step fails with `error: LCOV report not found at 'bazel-out/_coverage/_coverage_report.dat'`, that's an infrastructure issue, not a coverage regression. The fix that landed in PR #356 was to propagate `USE_BAZEL_VERSION` to the threshold step so `bazelisk run` doesn't start a different bazel server. If you see a similar mismatch resurface, check that env propagation first.
