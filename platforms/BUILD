# Platforms bazel-diff publishes release binaries for, and the constraint that
# separates them from the platforms it is ordinarily built and tested on.
#
# Bazel has no built-in notion of which libc a platform uses, and neither does
# rules_rust: `abi_to_constraints` in @rules_rust//rust/platform:triple_mappings
# maps both x86_64-unknown-linux-gnu and x86_64-unknown-linux-musl to nothing
# more than {os:linux, cpu:x86_64}. The setting below supplies that missing
# vocabulary so a musl toolchain can be selected without the glibc one matching
# too.

package(default_visibility = ["//visibility:public"])

# `system_libc` is the default, so @platforms//host and every stock platform
# satisfies it without declaring anything: only the musl platforms below opt out.
constraint_setting(
    name = "libc",
    default_constraint_value = ":system_libc",
)

# Whatever libc the platform ships -- glibc on a normal Linux distribution, and
# by extension the C runtime on macOS and Windows.
constraint_value(
    name = "system_libc",
    constraint_setting = ":libc",
)

constraint_value(
    name = "musl",
    constraint_setting = ":libc",
)

# Toolchain gates. See the `rust.toolchain` / `rust.repository_set` tags in
# MODULE.bazel, which use these to keep the glibc and musl Rust toolchains from
# both matching the same build.
config_setting(
    name = "is_system_libc",
    constraint_values = [":system_libc"],
)

config_setting(
    name = "is_musl",
    constraint_values = [":musl"],
)

# Platforms the published Linux binaries are built for:
# `bazel build //release:bazel-diff-rust --config=release-musl` (amd64) and
# `--config=release-musl-arm64` (arm64). Statically linked against musl, so the
# binary does not depend on the glibc version of the machine that runs it.
# Reachable from a glibc Linux host and from an Apple Silicon Mac -- both the
# Rust std and the C toolchain cross-compile.
platform(
    name = "linux_x86_64_musl",
    constraint_values = [
        "@platforms//cpu:x86_64",
        "@platforms//os:linux",
        ":musl",
    ],
)

platform(
    name = "linux_aarch64_musl",
    constraint_values = [
        "@platforms//cpu:aarch64",
        "@platforms//os:linux",
        ":musl",
    ],
)
