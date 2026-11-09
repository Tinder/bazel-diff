def _identity_platform(settings, _attr):
    return {"//command_line_option:platforms": settings["//command_line_option:platforms"]}

_identity = transition(
    implementation = _identity_platform,
    inputs = ["//command_line_option:platforms"],
    outputs = ["//command_line_option:platforms"],
)

def _forward(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.symlink(output = output, target_file = ctx.file.src)
    return [DefaultInfo(files = depset([output]))]

identity_forward = rule(
    implementation = _forward,
    attrs = {
        "src": attr.label(allow_single_file = True),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
    cfg = _identity,
)
