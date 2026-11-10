# The allowlist in //lib:consumers reaches this target through the enclosing
# //lib:allowed group and the visibility of //lib:thing.
genrule(
    name = "use_thing",
    srcs = ["//lib:thing"],
    outs = ["use_thing.txt"],
    cmd = "cp $< $@",
)
