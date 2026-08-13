//! Internal implementation modules for the experimental Rust CLI.
//!
//! `bazel-diff` is distributed and supported as a command-line tool. This
//! library target exists so Bazel and Cargo can share implementation code and
//! tests; its module-level API is not a stability commitment.

pub mod bazel;
pub mod fingerprint;
pub mod hash;
pub mod model;
pub mod module_graph;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/proto.rs"));
}
pub mod server;
