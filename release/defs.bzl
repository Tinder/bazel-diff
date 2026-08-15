"""Bazel-side naming for the Rust CLI binaries published to GitHub Releases.

`bazel build //release:bazel-diff-rust --config=release` writes
`bazel-bin/release/bazel-diff-rust-<os>-<arch>[.exe]` for whatever platform Bazel
is building for. Release automation therefore only has to run Bazel and upload
what lands in `bazel-bin/release/`: no per-runner `cp`, no shell-side knowledge
of where rules_rust drops the binary, and no way for the published asset name to
disagree with the platform it was actually built on.
"""

_OS_NAME = select(
    {
        "@platforms//os:linux": "linux",
        "@platforms//os:macos": "macos",
        "@platforms//os:windows": "windows",
    },
    no_match_error = "bazel-diff does not publish release binaries for this OS",
)

_ARCH_NAME = select(
    {
        "@platforms//cpu:aarch64": "arm64",
        "@platforms//cpu:x86_64": "amd64",
    },
    no_match_error = "bazel-diff does not publish release binaries for this CPU",
)

_EXTENSION = select({
    "@platforms//os:windows": ".exe",
    "//conditions:default": "",
})

# The asset name GitHub Releases publishes, e.g. "bazel-diff-rust-linux-amd64".
RELEASE_ASSET_NAME = "bazel-diff-rust-" + _OS_NAME + "-" + _ARCH_NAME + _EXTENSION

def _release_binary_impl(ctx):
    asset = ctx.actions.declare_file(ctx.attr.asset_name)

    # Bazel copies instead of symlinking on platforms without symlink support
    # (Windows, unless --windows_enable_symlinks), so the output is always a
    # file CI can upload directly.
    ctx.actions.symlink(
        output = asset,
        target_file = ctx.executable.binary,
        progress_message = "Naming release asset %{output}",
    )
    return [DefaultInfo(files = depset([asset]))]

release_binary = rule(
    implementation = _release_binary_impl,
    doc = "Republishes `binary` under the file name used for release assets.",
    attrs = {
        "asset_name": attr.string(
            mandatory = True,
            doc = "File name to give the binary. Use `RELEASE_ASSET_NAME`.",
        ),
        "binary": attr.label(
            mandatory = True,
            executable = True,
            cfg = "target",
            doc = "The binary to publish.",
        ),
    },
)
