def _macros_impl(ctx):
    ctx.file("BUILD.bazel", 'exports_files(["defs.bzl"])')
    ctx.file("defs.bzl", ctx.read(Label("//:defs.bzl")))

macros = repository_rule(implementation = _macros_impl)

def _bridge_impl(ctx):
    ctx.file("BUILD.bazel", 'load("@macros//:defs.bzl", "forward")\nforward(name = "lib")\n')

bridge = repository_rule(implementation = _bridge_impl)
