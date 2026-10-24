def _spoke_impl(rctx):
    rctx.file("data.txt", rctx.attr.version)
    rctx.file("BUILD.bazel", """load("@ext//:defs.bzl", "crate")
crate(name = "lib", srcs = ["data.txt"], visibility = ["//visibility:public"])
""")

spoke = repository_rule(implementation = _spoke_impl, attrs = {"version": attr.string()})

def _hub_impl(rctx):
    rctx.file("BUILD.bazel", "\n".join([
        'alias(name = "{0}", actual = "@crates__{0}//:lib", visibility = ["//visibility:public"])'.format(name)
        for name in rctx.attr.names
    ]))

hub = repository_rule(implementation = _hub_impl, attrs = {"names": attr.string_list()})

def _crates_impl(mctx):
    versions = {}
    for mod in mctx.modules:
        for lock in mod.tags.lock:
            versions.update(json.decode(mctx.read(lock.file)))
    for name, version in versions.items():
        spoke(name = "crates__" + name, version = version)
    hub(name = "crates", names = sorted(versions))

crates = module_extension(
    implementation = _crates_impl,
    tag_classes = {"lock": tag_class(attrs = {"file": attr.label()})},
)
