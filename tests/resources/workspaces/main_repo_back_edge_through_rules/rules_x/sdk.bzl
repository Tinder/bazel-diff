def _checker_repo_impl(rctx):
    rctx.file("BUILD.bazel", """alias(
    name = "checker",
    actual = "{}",
    visibility = ["//visibility:public"],
)
""".format(rctx.attr.checker))

# Like rules_go's `go_register_nogo`: the label arrives as a string attribute.
_checker_repo = repository_rule(
    implementation = _checker_repo_impl,
    attrs = {"checker": attr.string()},
)

def _sdk_impl(mctx):
    checker = None
    for mod in mctx.modules:
        for tag in mod.tags.checker:
            checker = str(tag.checker)
    _checker_repo(name = "x_checker", checker = checker or "@@rules_x+//:no_checker")

sdk = module_extension(
    implementation = _sdk_impl,
    tag_classes = {"checker": tag_class(attrs = {"checker": attr.label()})},
)
