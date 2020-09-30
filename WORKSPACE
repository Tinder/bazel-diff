workspace(name = "bazel_diff")

load("//:repositories.bzl", "bazel_diff_dependencies")
load("//:constants.bzl", "BAZEL_DIFF_MAVEN_ARTIFACTS")

bazel_diff_dependencies()

load("@rules_proto//proto:repositories.bzl", "rules_proto_dependencies", "rules_proto_toolchains")

rules_proto_dependencies()
rules_proto_toolchains()

load("@rules_jvm_external//:defs.bzl", "maven_install")

maven_install(
    name = "bazel_diff_maven",
    artifacts = BAZEL_DIFF_MAVEN_ARTIFACTS,
    repositories = [
        "http://uk.maven.org/maven2",
        "https://jcenter.bintray.com/",
    ]
)
