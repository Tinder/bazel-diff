load("//defs:thing.bzl", "thing")

# Uses the rule from thing.bzl -> MUST flip when thing.bzl is edited.
thing(name = "t")

# Unrelated native targets in the same package -> MUST NOT flip on a thing.bzl edit.
genrule(name = "native_gen", outs = ["g.txt"], cmd = "echo x > $@")
filegroup(name = "fg", srcs = ["data.txt"])
exports_files(["data.txt"])
