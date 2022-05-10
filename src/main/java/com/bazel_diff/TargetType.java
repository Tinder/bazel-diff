package com.bazel_diff;

public enum TargetType {
    RULE,
    SOURCE_FILE,
    GENERATED_FILE,
    PACKAGE_GROUP,
    ENVIRONMENT_GROUP,
    UNKNOWN,
}
