def _app_impl(rctx):
    rctx.file("copy.txt", rctx.read(Label("//:wrapper.sh")), executable = False)
    rctx.file("files.bzl", 'COPIES = Label("//:copies")', executable = False)
    rctx.file("BUILD.bazel", """load("@ext//:defs.bzl", "app_lib")
app_lib(name = "lib", visibility = ["//visibility:public"])
filegroup(name = "copies", srcs = ["copy.txt"], visibility = ["//visibility:public"])
""")

app = repository_rule(implementation = _app_impl)

def _tc_impl(rctx):
    rctx.file("version.txt", rctx.attr.version, executable = False)
    rctx.file("BUILD.bazel", """load("@ext//:defs.bzl", "tool")
exports_files(["version.txt"])
tool(name = "impl", version = "{version}")
toolchain(name = "tc", toolchain = ":impl", toolchain_type = "@ext//:toolchain_type", visibility = ["//visibility:public"])
""".format(version = rctx.attr.version))

tc = repository_rule(implementation = _tc_impl, attrs = {"version": attr.string()})

def _tc_alias_impl(rctx):
    rctx.file("BUILD.bazel", 'alias(name = "tc", actual = "@tc_impl//:tc")')

tc_alias = repository_rule(implementation = _tc_alias_impl)

def _tools_impl(mctx):
    versions = {"toolchain": "1.0"}
    for mod in mctx.modules:
        for lock in mod.tags.lock:
            versions.update(json.decode(mctx.read(lock.file)))
    app(name = "app")
    tc(name = "tc", version = versions["toolchain"])
    tc(name = "tc_impl", version = versions.get("alternate", versions["toolchain"]))
    tc_alias(name = "tc_alias")

tools = module_extension(
    implementation = _tools_impl,
    tag_classes = {"lock": tag_class(attrs = {"file": attr.label()})},
)
