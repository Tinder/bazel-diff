def _alias_repo_impl(ctx):
    ctx.symlink(ctx.attr.build_file, "BUILD.bazel")

_alias_repo = repository_rule(
    implementation = _alias_repo_impl,
    attrs = {"build_file": attr.label(mandatory = True)},
)

def _aliases_impl(mctx):
    for name in ["public", "hidden"]:
        _alias_repo(name = name, build_file = Label("//:" + name + ".BUILD"))

aliases = module_extension(implementation = _aliases_impl)
