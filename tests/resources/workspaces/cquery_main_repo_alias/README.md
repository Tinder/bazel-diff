# Main-repository alias under cquery

Reproduces [GitHub Issue #612](https://github.com/Tinder/bazel-diff/issues/612):
with `--useCquery`, a target that reaches its inputs through a main-repository
`alias` stops seeing changes to them.

## The graph

`//:wrapper` (a `genrule`) lists `//:tool` in its `srcs`. `//:tool` is an
`alias` of `//bar:baz`, a `filegroup` over `bar/baz.txt`.

## Expected behaviour

Changing `bar/baz.txt` impacts `//bar:baz.txt`, `//bar:baz`, `//:tool`,
`//:wrapper` and `//:wrapper.txt`, in query mode and in cquery mode alike.

## Behaviour before the fix

The compatible-target cquery that filters out `IncompatiblePlatformProvider`
targets names an alias by `repr(target)`, and Bazel prints a main-repository
alias with its apparent label (`//:tool`) even under `--consistent_labels`,
while every other target is canonical (`@@//:tool`). The two never match, so
the alias is dropped as if it were incompatible, and with it every edge from
`//:wrapper` through it. Under cquery only `@@//bar:baz` and `@@//bar:baz.txt`
are reported as impacted.
