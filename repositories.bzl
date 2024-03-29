"""
Methods to assist in loading dependencies for bazel-diff in WORKSPACE files
"""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
load("//:constants.bzl", "RULES_JVM_EXTERNAL_SHA", "RULES_JVM_EXTERNAL_TAG", "RULES_KOTLIN_SHA", "RULES_KOTLIN_VERSION")

def _maybe(repo_rule, name, **kwargs):
    if not native.existing_rule(name):
        repo_rule(name = name, **kwargs)

def bazel_diff_dependencies(
        rules_jvm_external_tag = RULES_JVM_EXTERNAL_TAG,
        rules_jvm_external_sha = RULES_JVM_EXTERNAL_SHA,
        rules_kotlin_version = RULES_KOTLIN_VERSION,
        rules_kotlin_sha = RULES_KOTLIN_SHA):
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
            "https://github.com/bazelbuild/rules_java/releases/download/7.5.0/rules_java-7.5.0.tar.gz",
        ],
        sha256 = "4da3761f6855ad916568e2bfe86213ba6d2637f56b8360538a7fb6125abf6518",
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
