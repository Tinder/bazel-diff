def _app_impl(rctx):
    rctx.file("BUILD.bazel", """load("@ext//:defs.bzl", "app_lib")
app_lib(name = "lib", visibility = ["//visibility:public"])
""")

app = repository_rule(implementation = _app_impl)

def _tc_impl(rctx):
    rctx.file("BUILD.bazel", """load("@ext//:defs.bzl", "tool")
tool(name = "impl", version = "{version}")
toolchain(name = "tc", toolchain = ":impl", toolchain_type = "@ext//:toolchain_type")
""".format(version = rctx.attr.version))

tc = repository_rule(implementation = _tc_impl, attrs = {"version": attr.string()})

def _tools_impl(mctx):
    versions = {"toolchain": "1.0"}
    for mod in mctx.modules:
        for lock in mod.tags.lock:
            versions.update(json.decode(mctx.read(lock.file)))
    app(name = "app")
    tc(name = "tc", version = versions["toolchain"])

tools = module_extension(
    implementation = _tools_impl,
    tag_classes = {"lock": tag_class(attrs = {"file": attr.label()})},
)
