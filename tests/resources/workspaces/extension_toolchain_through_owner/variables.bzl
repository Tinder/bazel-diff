def _variables_impl(ctx):
    return [platform_common.TemplateVariableInfo({"PROVIDER_VALUE": ctx.attr.value})]

variables = rule(
    implementation = _variables_impl,
    attrs = {"value": attr.string()},
)
