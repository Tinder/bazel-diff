# Two native genrules that do NOT load any `.bzl` file. They have nothing to do with
# the macro the reproducer adds in a separate package, so adding that macro must not
# change their hashes (https://github.com/Tinder/bazel-diff/issues/365).
genrule(
    name = "a",
    outs = ["a.txt"],
    cmd = "echo a > $@",
)

genrule(
    name = "b",
    outs = ["b.txt"],
    cmd = "echo b > $@",
)
