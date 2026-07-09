load("@rules_java//java:defs.bzl", "java_binary")
load("@rules_kotlin//kotlin:jvm.bzl", "kt_jvm_library", "kt_jvm_test")

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
    # SLF4J binding for JGit. Not referenced by bazel-diff code (JGit uses it internally), so it is a
    # pure runtime dep: it supplies org.slf4j.impl.StaticLoggerBinder, silencing the "Defaulting to
    # NOP logger" warning SLF4J prints to stderr when no binding is on the classpath.
    runtime_deps = [
        "@bazel_diff_maven//:org_slf4j_slf4j_nop",
    ],
    deps = [
        "//proto:analysis_v2_java_proto",
        "//proto:build_java_proto",
        "@bazel_diff_maven//:com_google_code_gson_gson",
        "@bazel_diff_maven//:com_google_guava_guava",
        "@bazel_diff_maven//:info_picocli_picocli",
        "@bazel_diff_maven//:io_insert_koin_koin_core_jvm",
        "@bazel_diff_maven//:org_apache_commons_commons_pool2",
        "@bazel_diff_maven//:org_eclipse_jgit_org_eclipse_jgit",
        "@bazel_diff_maven//:org_jetbrains_kotlinx_kotlinx_coroutines_core_jvm",
    ],
)

kt_jvm_test(
    name = "BuildGraphHasherTest",
    jvm_flags = [
        "-Dnet.bytebuddy.experimental=true",
    ],
    test_class = "com.bazel_diff.hash.BuildGraphHasherTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "TargetHashTest",
    test_class = "com.bazel_diff.hash.TargetHashTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "RuleHasherAlwaysAffectedTagsTest",
    test_class = "com.bazel_diff.hash.RuleHasherAlwaysAffectedTagsTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "SourceFileHasherTest",
    data = [
        ":src/test/kotlin/com/bazel_diff/hash/fixture/foo.ts",
    ],
    test_class = "com.bazel_diff.hash.SourceFileHasherTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "CalculateImpactedTargetsInteractorTest",
    test_class = "com.bazel_diff.interactor.CalculateImpactedTargetsInteractorTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "CalculateImpactedTargetsInteractorIssue335Test",
    test_class = "com.bazel_diff.interactor.CalculateImpactedTargetsInteractorIssue335Test",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "CalculateImpactedTargetsInteractorModuleQueryTest",
    test_class = "com.bazel_diff.interactor.CalculateImpactedTargetsInteractorModuleQueryTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "NormalisingPathConverterTest",
    test_class = "com.bazel_diff.cli.converter.NormalisingPathConverterTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "OptionsConverterTest",
    test_class = "com.bazel_diff.cli.converter.OptionsConverterTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "DeserialiseHashesInteractorTest",
    test_class = "com.bazel_diff.interactor.DeserialiseHashesInteractorTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "FingerprintInteractorTest",
    test_class = "com.bazel_diff.interactor.FingerprintInteractorTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "FingerprintGathererTest",
    test_class = "com.bazel_diff.cli.FingerprintGathererTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "FingerprintCommandTest",
    test_class = "com.bazel_diff.cli.FingerprintCommandTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "WarmupCommandTest",
    test_class = "com.bazel_diff.cli.WarmupCommandTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelRuleTest",
    test_class = "com.bazel_diff.bazel.BazelRuleTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelClientTest",
    test_class = "com.bazel_diff.bazel.BazelClientTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelTargetTest",
    test_class = "com.bazel_diff.bazel.BazelTargetTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelTargetTypeTest",
    test_class = "com.bazel_diff.bazel.BazelTargetTypeTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelDiffTest",
    test_class = "com.bazel_diff.cli.BazelDiffTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "VersionProviderTest",
    test_class = "com.bazel_diff.cli.VersionProviderTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "StderrLoggerTest",
    test_class = "com.bazel_diff.log.StderrLoggerTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "Slf4jBindingTest",
    test_class = "com.bazel_diff.log.Slf4jBindingTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelModServiceTest",
    test_class = "com.bazel_diff.bazel.BazelModServiceTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "ModuleGraphParserTest",
    test_class = "com.bazel_diff.bazel.ModuleGraphParserTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "StderrPollutionRegressionTest",
    test_class = "com.bazel_diff.bazel.StderrPollutionRegressionTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "ProcessStdinHangRegressionTest",
    test_class = "com.bazel_diff.process.ProcessStdinHangRegressionTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "E2ETest",
    timeout = "long",
    data = [":workspaces"],
    test_class = "com.bazel_diff.e2e.E2ETest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "ContentHashProviderTest",
    data = [
        ":src/test/kotlin/com/bazel_diff/io/fixture/correct.json",
        ":src/test/kotlin/com/bazel_diff/io/fixture/wrong.json",
    ],
    test_class = "com.bazel_diff.io.ContentHashProviderTest",
    runtime_deps = [
        ":cli-test-lib",
    ],
)

kt_jvm_test(
    name = "ServeCommandTest",
    test_class = "com.bazel_diff.cli.ServeCommandTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "LocalDiskHashCacheStorageTest",
    test_class = "com.bazel_diff.server.LocalDiskHashCacheStorageTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "MetricsServiceTest",
    test_class = "com.bazel_diff.server.MetricsServiceTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "GitClientTest",
    test_class = "com.bazel_diff.server.GitClientTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "JGitClientTest",
    test_class = "com.bazel_diff.server.JGitClientTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "HashServiceTest",
    jvm_flags = [
        "-Dnet.bytebuddy.experimental=true",
    ],
    test_class = "com.bazel_diff.server.HashServiceTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "ImpactedTargetsServiceTest",
    jvm_flags = [
        "-Dnet.bytebuddy.experimental=true",
    ],
    test_class = "com.bazel_diff.server.ImpactedTargetsServiceTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelDiffServerTest",
    test_class = "com.bazel_diff.server.BazelDiffServerTest",
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
        # Compile-time API for Slf4jBindingTest; the runtime binding (slf4j-nop) comes via cli-lib.
        "@bazel_diff_maven//:org_slf4j_slf4j_api",
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
