# Regression-test workspace for the under-invalidation (false-negative) half of
# https://github.com/Tinder/bazel-diff/issues/441.
#
# bazel-diff used to drop any target whose query `Discriminator` is not RULE /
# SOURCE_FILE / GENERATED_FILE, so a `PACKAGE_GROUP` change -- which genuinely
# alters downstream visibility and can flip a consumer from building to failing
# -- was invisible and `get-impacted-targets` reported NOTHING. Package_groups
# are now lowered to synthetic rules and hashed, and RuleHasher follows the
# (nodep) `visibility` edge from a gated rule to its package_group.
#
# `consumers` is created by a macro (see defs.bzl) whose allow-list lives in
# that `.bzl`. Between the two checkouts we edit ONLY defs.bzl (ALLOWED -> []),
# so the BUILD files and `:thing` stay byte-identical; the only semantic change
# rides entirely on the PACKAGE_GROUP target. `:thing` is declared directly
# (NOT via the macro) so its per-rule `.bzl` seed never picks up defs.bzl --
# isolating the package_group behaviour from the separate `.bzl` provenance
# behaviour that is the over-invalidation half of #441.

load(":defs.bzl", "define_consumers")

define_consumers(name = "consumers")

genrule(
    name = "thing",
    outs = ["thing.txt"],
    cmd = "echo thing > $@",
    visibility = [":consumers"],
)
