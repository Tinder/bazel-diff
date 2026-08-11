# Kotlin-to-Rust test parity

This document tracks the Kotlin test suite against the experimental Rust implementation.
`Consolidated` means one Rust test validates the same bazel-diff-owned contract as several Kotlin
examples. Tests that only validate Kotlin/JVM implementation mechanics, dependency-injection
wiring, or behavior guaranteed by Rust's type system or an external crate are marked
non-applicable rather than copied.

Rust unit tests do not create executables that impersonate Bazel. Query planning, repository
lowering, and module-impact decisions are tested as pure transformations with injected data;
subprocess integration is covered by the real-Bazel E2E suite.

| Kotlin test class | Rust coverage | Status / non-applicable details |
| --- | --- | --- |
| `bazel/BazelClientTest` | Query/cquery, external fallback, filtering, deduplication, and Bzlmod tests in `src/bazel.rs` | Consolidated |
| `bazel/BazelModServiceTest` | Bazel version/module-graph command tests in `src/bazel.rs` | Consolidated |
| `bazel/BazelRuleTest` | Rule input, digest, tag, visibility, and external-label tests in `src/bazel.rs` and `src/hash.rs` | Consolidated |
| `bazel/BazelTargetTest` | `target_names_and_lowering_cover_all_target_types` | Ported |
| `bazel/BazelTargetTypeTest` | Generated protobuf discriminators are exercised by target-lowering tests | Kotlin enum reflection is non-applicable |
| `bazel/ModuleGraphParserTest` | Parsing, cycle, edge, change, and fallback tests in `src/bazel.rs` and `src/module_graph.rs` | Consolidated |
| `bazel/StderrPollutionRegressionTest` | Prefixed JSON parsing and unchanged-graph fast paths in `src/module_graph.rs` | Consolidated |
| `cli/BazelDiffTest` | Compatibility argument normalization and command workflow tests in `src/main.rs` | Consolidated; Clap's own required-subcommand/default behavior is not retested |
| `cli/converter/ByteSizeConverterTest` | `parses_binary_byte_size` | Ported |
| `cli/converter/CommaSeparatedValueConverterTest` | No custom Rust converter exists | Clap delimiter behavior is external-crate behavior |
| `cli/converter/DurationConverterTest` | `parses_compound_duration` | Ported |
| `cli/converter/NormalisingPathConverterTest` | `option_and_path_compatibility_helpers_normalize_values` | Ported |
| `cli/converter/OptionsConverterTest` | `option_and_path_compatibility_helpers_normalize_values` | Ported |
| `cli/FingerprintCommandTest` | Fingerprint command workflow and output tests in `src/main.rs` and `src/fingerprint.rs` | Consolidated |
| `cli/FingerprintGathererTest` | Gather/import/version tests in `src/fingerprint.rs` | Ported |
| `cli/ServeCommandTest` | Serve configuration, cache, readiness, metrics, and routing tests in `src/main.rs` and `src/server.rs` | Consolidated; JVM shutdown-hook/Koin lifecycle mechanics are non-applicable |
| `cli/VersionProviderTest` | Rust version is compile-time `CARGO_PKG_VERSION` | JVM classpath-resource fallback is non-applicable |
| `cli/WarmupCommandTest` | Warmup success/failure and artifact-ordering tests in `src/main.rs` | Ported |
| `e2e/E2ETest` | `tests/e2e/{core,external,regressions}.rs` | Ported; the C++ Bzlmod case is ignored for the same Bazel 7 fixture/Bazel 8+ incompatibility documented by Kotlin |
| `hash/BuildGraphHasherTest` | Integrated graph hashing tests in `src/hash.rs` | Consolidated; Kotlin's phase-timing object is non-applicable |
| `hash/RuleHasherAlwaysAffectedTagsTest` | Always-affected and stable untagged paths in `src/hash.rs` | Consolidated |
| `hash/RuleHasherTest` | Cycle, memoization, source, visibility, cquery, and dependency-tracking tests in `src/hash.rs` | Consolidated |
| `hash/SourceFileHasherTest` | Content, executable-bit, external, missing-file, and label-form tests in `src/hash.rs` | Consolidated; Koin-constructor test is non-applicable |
| `hash/TargetHashTest` | Parsing and serialization tests in `src/model.rs` | Ported |
| `interactor/CalculateImpactedTargetsInteractorIssue335Test` | Extension/sub-string repository regression test in `src/module_graph.rs` | Ported |
| `interactor/CalculateImpactedTargetsInteractorModuleQueryTest` | Query success, removed-module, mixed-workspace, and Bzlmod-only fallback tests in `src/module_graph.rs` | Consolidated |
| `interactor/CalculateImpactedTargetsInteractorTest` | Distance/filter/sort tests in `src/model.rs` and module-change tests in `src/module_graph.rs` | Consolidated |
| `interactor/DeserialiseHashesInteractorTest` | Legacy/metadata read and malformed-input tests in `src/model.rs` | Consolidated |
| `interactor/FingerprintInteractorTest` | Deterministic/component sensitivity tests in `src/fingerprint.rs` | Consolidated |
| `io/ContentHashProviderTest` | `hash_options_load_content_hashes_and_validate_shape` | Ported |
| `log/StderrLoggerTest` | Rust has no logger abstraction; observable warning/error fallback paths are tested | Kotlin logger implementation is non-applicable |
| `process/ProcessStdinHangRegressionTest` | All Rust subprocess call sites explicitly use `Stdio::null()` | `std::process` EOF behavior is not retested |
| `server/BazelDiffServerTest` | Loopback routing, profile, distance, readiness, method, and timeout tests in `src/server.rs` | Consolidated |
| `server/CachePrunerTest` | Disabled and age/count/size pruning tests in `src/server.rs` | Consolidated; JVM scheduler interruption mechanics are non-applicable |
| `server/GitClientTest` | Real Git resolve, checkout, stale-lock recovery, and multi-remote targeted-fetch tests in `src/server.rs` | Consolidated |
| `server/HashServiceTest` | Cache-key, local/remote hit, backfill, metadata, and profile tests in `src/server.rs` | Consolidated |
| `server/ImpactedTargetsServiceTest` | Profiled query, distance, refetch, and route tests in `src/server.rs` | Consolidated |
| `server/LocalDiskHashCacheStorageTest` | Local hit/touch/stats and age/count/size pruning tests in `src/server.rs` | Consolidated |
| `server/MetricsServiceTest` | `metrics_and_human_sizes_report_cache_state` | Consolidated; JVM heap/clock injection is non-applicable |
| `server/QueryProfilerTest` | Profiled query response tests in `src/server.rs` | Consolidated; JVM GC-bean accounting is non-applicable |
| `server/S3HashCacheStorageTest` | Real loopback S3 success/miss/error and normalized-key tests in `src/server.rs` | Consolidated; `rust-s3` request construction itself is not retested |
| `server/TieredHashCacheStorageTest` | Remote backfill/local reuse and local pruning behavior in `src/server.rs` | Consolidated |

## Enforcement

- `//:rust_tests` runs both `//src:rust_tests` and `//src:cli_tests`.
- `//src:rust_tests` enforces at least 90% Rust production line coverage through
  `//tools/coverage:lcov_merger`.
- Rust E2E parity lives under `tests/e2e.rs` and runs in the dedicated CI E2E job.
