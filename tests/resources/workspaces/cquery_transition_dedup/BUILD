load(
    ":defs.bzl",
    "bootstrap_flag",
    "forward",
    "helper",
    "node",
    "observed_aspect_copy",
    "observed_copy",
)

bootstrap_flag(
    name = "bootstrap",
    build_setting_default = False,
)

config_setting(
    name = "bootstrapping",
    flag_values = {":bootstrap": "true"},
)

# Default configuration: app -> helper -> app[bootstrap=true] -> (nothing).
# The configured graph is acyclic and has two distinct `app` instances; only the
# default-configuration one depends on `helper`.
node(
    name = "app",
    deps = select({
        ":bootstrapping": [],
        "//conditions:default": [":helper"],
    }),
)

helper(
    name = "helper",
    src = "input.txt",
    dep = ":app",
)

genrule(
    name = "shared",
    srcs = select({
        ":bootstrapping": ["input.txt"],
        "//conditions:default": ["fixed.txt"],
    }),
    outs = ["shared.out"],
    cmd = 'cat "$(SRCS)" > "$@"',
)

forward(
    name = "forwarded",
    dep = ":shared",
)

genrule(
    name = "wrapper",
    srcs = [":forwarded"],
    outs = ["wrapper.out"],
    cmd = 'cat "$(SRCS)" > "$@"',
)

alias(
    name = "root_alias",
    actual = ":wrapper",
)

observed_copy(
    name = "observed",
    src = "fixed.txt",
)

filegroup(
    name = "observed_changed_input",
    srcs = ["input.txt"],
)

observed_aspect_copy(
    name = "observed_changed",
    src = ":observed_changed_input",
)
