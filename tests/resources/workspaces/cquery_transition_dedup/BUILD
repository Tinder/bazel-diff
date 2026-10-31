load(":defs.bzl", "bootstrap_flag", "helper", "node")

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
    dep = ":app",
    src = "input.txt",
)
