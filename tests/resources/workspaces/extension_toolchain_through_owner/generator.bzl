def _compiler(ctx):
    return [platform_common.ToolchainInfo(file = ctx.file.src)]

compiler = rule(
    implementation = _compiler,
    attrs = {"src": attr.label(allow_single_file = True)},
)

def _generate(ctx):
    if ctx.attr.fail_analysis:
        fail("the requested generator is unavailable")
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.symlink(output = out, target_file = ctx.toolchains["//:generator_type"].file)
    return [DefaultInfo(files = depset([out]))]

generate = rule(
    implementation = _generate,
    attrs = {"fail_analysis": attr.bool()},
    toolchains = ["//:generator_type"],
)

def _alternate(_settings, _attr):
    return {"//command_line_option:platforms": ["@outside//:platform"]}

_alternate_platform = transition(
    implementation = _alternate,
    inputs = [],
    outputs = ["//command_line_option:platforms"],
)

def _forward(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.symlink(output = out, target_file = ctx.file.src)
    return [DefaultInfo(files = depset([out]))]

forward = rule(
    implementation = _forward,
    attrs = {
        "src": attr.label(allow_single_file = True, cfg = _alternate_platform),
        "_allowlist_function_transition": attr.label(default = "@bazel_tools//tools/allowlists/function_transition_allowlist"),
    },
)
