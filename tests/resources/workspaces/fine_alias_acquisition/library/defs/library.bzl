def _library(ctx):
    selected = [file for file in ctx.files.srcs if file.basename == ctx.attr.selected]
    return [DefaultInfo(files = depset(selected))]

library = rule(
    implementation = _library,
    attrs = {
        "selected": attr.string(mandatory = True),
        "srcs": attr.label_list(allow_files = True),
    },
)
