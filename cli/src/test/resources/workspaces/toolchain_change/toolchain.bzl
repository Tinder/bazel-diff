"""Custom toolchain for reproducing https://github.com/Tinder/bazel-diff/issues/101.

The `greeter` rule resolves a `greeting_toolchain_type` toolchain at analysis time.
The BUILD file does *not* list the toolchain in `greeter`'s deps; bazel toolchain
resolution wires it up implicitly. As a result `bazel query 'deps(//:my_greeter)'`
does not contain the toolchain target, and bazel-diff (which runs `bazel query` by
default) does not detect changes to the toolchain's configuration.
"""

def _greeting_toolchain_impl(ctx):
    return [platform_common.ToolchainInfo(greeting = ctx.attr.greeting)]

greeting_toolchain = rule(
    implementation = _greeting_toolchain_impl,
    attrs = {"greeting": attr.string(mandatory = True)},
)

def _greeter_impl(ctx):
    toolchain = ctx.toolchains["//:greeting_toolchain_type"]
    out = ctx.actions.declare_file(ctx.attr.name + ".txt")
    ctx.actions.write(out, toolchain.greeting)
    return [DefaultInfo(files = depset([out]))]

greeter = rule(
    implementation = _greeter_impl,
    toolchains = ["//:greeting_toolchain_type"],
)
