load(":toolchain.bzl", "greeter", "greeting_toolchain")

toolchain_type(name = "greeting_toolchain_type")

greeting_toolchain(
    name = "greeting_toolchain_impl",
    greeting = "Hello",
)

toolchain(
    name = "greeting_toolchain",
    toolchain = ":greeting_toolchain_impl",
    toolchain_type = ":greeting_toolchain_type",
)

# `greeter` declares no explicit toolchain dep; resolution happens at analysis time
# via the registered toolchain. Changing :greeting_toolchain_impl's `greeting` attr
# should impact //:my_greeter, but `bazel query 'deps(//:my_greeter)'` does not
# include the toolchain target so bazel-diff misses the change.
greeter(name = "my_greeter")
