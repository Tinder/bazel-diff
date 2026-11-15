#![warn(clippy::pedantic)]
// See src/lib.rs in the root crate for why these pedantic lints are not adopted.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::similar_names,
    clippy::case_sensitive_file_extension_comparisons
)]

use std::collections::BTreeMap;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let code = lcov_merger::run(&argv, &env, &mut std::io::stderr());
    std::process::exit(code);
}
