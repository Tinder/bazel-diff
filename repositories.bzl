"""
Methods to assist in loading dependencies for bazel-diff in WORKSPACE files
"""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
load("//:constants.bzl", "DEFAULT_JVM_EXTERNAL_TAG", "RULES_JVM_EXTERNAL_SHA", "DEFAULT_KOTLIN_EXTERNAL_VERSION", "RULES_KOTLIN_EXTERNAL_SHA" )

def _maybe(repo_rule, name, **kwargs):
    if not native.existing_rule(name):
        repo_rule(name = name, **kwargs)

def bazel_diff_dependencies(rules_jvm_external_tag=DEFAULT_JVM_EXTERNAL_TAG,
                            rules_jvm_external_sha=RULES_JVM_EXTERNAL_SHA,
                            rules_kotlin_external_version=DEFAULT_KOTLIN_EXTERNAL_VERSION,
                            rules_kotlin_external_sha=RULES_KOTLIN_EXTERNAL_SHA):
    _maybe(
        http_archive,
        name = "bazel_skylib",
        urls = [
            "https://mirror.bazel.build/github.com/bazelbuild/bazel-skylib/releases/download/1.2.1/bazel-skylib-1.2.1.tar.gz",
            "https://github.com/bazelbuild/bazel-skylib/releases/download/1.2.1/bazel-skylib-1.2.1.tar.gz",
        ],
        sha256 = "f7be3474d42aae265405a592bb7da8e171919d74c16f082a5457840f06054728",
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
        url = "https://github.com/bazelbuild/rules_jvm_external/archive/%s.zip" % rules_jvm_external_tag
    )

    _maybe(
        http_archive,
        name = "io_bazel_rules_kotlin",
        sha256 = rules_kotlin_external_sha,
        url = "https://github.com/bazelbuild/rules_kotlin/releases/download/v%s/rules_kotlin_release.tgz" % rules_kotlin_external_version
    )
