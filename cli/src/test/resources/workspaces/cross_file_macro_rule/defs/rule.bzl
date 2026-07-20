"""The rule class, in a SEPARATE file from the macro that instantiates it.

Editing it must flip consumers via `$rule_implementation_hash` (its definition environment),
even though it is not in the instantiation call stack."""

def _split_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "split")
    return [DefaultInfo(files = depset([out]))]

split_rule = rule(implementation = _split_impl)
