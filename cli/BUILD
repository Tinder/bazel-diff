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
    srcs = [],
    outs = ["version"],
    cmd_bash = """
        version_tag=$$(grep ^STABLE_GIT_TAG bazel-out/stable-status.txt | cut -d' ' -f2); \
        printf '%s' $$version_tag > $@;
    """,
    stamp = 1,
)

config_setting(
    name = "macos",
    constraint_values = [
        "@@platforms//os:macos",
    ],
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
    deps = [
        "@bazel_diff_maven//:com_google_code_gson_gson",
        "@bazel_diff_maven//:com_google_guava_guava",
        "@bazel_diff_maven//:info_picocli_picocli",
        "@bazel_diff_maven//:io_insert_koin_koin_core_jvm",
        "@bazel_diff_maven//:org_apache_commons_commons_pool2",
        "@bazel_diff_maven//:org_jetbrains_kotlinx_kotlinx_coroutines_core_jvm",
        "@bazel_tools//src/main/protobuf:analysis_v2_java_proto",
        "@bazel_tools//src/main/protobuf:build_java_proto",
    ],
)

kt_jvm_test(
    name = "BuildGraphHasherTest",
    jvm_flags = select({
        ":macos": ["-Djava.security.manager=allow"],
        "//conditions:default": [],
    }),
    test_class = "com.bazel_diff.hash.BuildGraphHasherTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "TargetHashTest",
    jvm_flags = select({
        ":macos": ["-Djava.security.manager=allow"],
        "//conditions:default": [],
    }),
    test_class = "com.bazel_diff.hash.TargetHashTest",
    runtime_deps = [":cli-test-lib"],
)


kt_jvm_test(
    name = "SourceFileHasherTest",
    data = [
        ":src/test/kotlin/com/bazel_diff/hash/fixture/foo.ts",
    ],
    jvm_flags = select({
        ":macos": ["-Djava.security.manager=allow"],
        "//conditions:default": [],
    }),
    test_class = "com.bazel_diff.hash.SourceFileHasherTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "CalculateImpactedTargetsInteractorTest",
    jvm_flags = select({
        ":macos": ["-Djava.security.manager=allow"],
        "//conditions:default": [],
    }),
    test_class = "com.bazel_diff.interactor.CalculateImpactedTargetsInteractorTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "NormalisingPathConverterTest",
    jvm_flags = select({
        ":macos": ["-Djava.security.manager=allow"],
        "//conditions:default": [],
    }),
    test_class = "com.bazel_diff.cli.converter.NormalisingPathConverterTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "OptionsConverterTest",
    jvm_flags = select({
        ":macos": ["-Djava.security.manager=allow"],
        "//conditions:default": [],
    }),
    test_class = "com.bazel_diff.cli.converter.OptionsConverterTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "DeserialiseHashesInteractorTest",
    jvm_flags = select({
        ":macos": ["-Djava.security.manager=allow"],
        "//conditions:default": [],
    }),
    test_class = "com.bazel_diff.interactor.DeserialiseHashesInteractorTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "BazelRuleTest",
    jvm_flags = select({
        ":macos": ["-Djava.security.manager=allow"],
        "//conditions:default": [],
    }),
    test_class = "com.bazel_diff.bazel.BazelRuleTest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "E2ETest",
    jvm_flags = select({
        ":macos": ["-Djava.security.manager=allow"],
        "//conditions:default": [],
    }),
    test_class = "com.bazel_diff.e2e.E2ETest",
    runtime_deps = [":cli-test-lib"],
)

kt_jvm_test(
    name = "ContentHashProviderTest",
    data = [
        ":src/test/kotlin/com/bazel_diff/io/fixture/correct.json",
        ":src/test/kotlin/com/bazel_diff/io/fixture/wrong.json",
    ],
    jvm_flags = select({
        ":macos": ["-Djava.security.manager=allow"],
        "//conditions:default": [],
    }),
    test_class = "com.bazel_diff.io.ContentHashProviderTest",
    runtime_deps = [
        ":cli-test-lib",
    ],
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
    ],
)
