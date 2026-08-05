"""A Starlark rule `_thing` (+ co-located macro `thing`) whose definition digest is `$rule_implementation_hash`.

Editing this file must impact targets that instantiate `thing`, but MUST NOT
impact unrelated native targets that merely share a package with one -- that
over-invalidation is what packageBzlSeeds caused before instantiation-stack
attribution."""

def _thing_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "thing")
    return [DefaultInfo(files = depset([out]))]

_thing = rule(implementation = _thing_impl)

def thing(name, **kwargs):
    _thing(name = name, **kwargs)
