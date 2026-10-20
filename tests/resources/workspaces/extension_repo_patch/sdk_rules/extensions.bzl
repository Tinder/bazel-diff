"""A module extension that fetches an SDK the way rules_go's go_sdk does."""

def _sdk_repo_impl(rctx):
    rctx.file("version.txt", rctx.attr.version + "\n")
    rctx.file("BUILD.bazel", 'exports_files(["version.txt"])\n')
    for patch in rctx.attr.patches:
        rctx.patch(patch, strip = rctx.attr.patch_strip)

_sdk_repo = repository_rule(
    implementation = _sdk_repo_impl,
    attrs = {
        "version": attr.string(),
        "patches": attr.label_list(),
        "patch_strip": attr.int(default = 1),
    },
)

def _sdk_impl(mctx):
    version = "1.0.0"
    patches = []
    for mod in mctx.modules:
        for tag in mod.tags.download:
            if tag.version:
                version = tag.version
            patches += tag.patches
    _sdk_repo(name = "default_sdk", version = version, patches = patches)

sdk = module_extension(
    implementation = _sdk_impl,
    tag_classes = {
        "download": tag_class(attrs = {
            "version": attr.string(),
            "patches": attr.label_list(),
        }),
    },
)
