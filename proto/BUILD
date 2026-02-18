load("@rules_proto//proto:defs.bzl", "proto_library")
load("@rules_java//java:defs.bzl", "java_proto_library")

proto_library(
    name = "build_proto",
    srcs = ["build.proto"],
)

proto_library(
    name = "analysis_v2_proto",
    srcs = ["analysis_v2.proto"],
)

java_proto_library(
    name = "build_java_proto",
    visibility = ["//visibility:public"],
    deps = [":build_proto"],
)

java_proto_library(
    name = "analysis_v2_java_proto",
    visibility = ["//visibility:public"],
    deps = [":analysis_v2_proto"],
)
