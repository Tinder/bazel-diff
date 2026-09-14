# A perfectly healthy target that has nothing to do with the broken repo. It loads
# and hashes fine on its own. The bug is that when the sibling //bad package fails to
# load, `--keep_going` (the default) lets bazel-diff silently emit hashes for //good
# alone -- producing a hash set that is missing //bad without any error.
genrule(
    name = "good",
    outs = ["good.txt"],
    cmd = "echo good > $@",
    visibility = ["//visibility:public"],
)
