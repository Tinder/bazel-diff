load("//defs:macro.bzl", "split")

# Cross-file macro -> rule: must flip when EITHER defs/macro.bzl or defs/rule.bzl is edited.
split(name = "s")

# Unrelated native target in the same package: must NOT flip on either edit.
genrule(name = "unrelated_gen", outs = ["u.txt"], cmd = "echo u > $@")
