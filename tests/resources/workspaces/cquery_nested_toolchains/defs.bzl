"""Two readers force a forwarding toolchain onto different execution platforms."""

def _inner(ctx):
    return [platform_common.ToolchainInfo(payload = ctx.file.payload)]

inner = rule(
    implementation = _inner,
    attrs = {"payload": attr.label(allow_single_file = True)},
)

def _outer(ctx):
    return [ctx.toolchains[Label("//:inner_type")]]

outer = rule(implementation = _outer, toolchains = ["//:inner_type"])

def _reader(ctx):
    source = ctx.toolchains[Label("//:outer_type")].payload
    return [DefaultInfo(files = depset([source]))]

reader = rule(implementation = _reader, toolchains = ["//:outer_type"])
