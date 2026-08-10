load("@rules_java//java:defs.bzl", "java_binary")
load("@rules_kotlin//kotlin:jvm.bzl", "kt_jvm_library", "kt_jvm_test")
load("//tools/coverage:defs.bzl", "coverage_minimum_env")

config_setting(
    name = "enable_debug",
    values = {
        "compilation_mode": "dbg",
    },
)

genrule(
    name = "version_file",
    outs = ["src/main/resources/cli/version"],
    cmd = "echo '{version}' > $@".format(version = module_version() if module_version() else "unknown"),
    stamp = 1,
)

java_binary(
    name = "bazel-diff",
    jvm_flags = select({
        ":enable_debug": ["-DDEBUG=true"],
        "//conditions:default": [],
    }),
    main_class = "com.bazel_diff.Main",
    visibility = ["//visibility:public"],
    runtime_deps = [":cli-lib"],
)

kt_jvm_library(
    name = "cli-lib",
    srcs = glob(["src/main/kotlin/**/*.kt"]),
    resources = [":version_file"],
    # sts is runtime-only: the SDK's DefaultCredentialsProvider loads it reflectively to
    # assume a pod IAM role via web identity (IRSA) for the serve S3 cache backend.
    runtime_deps = [
        "@bazel_diff_maven//:software_amazon_awssdk_sts",
    ],
    deps = [
        "//proto:analysis_v2_java_proto",
        "//proto:build_java_proto",
        "@bazel_diff_maven//:com_google_code_gson_gson",
        "@bazel_diff_maven//:com_google_guava_guava",
        "@bazel_diff_maven//:info_picocli_picocli",
        "@bazel_diff_maven//:io_insert_koin_koin_core_jvm",
        "@bazel_diff_maven//:org_apache_commons_commons_pool2",
        "@bazel_diff_maven//:org_jetbrains_kotlinx_kotlinx_coroutines_core_jvm",
        "@bazel_diff_maven//:software_amazon_awssdk_regions",
        "@bazel_diff_maven//:software_amazon_awssdk_s3",
        "@bazel_diff_maven//:software_amazon_awssdk_sdk_core",
        "@bazel_diff_maven//:software_amazon_awssdk_url_connection_client",
    ],
)

