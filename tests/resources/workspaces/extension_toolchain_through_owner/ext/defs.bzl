def _tool_impl(ctx):
    return [platform_common.ToolchainInfo(version = ctx.attr.version)]

tool = rule(implementation = _tool_impl, attrs = {"version": attr.string()})

def _app_lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.toolchains["@ext//:toolchain_type"].version)
    return [DefaultInfo(files = depset([out]))]

app_lib = rule(
    implementation = _app_lib_impl,
    attrs = {"_wrapper": attr.label(default = "@ext//:wrapper.sh", allow_single_file = True)},
    toolchains = ["@ext//:toolchain_type"],
)
