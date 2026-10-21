"""A module extension whose generated repositories no longer match its usage.

The extension creates `generated_repo` only. transitive_dep's MODULE.bazel still
imports `missing_repo`, so evaluating the extension fails with

    module extension "broken" ... does not generate repository "missing_repo",
    yet it is imported as "missing_repo" in the usage at ...

Bazel only evaluates the extension when something asks for one of its
repositories, so a build that never does stays green.
"""

def _generated_repo_impl(rctx):
    rctx.file("BUILD.bazel", "")

_generated_repo = repository_rule(implementation = _generated_repo_impl)

def _broken_impl(_mctx):
    _generated_repo(name = "generated_repo")

broken = module_extension(implementation = _broken_impl)
