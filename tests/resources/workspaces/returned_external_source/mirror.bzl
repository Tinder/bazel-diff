def _mirror(ctx):
    ctx.file("BUILD.bazel", 'exports_files(["mirror.txt", "mirror_dir"])')
    source = ctx.path(Label("//:value.txt"))
    ctx.symlink(source, "mirror.txt")
    ctx.symlink(source.dirname.get_child("input_dir"), "mirror_dir")

mirror = repository_rule(implementation = _mirror)
