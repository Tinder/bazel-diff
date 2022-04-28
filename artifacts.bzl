""" Artifacts used to build bazel-diff """

load("@rules_jvm_external//:specs.bzl", "maven")

BAZEL_DIFF_MAVEN_ARTIFACTS = [
    maven.artifact("junit", "junit", "4.13", testonly = True),
    maven.artifact("org.mockito", "mockito-core", "3.5.15", testonly = True),
    "info.picocli:picocli:jar:4.3.2",
    "com.google.code.gson:gson:jar:2.8.6",
    "com.google.guava:guava:29.0-jre",
    "org.apache.commons:commons-pool2:2.11.1",
]
