"""Crate dependencies of the root Cargo package.

`rules_rs` stores that package in `DEP_DATA` under the empty string. Passing
`package_name = ""` to `all_crate_deps` does not look that key up: Starlark
treats `""` as false, so the helper uses `native.package_name()` (`src` or
`tests`) and returns no dependencies.

A `single_version_override` patch can fix the helper, but Bazel only applies
overrides from the root module. `@bazel-diff//:bazel-diff` is built as a
dependency in the BCR anonymous module, the override is ignored, and rustc
then fails with unresolved crates (`serde`, `buffa`, `rayon`, ...).

Read `DEP_DATA[""]` here instead, so the binary builds for dependents with
no root-module patch.
"""

load("@bd_crates//:data.bzl", "DEP_DATA")

def _root_package():
    data = DEP_DATA.get("")
    if not data:
        fail("rules_rs did not record the root Cargo package at DEP_DATA['']")
    return data

def _deps(kinds):
    data = _root_package()
    deps = []
    for kind in kinds:
        by_platform = data[kind + "_by_platform"]
        if by_platform:
            fail("platform-specific %s are not wired through crate_deps.bzl: %s" % (kind, by_platform))
        deps.extend(data[kind])
    return sorted(deps)

def normal_crate_deps():
    """Normal dependencies of the root Cargo package."""
    return _deps(["deps"])

def normal_and_dev_crate_deps():
    """Normal and dev dependencies of the root Cargo package."""
    return _deps(["deps", "dev_deps"])
