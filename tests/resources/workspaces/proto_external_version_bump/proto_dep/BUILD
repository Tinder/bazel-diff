load("@protobuf//bazel:java_proto_library.bzl", "java_proto_library")
load("@rules_proto//proto:defs.bzl", "proto_library")

proto_library(
    name = "greeting_proto",
    srcs = ["greeting.proto"],
    visibility = ["//visibility:public"],
)

java_proto_library(
    name = "greeting_java_proto",
    visibility = ["//visibility:public"],
    deps = [":greeting_proto"],
)
