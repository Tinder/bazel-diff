//! End-to-end suite: drives the real `bazel-diff` binary against fixture workspaces.

#[path = "e2e/core.rs"]
mod core;
#[path = "e2e/example.rs"]
mod example;
#[path = "e2e/external.rs"]
mod external;
#[path = "e2e/regressions.rs"]
mod regressions;
#[path = "e2e/support/mod.rs"]
mod support;
