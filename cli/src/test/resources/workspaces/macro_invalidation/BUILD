load(":miniature.bzl", "miniature")

# Source consumed by the macro-generated genrule. Used so the test workspace builds
# without external rules; the macro itself is what we're testing for invalidation.
exports_files(["image.png"])

miniature(
    name = "logo_miniature",
    src = "image.png",
)
