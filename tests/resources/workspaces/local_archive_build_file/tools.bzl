def _tool(ctx):
    return [platform_common.ToolchainInfo(value = ctx.file.value)]

tool = rule(implementation = _tool, attrs = {"value": attr.label(allow_single_file = True)})

def _use(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.expand_template(template = ctx.toolchains["//:type"].value, output = out, substitutions = {})
    return [DefaultInfo(files = depset([out]))]

use = rule(implementation = _use, toolchains = ["//:type"])

def _copy_definition(ctx):
    ctx.file("BUILD.bazel", ctx.read(ctx.attr.definition))

copy_definition = repository_rule(
    implementation = _copy_definition,
    attrs = {"definition": attr.label(mandatory = True)},
)

def _forward_dependency(ctx):
    ctx.file("stable.txt", "unchanged")
    ctx.file("BUILD.bazel", """
exports_files(["stable.txt"])
alias(name = "dependency", actual = "{}")
""".format(ctx.attr.dependency))

forward_dependency = repository_rule(
    implementation = _forward_dependency,
    attrs = {"dependency": attr.label(mandatory = True)},
)
