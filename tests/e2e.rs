//! End-to-end suite: drives the real `bazel-diff` binary against fixture workspaces.

#![warn(clippy::pedantic)]
// The fixtures are case-sensitive Bazel paths, and the suite is test code with
// long, linear scenarios; see src/lib.rs for the reasoning behind each allow.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::case_sensitive_file_extension_comparisons
)]

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
