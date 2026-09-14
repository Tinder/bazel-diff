# Cquery Failing Target Test Workspace

This test workspace reproduces the scenario described in [GitHub Issue #301](https://github.com/Tinder/bazel-diff/issues/301).

## The Issue

When using `--useCquery`, bazel-diff executes `bazel cquery` which analyzes all targets (executes their implementation functions). This causes problems with targets that are intentionally designed to fail during analysis, such as:
- `analysis_test` targets from the Bazel `rules_testing` library
- Other validation targets that verify build failures

With regular `bazel query`, these targets don't cause problems because `query` doesn't execute implementation functions.

## Test Workspace Structure

This workspace contains three targets:

1. **`normal_target`**: A regular target that works fine (using `write_file` from bazel_skylib)
2. **`dependent_target`**: Another regular target
3. **`failing_analysis_target`**: A target with a custom rule that's designed to fail during analysis

The `failing_analysis_target` uses `failing_rule.bzl` which implements a rule that always calls `fail()` in its implementation function, simulating the behavior of analysis tests.

## Using `--keep_going`

The `--keep_going` flag is **already supported** in bazel-diff and is **enabled by default** (`keepGoing = true` in GenerateHashesCommand.kt).

When using cquery with `--keep_going`:
- Bazel will analyze all targets it can
- Targets that fail analysis are skipped
- Partial results are returned
- **Exit code is 1** (not 0 or 3)

### Current Behavior

The current implementation in `BazelQueryService.kt` allows exit codes:
- `0` - Success
- `3` - When `--keep_going` is enabled (for query)

However, **cquery with `--keep_going` returns exit code 1** when some targets fail analysis but partial results are available.

### Exit Code Observations

From testing:
- `bazel query --keep_going`: Returns exit code 0
- `bazel cquery --keep_going` (with failing targets): Returns exit code 1 with message "command succeeded, but not all targets were analyzed"

## Solution Implemented

✅ **Custom cquery expressions** (Solution #2): Users can now specify custom cquery expressions that exclude problematic targets using the `--cqueryExpression` flag.

### Usage

```bash
bazel-diff generate-hashes \
  --useCquery \
  --cqueryExpression "deps(//:target1) + deps(//:target2)" \
  output.json
```

### Important Notes

When crafting custom cquery expressions to exclude failing targets:

- ❌ **Don't use**: `deps(//...:all-targets) except //:failing_target`
  - This still analyzes the failing target during pattern expansion

- ✅ **Do use**: `deps(//:target1) + deps(//:target2) + deps(//package:*)`
  - Explicitly specify which targets or packages to include
  - The failing targets are simply not mentioned in the expression

### Examples

**Exclude specific targets:**
```bash
--cqueryExpression "deps(//:normal_target) + deps(//:another_target)"
```

**Include all targets from specific packages:**
```bash
--cqueryExpression "deps(//src/...:*) + deps(//lib/...:*)"
```

**Combine multiple patterns:**
```bash
--cqueryExpression "deps(//...:all) - kind('.*_test', //...)"
```

## Test Case

The test case `testCqueryWithFailingAnalysisTargets` in `E2ETest.kt` verifies:
1. ✅ Regular query works (doesn't analyze targets)
2. ✅ Cquery without `--keep_going` fails with exit code 1
3. ✅ Cquery with `--keep_going` also fails with exit code 1 (current behavior)
4. ✅ **NEW**: Cquery with custom expression succeeds by avoiding the failing target
