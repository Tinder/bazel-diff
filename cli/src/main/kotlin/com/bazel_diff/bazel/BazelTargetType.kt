package com.bazel_diff.bazel

enum class BazelTargetType {
  RULE,
  SOURCE_FILE,
  GENERATED_FILE,
  PACKAGE_GROUP,
  ENVIRONMENT_GROUP,
  UNKNOWN
}
