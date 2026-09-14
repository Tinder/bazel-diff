load("@rules_java//java:defs.bzl", "java_library")

# Consumes a java_proto_library that lives in the remote `proto_dep` module.
# When proto_dep ships a new version whose .proto content changes, this target
# must be reported as impacted.
java_library(
    name = "consumer",
    srcs = ["Consumer.java"],
    visibility = ["//visibility:public"],
    deps = ["@proto_dep//:greeting_java_proto"],
)
