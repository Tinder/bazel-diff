def _tool(ctx):
    return [platform_common.ToolchainInfo(value = ctx.file.value)]

tool = rule(implementation = _tool, attrs = {"value": attr.label(allow_single_file = True)})

def _use(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.expand_template(template = ctx.toolchains["//:type"].value, output = out, substitutions = {})
    return [DefaultInfo(files = depset([out]))]

use = rule(implementation = _use, toolchains = ["//:type"])
