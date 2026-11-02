def _crate_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.srcs))]

crate = rule(
    implementation = _crate_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "_tool": attr.label(default = "@ext//:tool.sh", allow_single_file = True),
    },
)
