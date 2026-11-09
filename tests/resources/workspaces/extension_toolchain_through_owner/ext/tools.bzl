def _app_impl(rctx):
    rctx.file("BUILD.bazel", """load("@ext//:defs.bzl", "app_lib")
app_lib(name = "lib", visibility = ["//visibility:public"])
alias(name = "manual", actual = "@tc//docs:manual", visibility = ["//visibility:public"])
""")

app = repository_rule(implementation = _app_impl)

def _tc_impl(rctx):
    rctx.file("docs/BUILD.bazel", 'filegroup(name = "manual", srcs = ["guide.txt"], visibility = ["//visibility:public"])')
    rctx.file("docs/guide.txt", "unchanged guide")
    implementation = 'tool(name = "impl", version = {})'.format(repr(rctx.attr.version))
    if rctx.attr.indirect:
        implementation = """filegroup(name = "bundle", srcs = ["@payload//:version.txt"])
tool(name = "impl", payload = ":bundle")"""
    rctx.file("BUILD.bazel", """load("@ext//:defs.bzl", "tool")
{implementation}
toolchain(name = "tc", toolchain = ":impl", toolchain_type = "@ext//:toolchain_type", visibility = ["//visibility:public"])
""".format(implementation = implementation))

tc = repository_rule(implementation = _tc_impl, attrs = {
    "version": attr.string(),
    "indirect": attr.bool(),
})

def _payload_impl(rctx):
    rctx.file("BUILD.bazel", 'exports_files(["version.txt"])')
    rctx.file("version.txt", rctx.attr.value)

payload = repository_rule(implementation = _payload_impl, attrs = {"value": attr.string()})

def _tc_alias_impl(rctx):
    rctx.file("BUILD.bazel", 'alias(name = "tc", actual = "@tc_impl//:tc")')

tc_alias = repository_rule(implementation = _tc_alias_impl)

def _tools_impl(mctx):
    versions = {"toolchain": "1.0"}
    for mod in mctx.modules:
        for lock in mod.tags.lock:
            versions.update(json.decode(mctx.read(lock.file)))
    app(name = "app")
    if "payload" in versions:
        payload(name = "payload", value = versions["payload"])
    tc(name = "tc", version = versions["toolchain"], indirect = "payload" in versions)
    tc(name = "tc_impl", version = "fixed")
    tc_alias(name = "tc_alias")

tools = module_extension(
    implementation = _tools_impl,
    tag_classes = {"lock": tag_class(attrs = {"file": attr.label()})},
)
