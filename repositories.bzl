"""
Methods to assist in loading dependencies for bazel-diff in WORKSPACE files
"""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
load("//:constants.bzl", "DEFAULT_JVM_EXTERNAL_TAG", "DEFAULT_RULES_KOTLIN_VERSION", "RULES_JVM_EXTERNAL_SHA", "RULES_KOTLIN_SHA")

def _maybe(repo_rule, name, **kwargs):
    if not native.existing_rule(name):
        repo_rule(name = name, **kwargs)

def bazel_diff_dependencies(
        rules_jvm_external_tag = DEFAULT_JVM_EXTERNAL_TAG,
        rules_jvm_external_sha = RULES_JVM_EXTERNAL_SHA,
        default_rules_kotlin_version = DEFAULT_RULES_KOTLIN_VERSION,
        rules_kotlin_sha = RULES_KOTLIN_SHA):
    _maybe(
        http_archive,
        name = "bazel_skylib",
        urls = [
            "https://mirror.bazel.build/github.com/bazelbuild/bazel-skylib/releases/download/1.5.0/bazel-skylib-1.5.0.tar.gz",
            "https://github.com/bazelbuild/bazel-skylib/releases/download/1.5.0/bazel-skylib-1.5.0.tar.gz",
        ],
        sha256 = "cd55a062e763b9349921f0f5db8c3933288dc8ba4f76dd9416aac68acee3cb94",
    )

    _maybe(
        http_archive,
        name = "rules_proto",
        sha256 = "c22cfcb3f22a0ae2e684801ea8dfed070ba5bed25e73f73580564f250475e72d",
        strip_prefix = "rules_proto-4.0.0-3.19.2",
        urls = [
            "https://github.com/bazelbuild/rules_proto/archive/refs/tags/4.0.0-3.19.2.tar.gz",
        ],
    )

    _maybe(
        http_archive,
        name = "rules_jvm_external",
        strip_prefix = "rules_jvm_external-%s" % rules_jvm_external_tag,
        sha256 = rules_jvm_external_sha,
        url = "https://github.com/bazelbuild/rules_jvm_external/releases/download/%s/rules_jvm_external-%s.tar.gz" % (DEFAULT_JVM_EXTERNAL_TAG, DEFAULT_JVM_EXTERNAL_TAG),
    )

    _maybe(
        http_archive,
        name = "io_bazel_rules_kotlin",
        sha256 = rules_kotlin_sha,
        url = "https://github.com/bazelbuild/rules_kotlin/releases/download/v{version}/rules_kotlin-v{version}.tar.gz".format(version = default_rules_kotlin_version),
    )

    _maybe(
        http_archive,
        name = "rules_java",
        urls = [
            "https://github.com/bazelbuild/rules_java/releases/download/7.3.2/rules_java-7.3.2.tar.gz",
        ],
        sha256 = "3121a00588b1581bd7c1f9b550599629e5adcc11ba9c65f482bbd5cfe47fdf30",
    )
