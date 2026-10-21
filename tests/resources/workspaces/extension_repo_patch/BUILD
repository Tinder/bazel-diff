# Reproducer workspace for https://github.com/Tinder/bazel-diff/issues/522 and
# the hashbench toolchain_patch_edit scenario.
#
# Shape:
#   //:app  --(rule from @sdk_rules, implicit dep @sdk_rules//:runtime_marker.txt)
#           --(toolchain resolution, invisible to `bazel query`)--> @sdk_rules//:sdk_impl
#           --> @@sdk_rules++sdk+default_sdk//:version.txt   (patched by //patches:sdk.patch)
#
# Editing patches/sdk.patch changes what //:app builds, so //:app must be
# impacted; //:control must not.
load("@sdk_rules//:defs.bzl", "sdk_user")

sdk_user(name = "app")

filegroup(
    name = "control",
    srcs = ["control.txt"],
)
