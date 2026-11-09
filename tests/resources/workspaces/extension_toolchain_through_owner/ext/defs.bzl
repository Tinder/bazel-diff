def _tool_impl(ctx):
    return [platform_common.ToolchainInfo(version = ctx.attr.version, payload = ctx.file.payload)]

tool = rule(implementation = _tool_impl, attrs = {
    "deps": attr.label_list(),
    "version": attr.string(),
    "payload": attr.label(allow_single_file = True),
})

def _app_lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    selected = ctx.toolchains["@ext//:toolchain_type"]
    if selected.payload:
        ctx.actions.symlink(output = out, target_file = selected.payload)
    else:
        ctx.actions.write(out, selected.version)
    return [DefaultInfo(files = depset([out]))]

app_lib = rule(
    implementation = _app_lib_impl,
    attrs = {"_wrapper": attr.label(default = "@ext//:wrapper.sh", allow_single_file = True)},
    toolchains = ["@ext//:toolchain_type"],
)
