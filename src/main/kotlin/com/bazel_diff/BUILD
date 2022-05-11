load("@rules_java//java:defs.bzl", "java_binary", "java_library", "java_proto_library")
load("@rules_proto//proto:defs.bzl", "proto_library")
load("@io_bazel_rules_kotlin//kotlin:jvm.bzl", "kt_jvm_binary", "kt_jvm_import", "kt_jvm_library")
load("@rules_jvm_external//:defs.bzl", "artifact")

config_setting(
    name = "enable_debug",
    values = {
        "compilation_mode": "dbg",
    },
)

kt_jvm_binary(
    name = "bazel-diff",
    jvm_flags = select({
        ":enable_debug": ["-DDEBUG=true"],
        "//conditions:default": [],
    }),
    main_class = "com.bazel_diff.Main",
    visibility = ["//visibility:public"],
    runtime_deps = [":java-bazel-diff-lib"],
)

kt_jvm_library(
    name = "java-bazel-diff-lib",
    srcs = glob(["**/*.kt"]),
    visibility = ["//test/java/com/bazel_diff:__pkg__"],
    deps = [
        ":build_java_proto",
        "@bazel_diff_maven//:com_google_code_gson_gson",
        "@bazel_diff_maven//:com_google_guava_guava",
        "@bazel_diff_maven//:info_picocli_picocli",
        "@bazel_diff_maven//:io_insert_koin_koin_core_jvm",
        "@bazel_diff_maven//:org_apache_commons_commons_pool2",
        "@bazel_diff_maven//:org_jetbrains_kotlinx_kotlinx_coroutines_core_jvm",
        "@com_google_protobuf//:protobuf_java",
    ],
)

java_proto_library(
    name = "build_java_proto",
    deps = [":build_proto"],
)

proto_library(
    name = "build_proto",
    srcs = [":build_proto_gen"],
)

genrule(
    name = "build_proto_gen",
    srcs = ["@bazel_tools//src/main/protobuf:build.proto"],
    outs = ["build.proto"],
    cmd = "cp $< $@",
)