# Every kt_jvm_test declares a per-target line-coverage minimum, enforced by
# //tools/coverage:lcov_merger during `bazel coverage` only (plain `bazel test`
# is unaffected). Jacoco instruments the whole of :cli-lib for every test, so
# each check is scoped (coverage_include) to the source file(s) the test is
# responsible for covering. Floors are set from measured coverage minus ~10
# points of headroom (rounded down to 5, capped at 90) to absorb
# platform/Bazel-version variance while still catching real regressions —
# ratchet them upward as coverage improves.
kt_jvm_test(
    name = "BuildGraphHasherTest",
    env = coverage_minimum_env(
        70,
        ["cli/src/main/kotlin/com/bazel_diff/hash/BuildGraphHasher.kt"],
    ),
    jvm_flags = [
        "-Dnet.bytebuddy.experimental=true",
    ],
    test_class = "com.bazel_diff.hash.BuildGraphHasherTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "TargetHashTest",
    env = coverage_minimum_env(
        90,
        ["cli/src/main/kotlin/com/bazel_diff/hash/TargetHash.kt"],
    ),
    test_class = "com.bazel_diff.hash.TargetHashTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "RuleHasherAlwaysAffectedTagsTest",
    env = coverage_minimum_env(
        20,
        ["cli/src/main/kotlin/com/bazel_diff/hash/RuleHasher.kt"],
    ),
    test_class = "com.bazel_diff.hash.RuleHasherAlwaysAffectedTagsTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "SourceFileHasherTest",
    data = [
        ":src/test/kotlin/com/bazel_diff/hash/fixture/foo.ts",
    ],
    env = coverage_minimum_env(
        70,
        ["cli/src/main/kotlin/com/bazel_diff/hash/SourceFileHasher.kt"],
    ),
    test_class = "com.bazel_diff.hash.SourceFileHasherTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "CalculateImpactedTargetsInteractorTest",
    env = coverage_minimum_env(
        75,
        ["cli/src/main/kotlin/com/bazel_diff/interactor/CalculateImpactedTargetsInteractor.kt"],
    ),
    test_class = "com.bazel_diff.interactor.CalculateImpactedTargetsInteractorTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "CalculateImpactedTargetsInteractorIssue335Test",
    env = coverage_minimum_env(
        25,
        ["cli/src/main/kotlin/com/bazel_diff/interactor/CalculateImpactedTargetsInteractor.kt"],
    ),
    test_class = "com.bazel_diff.interactor.CalculateImpactedTargetsInteractorIssue335Test",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "CalculateImpactedTargetsInteractorModuleQueryTest",
    env = coverage_minimum_env(
        50,
        ["cli/src/main/kotlin/com/bazel_diff/interactor/CalculateImpactedTargetsInteractor.kt"],
    ),
    test_class = "com.bazel_diff.interactor.CalculateImpactedTargetsInteractorModuleQueryTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "NormalisingPathConverterTest",
    env = coverage_minimum_env(
        90,
        ["cli/src/main/kotlin/com/bazel_diff/cli/converter/NormalisingPathConverter.kt"],
    ),
    test_class = "com.bazel_diff.cli.converter.NormalisingPathConverterTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "OptionsConverterTest",
    env = coverage_minimum_env(
        90,
        ["cli/src/main/kotlin/com/bazel_diff/cli/converter/OptionsConverter.kt"],
    ),
    test_class = "com.bazel_diff.cli.converter.OptionsConverterTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "DurationConverterTest",
    env = coverage_minimum_env(
        80,
        ["cli/src/main/kotlin/com/bazel_diff/cli/converter/DurationConverter.kt"],
    ),
    test_class = "com.bazel_diff.cli.converter.DurationConverterTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "ByteSizeConverterTest",
    env = coverage_minimum_env(
        90,
        ["cli/src/main/kotlin/com/bazel_diff/cli/converter/ByteSizeConverter.kt"],
    ),
    test_class = "com.bazel_diff.cli.converter.ByteSizeConverterTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "DeserialiseHashesInteractorTest",
    env = coverage_minimum_env(
        75,
        ["cli/src/main/kotlin/com/bazel_diff/interactor/DeserialiseHashesInteractor.kt"],
    ),
    test_class = "com.bazel_diff.interactor.DeserialiseHashesInteractorTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "FingerprintInteractorTest",
    env = coverage_minimum_env(
        90,
        ["cli/src/main/kotlin/com/bazel_diff/interactor/FingerprintInteractor.kt"],
    ),
    test_class = "com.bazel_diff.interactor.FingerprintInteractorTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "FingerprintGathererTest",
    env = coverage_minimum_env(
        80,
        ["cli/src/main/kotlin/com/bazel_diff/cli/FingerprintGatherer.kt"],
    ),
    test_class = "com.bazel_diff.cli.FingerprintGathererTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "FingerprintCommandTest",
    env = coverage_minimum_env(
        75,
        ["cli/src/main/kotlin/com/bazel_diff/cli/FingerprintCommand.kt"],
    ),
    test_class = "com.bazel_diff.cli.FingerprintCommandTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "WarmupCommandTest",
    env = coverage_minimum_env(
        65,
        ["cli/src/main/kotlin/com/bazel_diff/cli/WarmupCommand.kt"],
    ),
    test_class = "com.bazel_diff.cli.WarmupCommandTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelRuleTest",
    env = coverage_minimum_env(
        65,
        ["cli/src/main/kotlin/com/bazel_diff/bazel/BazelRule.kt"],
    ),
    test_class = "com.bazel_diff.bazel.BazelRuleTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelClientTest",
    env = coverage_minimum_env(
        80,
        ["cli/src/main/kotlin/com/bazel_diff/bazel/BazelClient.kt"],
    ),
    test_class = "com.bazel_diff.bazel.BazelClientTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelTargetTest",
    env = coverage_minimum_env(
        85,
        ["cli/src/main/kotlin/com/bazel_diff/bazel/BazelTarget.kt"],
    ),
    test_class = "com.bazel_diff.bazel.BazelTargetTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelTargetTypeTest",
    env = coverage_minimum_env(
        90,
        ["cli/src/main/kotlin/com/bazel_diff/bazel/BazelTargetType.kt"],
    ),
    test_class = "com.bazel_diff.bazel.BazelTargetTypeTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelDiffTest",
    env = coverage_minimum_env(
        90,
        ["cli/src/main/kotlin/com/bazel_diff/cli/BazelDiff.kt"],
    ),
    test_class = "com.bazel_diff.cli.BazelDiffTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "VersionProviderTest",
    env = coverage_minimum_env(
        55,
        ["cli/src/main/kotlin/com/bazel_diff/cli/VersionProvider.kt"],
    ),
    test_class = "com.bazel_diff.cli.VersionProviderTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "StderrLoggerTest",
    env = coverage_minimum_env(
        90,
        ["cli/src/main/kotlin/com/bazel_diff/log/StderrLogger.kt"],
    ),
    test_class = "com.bazel_diff.log.StderrLoggerTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelModServiceTest",
    env = coverage_minimum_env(
        20,
        ["cli/src/main/kotlin/com/bazel_diff/bazel/BazelModService.kt"],
    ),
    test_class = "com.bazel_diff.bazel.BazelModServiceTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "ModuleGraphParserTest",
    env = coverage_minimum_env(
        65,
        ["cli/src/main/kotlin/com/bazel_diff/bazel/ModuleGraphParser.kt"],
    ),
    test_class = "com.bazel_diff.bazel.ModuleGraphParserTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "StderrPollutionRegressionTest",
    env = coverage_minimum_env(
        15,
        ["cli/src/main/kotlin/com/bazel_diff/bazel/ModuleGraphParser.kt"],
    ),
    test_class = "com.bazel_diff.bazel.StderrPollutionRegressionTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "ProcessStdinHangRegressionTest",
    env = coverage_minimum_env(
        60,
        ["cli/src/main/kotlin/com/bazel_diff/process/"],
    ),
    test_class = "com.bazel_diff.process.ProcessStdinHangRegressionTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "E2ETest",
    # "eternal": the suite runs ~1150s on CI's macos-latest x bazel 8.x cell,
    # right at the "long" ceiling, so slow-runner jitter was timing it out.
    timeout = "eternal",
    data = [":workspaces"],
    # Unlike the unit tests above (floors set from measurement), this is a
    # conservative smoke floor over the whole main tree: the E2E suite drives
    # the real binary through the full hashing/impacted-targets pipeline, so
    # well under half of all main-source lines executing means the suite is
    # broken. Measure and ratchet once a per-target baseline exists in CI.
    env = coverage_minimum_env(
        30,
        ["cli/src/main/kotlin/com/bazel_diff/"],
    ),
    test_class = "com.bazel_diff.e2e.E2ETest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "ContentHashProviderTest",
    data = [
        ":src/test/kotlin/com/bazel_diff/io/fixture/correct.json",
        ":src/test/kotlin/com/bazel_diff/io/fixture/wrong.json",
    ],
    env = coverage_minimum_env(
        90,
        ["cli/src/main/kotlin/com/bazel_diff/io/ContentHashProvider.kt"],
    ),
    test_class = "com.bazel_diff.io.ContentHashProviderTest",
    runtime_deps = [
        ":cli-test-lib",
    ],
)

