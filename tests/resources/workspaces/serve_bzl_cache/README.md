# Query-service `.bzl` digest-cache reproducer

This workspace reproduces a false negative in the long-running query service.
It has one native rule that a Starlark macro creates.

The regression test creates two Git revisions. The second revision changes only
the source of `defs.bzl`. It then asks one query-service process to compare the
two revisions with an empty disk cache.

The generated rule must be impacted because bazel-diff uses the macro
instantiation stack as part of that rule's identity. In v38.0.0, the service
keeps the first revision's `.bzl` digest in a process-wide `RuleHasher` cache.
The second revision therefore reuses the first digest and omits
`//:generated`.
