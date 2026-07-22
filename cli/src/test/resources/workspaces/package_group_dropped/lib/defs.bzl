"""Macro that creates the visibility package_group for `//lib`.

The `packages` allow-list lives here (not in the BUILD file) so that the ONLY
change between the two checkouts is to a `.bzl` that feeds a `package_group`.
The BUILD files, and every native rule in them, are byte-for-byte identical
across the two revisions -- so the sole semantic change is carried entirely by
the PACKAGE_GROUP target. This mirrors issue #441's macro-created
package_group experiment. Flip `ALLOWED` to `[]` in the "after" checkout to
revoke `//consumer`'s visibility of `//lib:thing`.
"""

ALLOWED = ["//consumer"]

def define_consumers(name):
    native.package_group(
        name = name,
        packages = ALLOWED,
    )
