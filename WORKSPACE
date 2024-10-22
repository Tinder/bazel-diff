workspace(name = "bazel_diff")

load("//:repositories.bzl", "bazel_diff_dependencies")

bazel_diff_dependencies()

load("@bazel_skylib//:workspace.bzl", "bazel_skylib_workspace")

bazel_skylib_workspace()

load("@bazel_features//:deps.bzl", "bazel_features_deps")

bazel_features_deps()

load("@rules_python//python:repositories.bzl", "py_repositories")

py_repositories()

load("@com_google_protobuf//:protobuf_deps.bzl", "protobuf_deps")

protobuf_deps()

load("@rules_proto//proto:repositories.bzl", "rules_proto_dependencies")

rules_proto_dependencies()

load("@rules_proto//proto:toolchains.bzl", "rules_proto_toolchains")

rules_proto_toolchains()

load("@rules_java//java:repositories.bzl", "rules_java_dependencies", "rules_java_toolchains")

rules_java_dependencies()

rules_java_toolchains()

load("@rules_jvm_external//:repositories.bzl", "rules_jvm_external_deps")

rules_jvm_external_deps()

load("@rules_jvm_external//:setup.bzl", "rules_jvm_external_setup")

rules_jvm_external_setup()

load("@rules_jvm_external//:defs.bzl", "maven_install")
load("@rules_kotlin//kotlin:repositories.bzl", "kotlin_repositories")
load("//:artifacts.bzl", "BAZEL_DIFF_MAVEN_ARTIFACTS")

kotlin_repositories()

load("@rules_kotlin//kotlin:core.bzl", "kt_register_toolchains")

kt_register_toolchains()

maven_install(
    name = "bazel_diff_maven",
    artifacts = BAZEL_DIFF_MAVEN_ARTIFACTS,
    maven_install_json = "//:maven_install.json",
    repositories = [
        "https://repo1.maven.org/maven2/",
    ],
)

load("@bazel_diff_maven//:defs.bzl", "pinned_maven_install")

pinned_maven_install()

load("@aspect_bazel_lib//lib:repositories.bzl", "aspect_bazel_lib_dependencies")

aspect_bazel_lib_dependencies()

load(
    "@aspect_rules_lint//format:repositories.bzl",
    "fetch_ktfmt",
    "rules_lint_dependencies",
)

rules_lint_dependencies()

fetch_ktfmt()
