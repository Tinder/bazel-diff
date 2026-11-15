def _wrap_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.srcs))]

wrap = rule(
    implementation = _wrap_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "_tool": attr.label(default = "@dependent//:tool.sh", allow_single_file = True),
    },
)
