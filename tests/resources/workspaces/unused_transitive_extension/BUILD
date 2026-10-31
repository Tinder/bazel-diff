# The only root target. It reaches into @direct_dep, a plain module repository,
# and nowhere near transitive_dep's extension.
genrule(
    name = "app",
    srcs = ["@direct_dep//:lib.txt"],
    outs = ["app.txt"],
    cmd = "cp $< $@",
)
