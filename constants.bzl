"""
Various constants used to build bazel-diff
"""

DEFAULT_JVM_EXTERNAL_TAG = "3.3"

RULES_JVM_EXTERNAL_SHA = "d85951a92c0908c80bd8551002d66cb23c3434409c814179c0ff026b53544dab"

BUILD_PROTO_MESSAGE_SHA = "50b79faec3c4154bed274371de5678b221165e38ab59c6167cc94b922d9d9152"

BAZEL_DIFF_MAVEN_ARTIFACTS = [
    "junit:junit:4.12",
    "org.mockito:mockito-core:3.3.3",
    "info.picocli:picocli:jar:4.3.2",
    "com.google.code.gson:gson:jar:2.8.6",
    "com.google.guava:guava:29.0-jre"
]
