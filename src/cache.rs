//! The cache key contract every producer of hash entries shares.
//!
//! `serve` and the one-shot CLI must agree on it exactly, so an entry one of them wrote is found
//! and trusted by the other: the key is the revision's git *tree* (not its commit, so a rebase or
//! an amended message shares the entry) joined with a digest of everything in the configuration
//! that can change hash content. Retention, tiers and remote locations are deliberately not in
//! it.

use crate::hash::HashOptions;
use crate::partial;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Digests the hash-affecting configuration: the package version, every query and hashing
/// option, whether dependency edges are recorded, and, only when on, partial-graph mode (its
/// entries hash under a different formula and carry recipes, so they are never served in place
/// of a default-mode entry; folding the flag in only when enabled keeps existing keys put).
pub fn configuration_fingerprint(
    options: &HashOptions,
    track_deps: bool,
    partial_graph: bool,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.update([0]);
    for value in [
        options.bazel.startup_options.join("\0"),
        options.bazel.command_options.join("\0"),
        options.bazel.cquery_options.join("\0"),
        options.bazel.use_cquery.to_string(),
        options.bazel.cquery_expression.clone().unwrap_or_default(),
        options.bazel.keep_going.to_string(),
        options.bazel.exclude_external_targets.to_string(),
        options
            .bazel
            .exclude_targets_query
            .clone()
            .unwrap_or_default(),
        options
            .bazel
            .fine_grained_external_repos
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
        options
            .bazel
            .label_only_module_extensions
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
        options
            .ignored_attributes
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(","),
        options
            .seed_filepaths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\0"),
        options
            .always_affected_tags
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(","),
        track_deps.to_string(),
        // Full-content entries hash git blob ids rather than file bytes; keep them apart from
        // entries a build without this seeding wrote under the same package version.
        String::from("sourceContent=git-blob"),
    ] {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    if partial_graph {
        hasher.update(format!("partialGraph={}", partial::RECIPE_VERSION).as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())[..12].to_owned()
}

/// The key of a revision's entry: its tree and the configuration fingerprint, plus a digest of
/// the `modifiedFilepaths` set for a scoped entry so it is never served as the full one.
pub fn cache_key(tree: &str, fingerprint: &str, modified: &BTreeSet<PathBuf>) -> String {
    let base = format!("{tree}.{fingerprint}");
    if modified.is_empty() {
        return base;
    }
    let mut hasher = Sha256::new();
    for path in modified {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(b"\n");
    }
    format!("{base}.{}", &hex::encode(hasher.finalize())[..12])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bazel::BazelOptions;
    use std::collections::HashSet;

    fn options() -> HashOptions {
        HashOptions {
            bazel: BazelOptions {
                workspace: PathBuf::from("."),
                bazel: PathBuf::from("bazel"),
                startup_options: Vec::new(),
                command_options: Vec::new(),
                cquery_options: Vec::new(),
                use_cquery: false,
                cquery_expression: None,
                keep_going: false,
                fine_grained_external_repos: BTreeSet::new(),
                label_only_module_extensions: BTreeSet::new(),
                exclude_external_targets: false,
                exclude_targets_query: None,
                no_bazelrc: false,
                verbose: false,
                cancellation: None,
            },
            content_hashes: None,
            ignored_attributes: HashSet::new(),
            seed_filepaths: BTreeSet::new(),
            modified_filepaths: BTreeSet::new(),
            track_deps: false,
            always_affected_tags: HashSet::new(),
            partial_tree: None,
        }
    }

    #[test]
    fn fingerprint_moves_with_hash_affecting_options_only() {
        let base = configuration_fingerprint(&options(), false, false);
        assert_eq!(base.len(), 12);
        assert_eq!(configuration_fingerprint(&options(), false, false), base);

        let mut cquery = options();
        cquery.bazel.use_cquery = true;
        assert_ne!(configuration_fingerprint(&cquery, false, false), base);
        let mut seeded = options();
        seeded.seed_filepaths.insert(PathBuf::from("seed"));
        assert_ne!(configuration_fingerprint(&seeded, false, false), base);
        let mut ignored = options();
        ignored.ignored_attributes.insert("visibility".into());
        assert_ne!(configuration_fingerprint(&ignored, false, false), base);
        assert_ne!(configuration_fingerprint(&options(), true, false), base);

        // Partial-graph mode has its own key space; the default mode's keys stay put.
        let partial = configuration_fingerprint(&options(), false, true);
        assert_ne!(partial, base);
        assert_eq!(configuration_fingerprint(&options(), false, true), partial);
    }

    #[test]
    fn keys_join_tree_and_fingerprint_and_scope_modified_sets() {
        let full = cache_key("abc", "fp", &BTreeSet::new());
        assert_eq!(full, "abc.fp");
        let scoped = cache_key(
            "abc",
            "fp",
            &BTreeSet::from([PathBuf::from("b"), PathBuf::from("a")]),
        );
        assert!(scoped.starts_with("abc.fp."));
        assert_eq!(scoped.len(), "abc.fp.".len() + 12);
        assert_eq!(
            scoped,
            cache_key(
                "abc",
                "fp",
                &BTreeSet::from([PathBuf::from("a"), PathBuf::from("b")])
            )
        );
        assert_ne!(
            scoped,
            cache_key("abc", "fp", &BTreeSet::from([PathBuf::from("a")]))
        );
    }
}
