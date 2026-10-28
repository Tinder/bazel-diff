def _outside_repository_impl(rctx):
    for name in ["BUILD.bazel", "data.txt"]:
        rctx.symlink(rctx.attr.path + "/" + name, name)

outside_repository = repository_rule(
    implementation = _outside_repository_impl,
    attrs = {"path": attr.string(mandatory = True)},
)

def _outside_impl(mctx):
    path = str(mctx.path(Label("//:MODULE.bazel")).dirname) + "/../data"
    outside_repository(name = "outside", path = path)

outside = module_extension(implementation = _outside_impl)
