""" Artifacts used to build bazel-diff """

load("@rules_jvm_external//:specs.bzl", "maven")

BAZEL_DIFF_MAVEN_ARTIFACTS = [
    maven.artifact("junit", "junit", "4.13.2"),
    maven.artifact("org.mockito.kotlin", "mockito-kotlin", "4.0.0", testonly = True),
    maven.artifact("com.willowtreeapps.assertk", "assertk-jvm", "0.25", testonly = True),
    maven.artifact("io.insert-koin", "koin-test-junit4", "3.1.6", testonly = True),
    "info.picocli:picocli:jar:4.6.3",
    "com.google.code.gson:gson:jar:2.9.0",
    "com.google.guava:guava:31.1-jre",
    "org.apache.commons:commons-pool2:2.11.1",
    "io.insert-koin:koin-core-jvm:3.1.6",
    "org.jetbrains.kotlinx:kotlinx-coroutines-core:1.5.2",
]
