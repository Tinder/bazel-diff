"""Bzlmod extension that fetches repositories for aspect_rules_lint format (e.g. ktfmt)."""

load("@aspect_rules_lint//format:repositories.bzl", "fetch_ktfmt")

def _non_module_repositories_impl(module_ctx):
    fetch_ktfmt()

non_module_repositories = module_extension(_non_module_repositories_impl)