kt_jvm_test(
    name = "ServeCommandTest",
    env = coverage_minimum_env(
        60,
        ["cli/src/main/kotlin/com/bazel_diff/cli/ServeCommand.kt"],
    ),
    test_class = "com.bazel_diff.cli.ServeCommandTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "LocalDiskHashCacheStorageTest",
    env = coverage_minimum_env(
        75,
        ["cli/src/main/kotlin/com/bazel_diff/server/HashCacheStorage.kt"],
    ),
    test_class = "com.bazel_diff.server.LocalDiskHashCacheStorageTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "S3HashCacheStorageTest",
    env = coverage_minimum_env(
        80,
        ["cli/src/main/kotlin/com/bazel_diff/server/S3HashCacheStorage.kt"],
    ),
    jvm_flags = [
        "-Dnet.bytebuddy.experimental=true",
    ],
    test_class = "com.bazel_diff.server.S3HashCacheStorageTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "TieredHashCacheStorageTest",
    env = coverage_minimum_env(
        90,
        ["cli/src/main/kotlin/com/bazel_diff/server/TieredHashCacheStorage.kt"],
    ),
    test_class = "com.bazel_diff.server.TieredHashCacheStorageTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "CachePrunerTest",
    env = coverage_minimum_env(
        80,
        ["cli/src/main/kotlin/com/bazel_diff/server/CachePruner.kt"],
    ),
    test_class = "com.bazel_diff.server.CachePrunerTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "MetricsServiceTest",
    env = coverage_minimum_env(
        80,
        ["cli/src/main/kotlin/com/bazel_diff/server/MetricsService.kt"],
    ),
    test_class = "com.bazel_diff.server.MetricsServiceTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "GitClientTest",
    env = coverage_minimum_env(
        75,
        ["cli/src/main/kotlin/com/bazel_diff/server/GitClient.kt"],
    ),
    test_class = "com.bazel_diff.server.GitClientTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "HashServiceTest",
    env = coverage_minimum_env(
        75,
        ["cli/src/main/kotlin/com/bazel_diff/server/HashService.kt"],
    ),
    jvm_flags = [
        "-Dnet.bytebuddy.experimental=true",
    ],
    test_class = "com.bazel_diff.server.HashServiceTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "ImpactedTargetsServiceTest",
    env = coverage_minimum_env(
        85,
        ["cli/src/main/kotlin/com/bazel_diff/server/ImpactedTargetsService.kt"],
    ),
    jvm_flags = [
        "-Dnet.bytebuddy.experimental=true",
    ],
    test_class = "com.bazel_diff.server.ImpactedTargetsServiceTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelDiffServerTest",
    env = coverage_minimum_env(
        85,
        ["cli/src/main/kotlin/com/bazel_diff/server/BazelDiffServer.kt"],
    ),
    test_class = "com.bazel_diff.server.BazelDiffServerTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "QueryProfilerTest",
    env = coverage_minimum_env(
        85,
        ["cli/src/main/kotlin/com/bazel_diff/server/QueryProfiler.kt"],
    ),
    test_class = "com.bazel_diff.server.QueryProfilerTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_library(
    name = "cli-test-lib",
    testonly = True,
    srcs = glob(["src/test/kotlin/**/*.kt"]),
    resources = glob(["src/test/resources/**/*"]),
    deps = [
        ":cli-lib",
        "@bazel_diff_maven//:com_willowtreeapps_assertk_assertk_jvm",
        "@bazel_diff_maven//:io_insert_koin_koin_test_junit4",
        "@bazel_diff_maven//:io_insert_koin_koin_test_jvm",
        "@bazel_diff_maven//:junit_junit",
        "@bazel_diff_maven//:org_mockito_kotlin_mockito_kotlin",
        "@bazel_diff_maven//:software_amazon_awssdk_s3",
        "@bazel_diff_maven//:software_amazon_awssdk_sdk_core",
    ],
)

filegroup(
    name = "workspaces",
    srcs = [
        "src/test/resources/workspaces",
    ],
)

java_binary(
    name = "ktfmt",
    main_class = "com.facebook.ktfmt.cli.Main",
    visibility = ["//cli/format:__pkg__"],
    runtime_deps = ["@ktfmt//jar"],
)
