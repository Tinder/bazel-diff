"""A module extension whose repository rule always fails to fetch.

This models the real-world failure described in the bug report: a repo rule that
cannot be resolved (e.g. a transient network error while fetching a Go/remote
dependency). Module resolution succeeds -- the extension just *declares* the repo --
but any attempt to actually fetch `@failing_dep` blows up in the repository rule's
implementation function, which is exactly what happens when Bazel tries to load a
package that depends on the repo.
"""

def _failing_repo_impl(_rctx):
    fail(
        "simulated network error resolving repository rule: " +
        "Get \"https://proxy.golang.org/...\": net/http: TLS handshake timeout",
    )

failing_repo = repository_rule(implementation = _failing_repo_impl)

def _failing_ext_impl(_mctx):
    failing_repo(name = "failing_dep")

failing_ext = module_extension(implementation = _failing_ext_impl)
