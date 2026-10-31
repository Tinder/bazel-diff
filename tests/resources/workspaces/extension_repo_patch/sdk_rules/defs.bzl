"""A toolchain backed by the extension's SDK repo and a rule that uses it."""

SdkInfo = provider(fields = ["version_file"])

def _sdk_toolchain_impl(ctx):
    return [platform_common.ToolchainInfo(sdk = SdkInfo(version_file = ctx.file.version_file))]

sdk_toolchain = rule(
    implementation = _sdk_toolchain_impl,
    attrs = {"version_file": attr.label(allow_single_file = True)},
)

def _sdk_user_impl(ctx):
    sdk = ctx.toolchains["//:toolchain_type"].sdk
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        inputs = [sdk.version_file],
        outputs = [out],
        command = "cp \"$1\" \"$2\"",
        arguments = [sdk.version_file.path, out.path],
    )
    return [DefaultInfo(files = depset([out]))]

# `_runtime` mirrors rules_go's `_go_context_data`: an implicit dependency on the
# rule set's own repository that every target of the rule carries.
sdk_user = rule(
    implementation = _sdk_user_impl,
    attrs = {
        "_runtime": attr.label(default = "//:runtime_marker.txt", allow_single_file = True),
    },
    toolchains = ["//:toolchain_type"],
)
