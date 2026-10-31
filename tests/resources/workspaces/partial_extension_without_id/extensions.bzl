"""A healthy extension beside a metadata failure without a combined extension ID."""

def _repo_impl(repo_ctx):
    repo_ctx.file("BUILD.bazel", "exports_files([\"data\"])\n")
    repo_ctx.file("data", "healthy\n")

_repo = repository_rule(implementation = _repo_impl)

def _healthy_impl(_module_ctx):
    _repo(name = "healthy")

healthy = module_extension(implementation = _healthy_impl)

def _broken_impl(module_ctx):
    _repo(name = "generated")
    return module_ctx.extension_metadata(
        root_module_direct_deps = ["not_generated"],
        root_module_direct_dev_deps = [],
    )

broken = module_extension(implementation = _broken_impl)
