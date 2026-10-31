def _registry_impl(ctx):
    ctx.file("dl", ctx.read(Label("@@//:registry.txt")))
    ctx.file("BUILD.bazel", 'exports_files(["dl"])')

_registry = repository_rule(implementation = _registry_impl)

def _hub_impl(ctx):
    version = ctx.attr.version
    ctx.file("defs.bzl", "VALUE = %s # %s" % (repr(version.split("-")[0]), version))
    # The same Bazel alias can also have a repository-time file at its location.
    text = ctx.read(Label("@@//:alias.txt"))
    if text:
        ctx.file("app", text)
    ctx.file("BUILD.bazel", 'alias(name = "app", actual = "@app//:shared", visibility = ["//visibility:public"])')

_hub = repository_rule(
    implementation = _hub_impl,
    attrs = {"version": attr.string()},
)

# Older Bazel releases only expose the scalar form of this fixture's label.
_has_label_list_dict = hasattr(attr, "label_list_dict")

def _spoke_impl(ctx):
    ctx.file("registry_copy", ctx.read(ctx.attr.registry))
    alias = ctx.attr.aliases["default"][0] if _has_label_list_dict else ctx.attr.aliases
    path = ctx.path(alias)
    ctx.watch(path)
    ctx.file("alias_copy", ctx.read(path).split("\n")[0] if path.exists else "absent")
    ctx.file("BUILD.bazel", """load("@hub//:defs.bzl", "VALUE")

exports_files(["registry_copy", "alias_copy"])
genrule(
    name = "shared",
    outs = ["shared.txt"],
    cmd = "echo " + VALUE + " > $@",
    visibility = ["//visibility:public"],
)
""")

_spoke = repository_rule(
    implementation = _spoke_impl,
    attrs = {
        "aliases": attr.label_list_dict() if _has_label_list_dict else attr.label(),
        "registry": attr.label(),
        "version": attr.string(),
    },
)

def _family_impl(ctx):
    for module in ctx.modules:
        for tag in module.tags.config:
            settings = json.decode(ctx.read(tag.settings))
            _hub(name = "hub", version = settings["version"])
            _registry(name = "registry")
            alias = "@hub//:app"
            _spoke(
                name = "app",
                aliases = {"default": [alias]} if _has_label_list_dict else alias,
                registry = "@registry//:dl",
                version = settings["spoke"],
            )

family = module_extension(
    implementation = _family_impl,
    tag_classes = {"config": tag_class(attrs = {"settings": attr.label()})},
)
