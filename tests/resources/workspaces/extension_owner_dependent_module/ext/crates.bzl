def _spoke_impl(rctx):
    rctx.file("data.txt", rctx.attr.version)
    rctx.file("BUILD.bazel", """load("@ext//:defs.bzl", "crate")
crate(name = "lib", srcs = ["data.txt"], visibility = ["//visibility:public"])
""")

spoke = repository_rule(implementation = _spoke_impl, attrs = {"version": attr.string()})

def _hub_impl(rctx):
    rctx.file("BUILD.bazel", "\n".join([
        'alias(name = "{}", actual = "@{}//:lib", visibility = ["//visibility:public"])'.format(name, spoke)
        for name, spoke in rctx.attr.spokes.items()
    ]))

hub = repository_rule(implementation = _hub_impl, attrs = {"spokes": attr.string_dict()})

def _crates_impl(mctx):
    versions = {}
    for mod in mctx.modules:
        for lock in mod.tags.lock:
            versions.update(json.decode(mctx.read(lock.file)))
    spokes = {}
    for name, version in versions.items():
        # Like npm's `npm__user-agents__2.1.229`: a bump replaces the spoke rather
        # than changing it in place.
        spokes[name] = "crates__{}__{}".format(name, version.replace(".", "_"))
        spoke(name = spokes[name], version = version)
    hub(name = "crates", spokes = spokes)
    hub(name = "ext_tools", spokes = {})

crates = module_extension(
    implementation = _crates_impl,
    tag_classes = {"lock": tag_class(attrs = {"file": attr.label()})},
)
