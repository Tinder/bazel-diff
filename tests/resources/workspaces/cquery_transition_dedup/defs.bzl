"""Rules that produce two configured instances of one label.

`helper` reaches its `dep` through a transition that forces `//:bootstrap` on, so
`deps(//:app)` contains `//:app` twice: once in the default configuration, where
its `select()` picks `[":helper"]`, and once with bootstrap=true, where the
`select()` is empty. The label is the same; the configured targets are not.
"""

def _impl(_ctx):
    return []

bootstrap_flag = rule(
    implementation = _impl,
    build_setting = config.bool(flag = True),
)

node = rule(
    implementation = _impl,
    attrs = {
        "deps": attr.label_list(),
    },
)

def _bootstrap_impl(_settings, _attr):
    return {"//:bootstrap": True}

_bootstrap = transition(
    implementation = _bootstrap_impl,
    inputs = [],
    outputs = ["//:bootstrap"],
)

helper = rule(
    implementation = _impl,
    attrs = {
        "dep": attr.label(cfg = _bootstrap),
        "src": attr.label(allow_single_file = True),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
