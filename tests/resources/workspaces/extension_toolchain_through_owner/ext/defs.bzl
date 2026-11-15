def _tool_impl(ctx):
    return [platform_common.ToolchainInfo(version = ctx.attr.version, runtime = ctx.file.runtime)]

tool = rule(
    implementation = _tool_impl,
    attrs = {
        "runtime": attr.label(allow_single_file = True),
        "version": attr.string(),
    },
)

def _app_lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    toolchain = ctx.toolchains["@ext//:toolchain_type"]
    if toolchain.runtime:
        ctx.actions.symlink(output = out, target_file = toolchain.runtime)
    else:
        ctx.actions.write(out, toolchain.version)
    return [DefaultInfo(files = depset([out]))]

app_lib = rule(
    implementation = _app_lib_impl,
    attrs = {"_wrapper": attr.label(default = "@ext//:wrapper.sh", allow_single_file = True)},
    toolchains = ["@ext//:toolchain_type"],
)
