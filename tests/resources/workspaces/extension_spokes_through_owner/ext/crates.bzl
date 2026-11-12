def _spoke_impl(rctx):
    rctx.file("data.txt", rctx.attr.version)
    rctx.file("BUILD.bazel", """load("@ext//:defs.bzl", "crate")
crate(name = "lib", srcs = ["data.txt"], visibility = ["//visibility:public"])
""")

spoke = repository_rule(implementation = _spoke_impl, attrs = {"version": attr.string()})

def _forward_impl(rctx):
    rctx.file("payload.txt", "unchanged")
    rules = [
        'package(default_visibility = ["//visibility:public"])',
        'genrule(name = "lib", srcs = [%s], outs = ["copy.txt"], cmd = "cp $< $@")' % repr(rctx.attr.src),
    ]
    if rctx.attr.aliases:
        rules.extend([
            'config_setting(name = "alternate", values = {"define":"alternate=1"})',
            'alias(name = "unused", actual = ":payload.txt")',
        ])
    actual = ":lib"
    for i in range(rctx.attr.aliases):
        name = "hop_{}".format(i)
        rules.append('alias(name = "%s", actual = select({":alternate": ":payload.txt", "//conditions:default": "%s"}))' % (name, actual))
        actual = ":" + name
    rctx.file("BUILD.bazel", "\n".join(rules))

forward = repository_rule(implementation = _forward_impl, attrs = {"aliases": attr.int(), "src": attr.string(default = "payload.txt")})

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
    forward(name = "crates__relay", src = "@crates__b//:lib")
    forward(name = "crates__chain", aliases = 6, src = "@crates__b//:lib")
    hub(name = "crates", names = sorted(versions) + ["relay"])
    hub(name = "ext_tools", names = [])

crates = module_extension(
    implementation = _crates_impl,
    tag_classes = {"lock": tag_class(attrs = {"file": attr.label()})},
)
