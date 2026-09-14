"""The macro, in a SEPARATE file from the rule it instantiates.

Editing it must flip consumers via the instantiation-stack seed (this file is in the call chain)."""

load(":rule.bzl", "split_rule")

def split(name, **kwargs):
    split_rule(name = name, **kwargs)
