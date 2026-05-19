---
name: improve-coverage
description: Use when you need to raise main-source line coverage in the bazel-diff repo, write tests for an under-covered Kotlin file, fix a CI failure on the 90% coverage gate, or pick the highest-leverage files to test next. Triggers on requests like "the coverage gate is failing, fix it", "write tests for X", "we need more coverage", or "what should I test to get to 90%".
---

# Improving coverage to clear the 90% gate

bazel-diff enforces a 90% main-source line-coverage gate on every PR (see [coverage-status](../coverage-status/SKILL.md) for the inspection side). When the gate fails or you want to raise the bar, the workflow is: pick the worst-covered files, write small focused unit tests, re-run the gate locally before pushing.

## 1. Pick the right files to target

Run `make coverage` (or check the latest CI artifact) and look at the top of the sorted table. Prioritise files by **uncovered-lines-per-test-effort**, not by lowest percentage:

- **Highest-leverage**: small files at 0% (e.g. enum classes, value objects, single-method utilities) — one short unit test usually moves the needle without much code.
- **Highest absolute gain**: large files with moderate coverage (e.g. `BazelQueryService.kt` at 303 lines / 93%) — closing a small percentage gap covers many lines.
- **Lowest leverage**: tiny files at 50–80% where the remaining branches are error paths needing fault injection or refactors.

A worked example: the PR that added the gate ([#356](https://github.com/Tinder/bazel-diff/pull/356)) raised coverage from 88.76% → 90.37% by adding five small files of tests — `BazelTargetTypeTest`, `VersionProviderTest`, `BazelDiffTest`, `StderrLoggerTest`, `BazelTargetTest` — for a total of 26 newly-covered lines.

## 2. Write the test

Existing tests follow a consistent shape:

- Live under `cli/src/test/kotlin/...` mirroring the main source path.
- Use **JUnit 4** (`@Test`, `@Before`, `@After`, `org.junit.Assert.assertThrows`).
- Use **assertk** for assertions (`assertk.assertThat`, with explicit imports for each assertion like `assertk.assertions.isEqualTo`). Forgetting `import assertk.assertions.contains` on a `String.contains` assertion produces a confusing receiver-mismatch error — import every assertion you use.
- Use **mockito-kotlin** when mocking is required (existing examples: `cli/src/test/kotlin/com/bazel_diff/bazel/BazelClientTest.kt`).
- Use **koin** for DI-test setup (existing pattern: `cli/src/test/kotlin/com/bazel_diff/interactor/CalculateImpactedTargetsInteractorIssue335Test.kt`).

Tiny files (enums, value objects, small command classes) usually only need a few targeted tests. Looking at the bytes via `assertThat(BazelTargetType.entries).hasSize(N).containsExactlyInAnyOrder(...)` is enough to cover an enum's declaration lines.

## 3. Register the test target in cli/BUILD

Every test needs its own `kt_jvm_test` entry:

```python
kt_jvm_test(
    name = "BazelTargetTypeTest",
    test_class = "com.bazel_diff.bazel.BazelTargetTypeTest",
    runtime_deps = [":cli-test-lib"],
)
```

The `:cli-test-lib` glob picks up the new test source automatically; the explicit `kt_jvm_test` rule is what makes it executable via `bazel test //cli:<name>`.

## 4. Verify locally before pushing

```bash
bazel test //cli:<YourNewTest>        # one-off run of the new test
make coverage                          # full gate
```

The local number may be lower than CI's because `//cli:E2ETest` often fails or is excluded on dev machines (JDK-env sandbox issues). If you've added tests for a file that's also exercised by E2E (e.g. `BazelQueryService.kt`), the CI delta will be smaller than the local delta — count only the lines that weren't already covered by E2E.

## 5. Things that don't work / aren't worth attempting

- **`Main.kt`** — calls `exitProcess(...)` which kills the JVM; can't be tested in-process without a SecurityManager hack or refactor. Stays at 0%; the threshold tolerates it.
- **`throw IllegalArgumentException(...)` branches in resource-loading code** like `VersionProvider.kt` — the production code resolves the classloader from `this::class.java`, with no injection seam to swap it for one missing the resource. Refactor or skip.
- **`else -> BazelTargetType.UNKNOWN` branches** — only reachable when Bazel's `Build.Target.Discriminator` adds a new enum value the production code doesn't recognise. Triggering today would require a hand-forged proto with a reserved discriminator number, which the protobuf builder rejects.

## When the gate fails on a flake, not on a coverage drop

If the CI threshold step fails with `error: LCOV report not found at 'bazel-out/_coverage/_coverage_report.dat'`, that's an infrastructure issue, not a coverage regression. The fix that landed in PR #356 was to propagate `USE_BAZEL_VERSION` to the threshold step so `bazelisk run` doesn't start a different bazel server. If you see a similar mismatch resurface, check that env propagation first.
