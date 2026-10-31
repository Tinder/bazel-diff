def _repo_impl(rctx):
    rctx.file("BUILD.bazel", "exports_files([\"x\"])\n")
    rctx.file("x", "healthy")

_repo = repository_rule(implementation = _repo_impl)

def _extension_impl(_mctx):
    _repo(name = "healthy_repo")

repos = module_extension(implementation = _extension_impl)
