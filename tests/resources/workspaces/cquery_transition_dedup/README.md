# cquery configuration identity test workspace

Reproduces [GitHub Issue #512](https://github.com/Tinder/bazel-diff/issues/512):
`generate-hashes --useCquery` collapses configured targets by label, so when one
label appears in two configurations only one instance survives, and its
dependency edges are the only ones hashed.

## The graph

`helper` depends on `app` through a Starlark transition that forces the
`//:bootstrap` flag on. `app` selects its deps on that flag:

| configured target        | deps        |
| ------------------------ | ----------- |
| `//:app` (default)       | `//:helper` |
| `//:helper` (default)    | `//:app` (bootstrap=true), `input.txt` |
| `//:app` (bootstrap=true)| none        |

`bazel cquery 'deps(//:app)'` therefore lists `//:app` twice. The configured
graph is acyclic. The unconfigured graph is not: plain `bazel query` sees every
branch of the `select()`, so `app -> helper -> app` is a real cycle there and
`generate-hashes` without `--useCquery` reports it. This fixture is only
meaningful under `--useCquery`.

## Expected behaviour

Editing `input.txt` changes `//:helper`, and through it the top-level `//:app`.
The dependency edges for `//:app` include `//:helper`.

## Behaviour before the fix

bazel-diff keyed the cquery results by label, so one of the two `//:app`
instances was dropped and every edge into `//:app` resolved to the survivor.
Which instance survived followed the cquery output order, and each outcome
reproduced one of the symptoms in the issue:

- The default-configuration instance survives (what Bazel 8.7 produces here):
  `//:helper`'s edge to `//:app` (bootstrap=true) now points at the instance
  that depends on `//:helper`, and `generate-hashes` fails with
  `Circular dependency detected: @@//:app -> @@//:helper -> @@//:app`. This is
  the same shape as the false LLVM toolchain cycle in the issue, where a
  toolchain in one configuration reaches the same toolchain label in another.
- The bootstrap instance survives: `//:app` has no deps, hashes as unchanged
  when `input.txt` changes, and its dep edges omit `//:helper`.

## Behaviour after the fix

Configured targets are keyed by `label|configuration checksum`, the same
identity `configured_rule_input` gives each edge, so both `//:app` instances
are hashed and each edge resolves to the instance it names. The hash file is
still keyed by label: a label with several instances hashes all of them, and
its dependency edges are the union of theirs.
