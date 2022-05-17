workspace(name = "bazel_diff")

load("//:repositories.bzl", "bazel_diff_dependencies")

bazel_diff_dependencies()

load("@rules_proto//proto:repositories.bzl", "rules_proto_dependencies", "rules_proto_toolchains")

rules_proto_dependencies()

rules_proto_toolchains()

load("@rules_jvm_external//:defs.bzl", "maven_install")
load("//:artifacts.bzl", "BAZEL_DIFF_MAVEN_ARTIFACTS")
load("@io_bazel_rules_kotlin//kotlin:repositories.bzl", "kotlin_repositories")

kotlin_repositories()

load("@io_bazel_rules_kotlin//kotlin:core.bzl", "kt_register_toolchains")

kt_register_toolchains()

maven_install(
    name = "bazel_diff_maven",
    artifacts = BAZEL_DIFF_MAVEN_ARTIFACTS,
    fetch_sources = True,
    generate_compat_repositories = True,
    repositories = [
        "https://repo1.maven.org/maven2/",
    ],
)
