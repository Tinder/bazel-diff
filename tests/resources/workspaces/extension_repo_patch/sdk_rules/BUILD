load(":defs.bzl", "sdk_toolchain")

package(default_visibility = ["//visibility:public"])

exports_files(["runtime_marker.txt"])

toolchain_type(name = "toolchain_type")

sdk_toolchain(
    name = "sdk_impl",
    version_file = "@default_sdk//:version.txt",
)

toolchain(
    name = "sdk_toolchain",
    toolchain = ":sdk_impl",
    toolchain_type = ":toolchain_type",
)
