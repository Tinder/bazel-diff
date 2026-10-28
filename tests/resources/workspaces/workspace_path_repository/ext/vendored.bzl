def _vendored_repository_impl(rctx):
    for name in ["BUILD.bazel", "data.txt"]:
        rctx.symlink(rctx.attr.path + "/" + name, name)

vendored_repository = repository_rule(
    implementation = _vendored_repository_impl,
    attrs = {"path": attr.string(mandatory = True)},
)

def _vendored_impl(mctx):
    path = str(mctx.path(Label("@@//:MODULE.bazel")).dirname) + "/vendored"
    vendored_repository(name = "vendored", path = path)

vendored = module_extension(implementation = _vendored_impl)
