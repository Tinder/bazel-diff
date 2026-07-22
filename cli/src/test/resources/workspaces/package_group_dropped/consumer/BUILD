# Depends on //lib:thing, whose visibility is controlled by the //lib:consumers
# package_group. Emptying that package_group's `packages` list (the only change
# between the two checkouts) would make this target fail visibility analysis in
# a real build -- yet bazel-diff reports zero impacted targets because the
# package_group is dropped from the hashed graph. See //lib:BUILD.
genrule(
    name = "use_thing",
    srcs = ["//lib:thing"],
    outs = ["use_thing.txt"],
    cmd = "cp $< $@",
)
