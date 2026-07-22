# Reproducer workspace for https://github.com/Tinder/bazel-diff/issues/441
#
# The under-invalidation (false-negative) half of the issue: bazel-diff drops
# any target whose query `Discriminator` is not RULE / SOURCE_FILE /
# GENERATED_FILE. `PACKAGE_GROUP` is one such dropped type
# (BazelQueryService.toBazelTarget -> logs "Unsupported target type" and
# returns null). Because the package_group is never hashed, a change to its
# `packages` list -- which genuinely alters downstream visibility and can flip
# a consumer from building to failing -- is invisible to bazel-diff, so
# `get-impacted-targets` reports NOTHING.
#
# `consumers` is created by a macro (see defs.bzl) whose allow-list lives in
# that `.bzl`. Between the two checkouts we edit ONLY defs.bzl (ALLOWED -> []),
# so the BUILD files and `:thing` stay byte-identical; the only semantic change
# rides entirely on the dropped PACKAGE_GROUP target. `:thing` is declared
# directly (NOT via the macro) so its per-rule `.bzl` seed never picks up
# defs.bzl -- isolating the dropped-package_group bug from the separate `.bzl`
# provenance behaviour that is the over-invalidation half of #441.

load(":defs.bzl", "define_consumers")

define_consumers(name = "consumers")

genrule(
    name = "thing",
    outs = ["thing.txt"],
    cmd = "echo thing > $@",
    visibility = [":consumers"],
)
