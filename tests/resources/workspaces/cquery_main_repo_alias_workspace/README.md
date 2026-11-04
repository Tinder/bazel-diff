# Main-repository alias under cquery, in WORKSPACE mode

The WORKSPACE-mode counterpart of `cquery_main_repo_alias` (GitHub Issue #612).

## The graph

`//:wrapper` (a `genrule`) lists `//:tool` in its `srcs`. `//:tool` is an
`alias` of `//bar:baz`, a `filegroup` over `bar/baz.txt`.

## Expected behaviour

Changing `bar/baz.txt` impacts `//bar:baz.txt`, `//bar:baz`, `//:tool`,
`//:wrapper` and `//:wrapper.txt`, in query mode and in cquery mode alike.

## Behaviour before the fix

The compatible-target cquery names an alias by `repr(target)`, which prints a
main-repository alias with its apparent label (`//:tool`). The fix for #612
prefixed it with `@@`, the main repository's canonical form under Bzlmod. In
WORKSPACE mode, though, `--consistent_labels` prints main-repository targets
as `@//:tool`, so `@@//:tool` still never matches, the alias is dropped as if
it were incompatible, and so is every edge from `//:wrapper` through it. Under
cquery only `//bar:baz` and `//bar:baz.txt` are reported as impacted.
