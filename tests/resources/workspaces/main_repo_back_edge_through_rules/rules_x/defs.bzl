def _x_library_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(
        inputs = ctx.files.srcs,
        outputs = [out],
        command = "cat {} > {}".format(" ".join([f.path for f in ctx.files.srcs]), out.path),
    )
    return [DefaultInfo(files = depset([out]))]

x_library = rule(
    implementation = _x_library_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "_checker": attr.label(default = Label("@x_checker//:checker"), cfg = "exec"),
    },
)
