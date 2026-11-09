"""A public alias hub, an imported bridge, and an independent private sibling."""

def _aliases_impl(rctx):
    rctx.file("BUILD.bazel", "\n".join([
        'alias(name = "{0}", actual = "{1}", visibility = ["//visibility:public"])'.format(name, actual)
        for name, actual in rctx.attr.aliases.items()
    ]))

_aliases = repository_rule(
    implementation = _aliases_impl,
    attrs = {"aliases": attr.string_dict()},
)

def _file_impl(rctx):
    rctx.file("value.txt", rctx.attr.value)
    rctx.file("BUILD.bazel", 'exports_files(["value.txt"])\n')

_file = repository_rule(
    implementation = _file_impl,
    attrs = {"value": attr.string()},
)

def _bridges_impl(mctx):
    _aliases(name = "public", aliases = {"app": "@bridge//:app", "noise": "@noise//:value.txt"})
    _aliases(name = "bridge", aliases = {"app": "@@//:app"})
    _file(name = "noise", value = mctx.read(Label("//:noise.txt")))

bridges = module_extension(implementation = _bridges_impl)
