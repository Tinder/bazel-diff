"""
Methods to assist in loading dependencies for bazel-diff in WORKSPACE files
"""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
load("//:constants.bzl", "RULES_JAVA_INTEGRITY", "RULES_JAVA_VERSION", "RULES_JVM_EXTERNAL_SHA", "RULES_JVM_EXTERNAL_TAG", "RULES_KOTLIN_SHA", "RULES_KOTLIN_VERSION")

def _maybe(repo_rule, name, **kwargs):
    if not native.existing_rule(name):
        repo_rule(name = name, **kwargs)

def bazel_diff_dependencies(
        rules_java_version = RULES_JAVA_VERSION,
        rules_java_integrity = RULES_JAVA_INTEGRITY,
        rules_jvm_external_tag = RULES_JVM_EXTERNAL_TAG,
        rules_jvm_external_sha = RULES_JVM_EXTERNAL_SHA,
        rules_kotlin_version = RULES_KOTLIN_VERSION,
        rules_kotlin_sha = RULES_KOTLIN_SHA):
    """Loads all external repositories required by bazel-diff (Skylib, rules_java, rules_jvm_external, rules_kotlin, etc.).

    Args:
      rules_java_version: Version string for rules_java.
      rules_java_integrity: Integrity hash for rules_java archive.
      rules_jvm_external_tag: Tag/version for rules_jvm_external.
      rules_jvm_external_sha: SHA256 of rules_jvm_external archive.
      rules_kotlin_version: Version string for rules_kotlin.
      rules_kotlin_sha: SHA256 of rules_kotlin archive.
    """
    _maybe(
        http_archive,
        name = "bazel_skylib",
        sha256 = "cd55a062e763b9349921f0f5db8c3933288dc8ba4f76dd9416aac68acee3cb94",
        urls = [
            "https://mirror.bazel.build/github.com/bazelbuild/bazel-skylib/releases/download/1.5.0/bazel-skylib-1.5.0.tar.gz",
            "https://github.com/bazelbuild/bazel-skylib/releases/download/1.5.0/bazel-skylib-1.5.0.tar.gz",
        ],
    )

    _maybe(
        http_archive,
        name = "bazel_features",
        sha256 = "d7787da289a7fb497352211ad200ec9f698822a9e0757a4976fd9f713ff372b3",
        strip_prefix = "bazel_features-1.9.1",
        url = "https://github.com/bazel-contrib/bazel_features/releases/download/v1.9.1/bazel_features-v1.9.1.tar.gz",
    )

    _maybe(
        http_archive,
        name = "rules_proto",
        sha256 = "71fdbed00a0709521ad212058c60d13997b922a5d01dbfd997f0d57d689e7b67",
        strip_prefix = "rules_proto-6.0.0-rc2",
        urls = [
            "https://github.com/bazelbuild/rules_proto/archive/refs/tags/6.0.0-rc2.tar.gz",
        ],
    )

    _maybe(
        http_archive,
        name = "rules_python",
        sha256 = "c68bdc4fbec25de5b5493b8819cfc877c4ea299c0dcb15c244c5a00208cde311",
        strip_prefix = "rules_python-0.31.0",
        url = "https://github.com/bazelbuild/rules_python/releases/download/0.31.0/rules_python-0.31.0.tar.gz",
    )

    _maybe(
        http_archive,
        name = "com_google_protobuf",
        integrity = "sha256-T8X/Gywzn7hs06JfC1MRR4qwgeZa0ljGeJNZzYTUIfg=",
        strip_prefix = "protobuf-26.1",
        urls = [
            "https://github.com/protocolbuffers/protobuf/archive/v26.1.tar.gz",
        ],
    )

    _maybe(
        http_archive,
        name = "rules_java",
        urls = [
            "https://github.com/bazelbuild/rules_java/releases/download/%s/rules_java-%s.tar.gz" % (rules_java_version, rules_java_version),
        ],
        integrity = rules_java_integrity,
    )

    _maybe(
        http_archive,
        name = "rules_jvm_external",
        strip_prefix = "rules_jvm_external-%s" % rules_jvm_external_tag,
        sha256 = rules_jvm_external_sha,
        url = "https://github.com/bazelbuild/rules_jvm_external/releases/download/%s/rules_jvm_external-%s.tar.gz" % (rules_jvm_external_tag, rules_jvm_external_tag),
    )

    _maybe(
        http_archive,
        name = "rules_kotlin",
        sha256 = rules_kotlin_sha,
        url = "https://github.com/bazelbuild/rules_kotlin/releases/download/v{version}/rules_kotlin-v{version}.tar.gz".format(version = rules_kotlin_version),
    )

    _maybe(
        http_archive,
        name = "aspect_bazel_lib",
        sha256 = "5c42b1547cd4fab56fb90f75295aaf6d9e4aed5b51bfcb2457e44b886204a6e2",
        strip_prefix = "bazel-lib-3.2.1",
        url = "https://github.com/aspect-build/bazel-lib/releases/download/v3.2.1/bazel-lib-v3.2.1.tar.gz",
    )

    _maybe(
        http_archive,
        name = "aspect_rules_lint",
        sha256 = "7d5feef9ad85f0ba78cc5757a9478f8fa99c58a8cabc1660d610b291dc242e9b",
        strip_prefix = "rules_lint-1.0.2",
        url = "https://github.com/aspect-build/rules_lint/releases/download/v1.0.2/rules_lint-v1.0.2.tar.gz",
    )

    _maybe(
        http_archive,
        name = "rules_license",
        urls = [
            "https://mirror.bazel.build/github.com/bazelbuild/rules_license/releases/download/1.0.0/rules_license-1.0.0.tar.gz",
            "https://github.com/bazelbuild/rules_license/releases/download/1.0.0/rules_license-1.0.0.tar.gz",
        ],
        sha256 = "26d4021f6898e23b82ef953078389dd49ac2b5618ac564ade4ef87cced147b38",
    )
