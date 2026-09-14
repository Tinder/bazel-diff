"""Hub-and-spoke external repos modeled after rules_python's pip_parse (WORKSPACE mode).

- One spoke repo per package whose repository-rule attrs embed the pinned
  version, so a version bump flips the //external:<spoke> seed hash.
- A hub repo of alias packages that consumers reference:
  @pip//numpy:pkg -> @pip_numpy//:lib.
"""

def _fake_spoke_impl(rctx):
    rctx.file("WORKSPACE", "workspace(name = \"{}\")\n".format(rctx.name))
    rctx.file(
        "BUILD",
        "filegroup(\n" +
        "    name = \"lib\",\n" +
        "    srcs = [\"payload.txt\"],\n" +
        "    visibility = [\"//visibility:public\"],\n" +
        ")\n",
    )
    rctx.file("payload.txt", "version {}\n".format(rctx.attr.version))

fake_spoke = repository_rule(
    implementation = _fake_spoke_impl,
    # The version literal in this attr is the change-detection signal, exactly
    # like the requirement string in a real pip spoke repo.
    attrs = {"version": attr.string(mandatory = True)},
)

def _fake_hub_impl(rctx):
    rctx.file("WORKSPACE", "workspace(name = \"{}\")\n".format(rctx.name))
    rctx.file("BUILD", "")
    rctx.file(
        "numpy/BUILD",
        "alias(\n" +
        "    name = \"pkg\",\n" +
        "    actual = \"@{}//:lib\",\n".format(rctx.attr.spoke) +
        "    visibility = [\"//visibility:public\"],\n" +
        ")\n",
    )

fake_hub = repository_rule(
    implementation = _fake_hub_impl,
    attrs = {"spoke": attr.string(mandatory = True)},
)
