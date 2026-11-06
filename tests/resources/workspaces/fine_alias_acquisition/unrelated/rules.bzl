def _probe(ctx):
    return [DefaultInfo()]

probe = rule(implementation = _probe)
