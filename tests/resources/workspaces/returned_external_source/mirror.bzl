def _mirror(ctx):
    ctx.file("BUILD.bazel", 'exports_files(["mirror.txt"])')
    ctx.symlink(ctx.path(Label("//:value.txt")), "mirror.txt")

mirror = repository_rule(implementation = _mirror)
