# Depends on //lib:thing, whose visibility is controlled by the //lib:consumers
# package_group. Emptying that package_group's `packages` list (the only change
# between the two checkouts) makes this target fail visibility analysis in a
# real build -- and bazel-diff now reports it as impacted, via the
# //lib:consumers -> //lib:thing visibility edge and the ordinary dep edge from
# here to //lib:thing. See //lib:BUILD.
genrule(
    name = "use_thing",
    srcs = ["//lib:thing"],
    outs = ["use_thing.txt"],
    cmd = "cp $< $@",
)
