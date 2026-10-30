use crate::support::*;
use serde_json::Value;
use std::fs;
use std::path::Path;

fn integration_case(generate_args: &[&str], impacted_args: &[&str], golden: &str) {
    let project_a = extract_fixture("integration-test-1.zip");
    let project_b = extract_fixture("integration-test-2.zip");
    let workspace_a = project_a.path().join("integration");
    let workspace_b = project_b.path().join("integration");
    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let result = output.path().join("impacted.txt");
    assert!(generate(&workspace_a, &from, generate_args)
        .status
        .success());
    assert!(generate(&workspace_b, &to, generate_args).status.success());
    assert!(impacted(&workspace_b, &from, &to, &result, impacted_args)
        .status
        .success());
    assert_eq!(
        filter_internal(read_lines(&result)),
        filter_internal(golden_lines(golden))
    );
}

#[test]
fn integration_golden() {
    integration_case(&[], &[], "impacted_targets-1-2.txt");
}

#[test]
fn integration_no_keep_going() {
    integration_case(&["--no-keep_going"], &[], "impacted_targets-1-2.txt");
}

#[test]
fn integration_target_types() {
    integration_case(
        &["--includeTargetType"],
        &["-tt", "Rule,SourceFile"],
        "impacted_targets-1-2-rule-sourcefile.txt",
    );
}

#[test]
fn cquery_streamed_proto() {
    let workspace = copy_workspace("distance_metrics");
    let output = workspace.path().join("hashes.json");
    assert!(generate(workspace.path(), &output, &["--useCquery"])
        .status
        .success());
    assert!(!hashes(&output).is_empty());
}

#[test]
fn exclude_targets_query_filters_manual_targets() {
    let workspace = copy_workspace("distance_metrics");
    let build = workspace.path().join("BUILD");
    fs::OpenOptions::new()
        .append(true)
        .open(&build)
        .unwrap()
        .write_all(
            b"\nsh_library(name = \"manual_only\", srcs = [\"lib.sh\"], tags = [\"manual\"])\n",
        )
        .unwrap();
    let plain = workspace.path().join("plain.json");
    let filtered = workspace.path().join("filtered.json");
    assert!(generate(workspace.path(), &plain, &[]).status.success());
    assert!(hashes(&plain).contains_key("//:manual_only"));
    assert!(generate(
        workspace.path(),
        &filtered,
        &[
            "--excludeTargetsQuery",
            r#"attr("tags", "[\[ ]manual[,\]]", //...)"#,
        ],
    )
    .status
    .success());
    let hashes = hashes(&filtered);
    assert!(!hashes.contains_key("//:manual_only"));
    assert!(hashes.contains_key("//:lib"));
}

#[test]
fn target_distance_metrics() {
    let workspace = copy_workspace("distance_metrics");
    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let deps = output.path().join("deps.json");
    let result = output.path().join("impacted.json");
    assert!(generate(workspace.path(), &from, &["--includeTargetType"])
        .status
        .success());
    fs::OpenOptions::new()
        .append(true)
        .open(workspace.path().join("A/one.sh"))
        .unwrap()
        .write_all(b"foo")
        .unwrap();
    assert!(generate(
        workspace.path(),
        &to,
        &["--includeTargetType", "-d", deps.to_str().unwrap(),],
    )
    .status
    .success());
    assert!(impacted(
        workspace.path(),
        &from,
        &to,
        &result,
        &["-d", deps.to_str().unwrap(), "-tt", "Rule,GeneratedFile"],
    )
    .status
    .success());
    let actual: Vec<Value> = serde_json::from_slice(&fs::read(result).unwrap()).unwrap();
    let distances = actual
        .iter()
        .map(|entry| {
            (
                entry["label"].as_str().unwrap(),
                entry["targetDistance"].as_u64().unwrap(),
                entry["packageDistance"].as_u64().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    for expected in [
        ("//A:one", 0, 0),
        ("//A:gen_two", 1, 0),
        ("//A:two.sh", 2, 0),
        ("//A:two", 3, 0),
        ("//A:three", 4, 0),
        ("//:lib", 5, 1),
    ] {
        assert!(
            distances.contains(&expected),
            "missing {expected:?}: {distances:?}"
        );
    }
}

#[test]
fn cquery_failing_analysis_targets() {
    let workspace = copy_workspace("cquery_failing_target");
    let output = workspace.path().join("hashes.json");
    assert!(generate(workspace.path(), &output, &[]).status.success());
    assert!(!generate(workspace.path(), &output, &["--useCquery"])
        .status
        .success());
    assert!(
        !generate(workspace.path(), &output, &["--useCquery", "--keep_going"],)
            .status
            .success()
    );
    assert!(generate(
        workspace.path(),
        &output,
        &[
            "--useCquery",
            "--cqueryExpression",
            "deps(//:normal_target) + deps(//:dependent_target)",
        ],
    )
    .status
    .success());
    let text = fs::read_to_string(output).unwrap();
    assert!(text.contains("normal_target"));
    assert!(text.contains("dependent_target"));
    assert!(!text.contains("failing_analysis_target"));
}

#[test]
fn keep_going_partial_graph_behavior() {
    let workspace = copy_workspace("keep_going_repo_failure");
    let default = workspace.path().join("default.json");
    let partial = workspace.path().join("partial.json");
    assert!(!generate(workspace.path(), &default, &[]).status.success());
    assert!(generate(workspace.path(), &partial, &["--keep_going"])
        .status
        .success());
    let text = fs::read_to_string(partial).unwrap();
    assert!(text.contains("//good:good"));
    assert!(!text.contains("//bad:"));
}

#[test]
fn serve_end_to_end() {
    let workspace = copy_workspace("distance_metrics");
    let from = init_git(workspace.path());
    fs::write(workspace.path().join("lib.sh"), "echo changed\n").unwrap();
    let to = commit(workspace.path(), "change");
    let cache = tempfile::tempdir().unwrap();
    let server = ServerProcess::start(workspace.path(), cache.path(), &["--trackDeps"]);
    let (status, body) = server
        .get(&format!("/impacted_targets?from={from}&to={to}"))
        .unwrap();
    assert_eq!(status, 200);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert!(body["impactedTargets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|label| label == "//:lib"));
    let (status, body) = server
        .get(&format!(
            "/impacted_targets_with_distances?from={from}&to={to}"
        ))
        .unwrap();
    assert_eq!(status, 200);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert!(body["impactedTargets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|target| target["label"] == "//:lib" && target["targetDistance"] == 0));
    let (status, body) = server
        .get(&format!(
            "/dependency_edges?from={from}&to={to}&profile=true"
        ))
        .unwrap();
    assert_eq!(status, 200);
    let graph: Value = serde_json::from_str(&body).unwrap();
    assert!(
        graph.get("from").is_none() && graph.get("impactedTargets").is_none(),
        "body must be the generate-hashes graph, got {graph}"
    );
    assert!(
        graph
            .as_object()
            .unwrap()
            .keys()
            .any(|label| label.trim_start_matches("@@") == "//:lib" || label.contains(":lib")),
        "expected //:lib in dep-edges graph, got {graph}"
    );
}

/// `--partialGraph`: a source edit is answered without a `bazel query`, with the same impacted
/// set and distances as the default mode, and the derived cache entry is byte-identical to the
/// one a full generation in the same mode writes; a `BUILD` edit falls back to a full generation.
#[test]
fn serve_partial_graph_derives_source_edits() {
    let workspace = copy_workspace("distance_metrics");
    let from = init_git(workspace.path());
    fs::write(workspace.path().join("lib.sh"), "echo changed\n").unwrap();
    let to = commit(workspace.path(), "source edit");
    let build = workspace.path().join("BUILD");
    let original = fs::read_to_string(&build).unwrap();
    fs::write(&build, format!("{original}\n# touched\n")).unwrap();
    let rebuilt = commit(workspace.path(), "build edit");
    let to_tree = git(workspace.path(), &["rev-parse", &format!("{to}^{{tree}}")]);

    let query = |server: &ServerProcess, path: &str| -> Value {
        let (status, body) = server.get(path).unwrap();
        assert_eq!(status, 200, "{path}: {body}");
        serde_json::from_str(&body).unwrap()
    };
    let (expected, expected_distances) = {
        let cache = tempfile::tempdir().unwrap();
        let server = ServerProcess::start(workspace.path(), cache.path(), &["--trackDeps"]);
        (
            query(&server, &format!("/impacted_targets?from={from}&to={to}")),
            query(
                &server,
                &format!("/impacted_targets_with_distances?from={from}&to={to}"),
            ),
        )
    };
    assert!(expected["impactedTargets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|label| label == "//:lib"));

    let partial_cache = tempfile::tempdir().unwrap();
    {
        let server = ServerProcess::start(
            workspace.path(),
            partial_cache.path(),
            &["--trackDeps", "--partialGraph"],
        );
        let body = query(
            &server,
            &format!("/impacted_targets?from={from}&to={to}&profile=true"),
        );
        assert_eq!(body["impactedTargets"], expected["impactedTargets"]);
        let generation = &body["profile"]["hashRetrievals"][1]["generation"];
        assert_eq!(generation["mode"], "derived", "{body}");
        assert_eq!(generation["derivedFrom"], from);
        assert_eq!(generation["bazelQueryMillis"], 0);
        assert_eq!(generation["changedSources"], 1);
        let distances = query(
            &server,
            &format!("/impacted_targets_with_distances?from={from}&to={to}"),
        );
        assert_eq!(
            distances["impactedTargets"],
            expected_distances["impactedTargets"]
        );

        let body = query(
            &server,
            &format!("/impacted_targets?from={to}&to={rebuilt}&profile=true"),
        );
        let generation = &body["profile"]["hashRetrievals"][1]["generation"];
        assert_eq!(generation["mode"], "full", "{body}");
        assert!(generation["partialFallbackReason"]
            .as_str()
            .unwrap()
            .contains("graph-defining file BUILD"));
        let metrics = query(&server, "/metrics");
        assert_eq!(metrics["partialGraph"]["enabled"], true);
        assert_eq!(metrics["partialGraph"]["derived"], 1);
    }

    // A fresh partial-graph server generating `to` from scratch must write the same bytes.
    let verify_cache = tempfile::tempdir().unwrap();
    {
        let server = ServerProcess::start(
            workspace.path(),
            verify_cache.path(),
            &["--trackDeps", "--partialGraph"],
        );
        let body = query(
            &server,
            &format!("/impacted_targets?from={to}&to={to}&profile=true"),
        );
        assert_eq!(
            body["profile"]["hashRetrievals"][0]["generation"]["mode"],
            "full"
        );
    }
    let entry = |cache: &Path| {
        let path = fs::read_dir(cache)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&to_tree) && name.ends_with(".json"))
            })
            .unwrap_or_else(|| panic!("no entry for tree {to_tree} in {}", cache.display()));
        fs::read(path).unwrap()
    };
    assert_eq!(
        entry(partial_cache.path()),
        entry(verify_cache.path()),
        "derived and fully generated entries differ"
    );
}

/// `generate-hashes --partialGraph`: a source edit is hashed from the `--baseHashes` entry with
/// no `bazel query`, as the working tree stands (committed or not), with the same impacted set
/// as the default mode and a file byte-identical to the one a full generation in the mode
/// writes; the two modes' files are refused side by side; a `BUILD` edit falls back to a full
/// generation.
#[test]
fn generate_hashes_partial_graph_derives_from_base_hashes() {
    let workspace = copy_workspace("distance_metrics");
    init_git(workspace.path());
    let out = tempfile::tempdir().unwrap();
    let path = |name: &str| out.path().join(name);
    let generated = |output: &Path, extra: &[&str]| -> String {
        let result = generate(workspace.path(), output, extra);
        assert!(result.status.success(), "generate-hashes {extra:?}");
        String::from_utf8_lossy(&result.stderr).into_owned()
    };
    let base_default = path("base_default.json");
    let base_partial = path("base_partial.json");
    let base_deps = path("base_deps.json");
    generated(&base_default, &[]);
    let log = generated(
        &base_partial,
        &["--partialGraph", "-d", &base_deps.display().to_string()],
    );
    assert!(log.contains("full generation"), "{log}");
    assert!(log.contains("no --baseHashes"), "{log}");
    let base_partial_arg = base_partial.display().to_string();

    // An uncommitted source edit: derived from the base, no query.
    fs::write(workspace.path().join("lib.sh"), "echo changed\n").unwrap();
    let to_default = path("to_default.json");
    let to_derived = path("to_derived.json");
    let to_full = path("to_full.json");
    let derived_deps = path("derived_deps.json");
    let full_deps = path("full_deps.json");
    generated(&to_default, &[]);
    let log = generated(
        &to_derived,
        &[
            "--partialGraph",
            "--baseHashes",
            &base_partial_arg,
            "-d",
            &derived_deps.display().to_string(),
        ],
    );
    assert!(log.contains("partial graph: derived"), "{log}");
    assert!(!log.contains("bazel query"), "{log}");
    generated(
        &to_full,
        &["--partialGraph", "-d", &full_deps.display().to_string()],
    );
    assert_eq!(dep_edges(&derived_deps), dep_edges(&full_deps));
    assert!(dep_edges(&derived_deps).contains_key("//:lib"));
    assert_eq!(
        fs::read(&to_derived).unwrap(),
        fs::read(&to_full).unwrap(),
        "derived and fully generated files differ"
    );
    let tree: Value = serde_json::from_slice(&fs::read(&to_full).unwrap()).unwrap();

    // The same impacted set as the default mode, including the edited script's dependents.
    let expected = path("expected.txt");
    let actual = path("actual.txt");
    assert!(
        impacted(workspace.path(), &base_default, &to_default, &expected, &[])
            .status
            .success()
    );
    assert!(
        impacted(workspace.path(), &base_partial, &to_derived, &actual, &[])
            .status
            .success()
    );
    let expected = filter_internal(read_lines(&expected));
    assert!(expected.contains("//:lib"), "{expected:?}");
    assert_eq!(filter_internal(read_lines(&actual)), expected);
    let mixed = impacted(
        workspace.path(),
        &base_default,
        &to_derived,
        &path("mixed.txt"),
        &[],
    );
    assert!(!mixed.status.success());
    assert!(String::from_utf8_lossy(&mixed.stderr).contains("same mode"));

    // Committing the edit leaves the tree, and so the file, unchanged: the file written from the
    // dirty working tree already recorded the tree the commit now points at.
    let to = commit(workspace.path(), "source edit");
    assert_eq!(
        tree["metadata"]["tree"],
        git(workspace.path(), &["rev-parse", &format!("{to}^{{tree}}")]).as_str(),
        "the file records the working tree's tree id"
    );
    let committed = path("committed.json");
    generated(
        &committed,
        &[
            "--partialGraph",
            "--baseHashes",
            &base_partial_arg,
            "-d",
            &path("committed_deps.json").display().to_string(),
        ],
    );
    assert_eq!(fs::read(&committed).unwrap(), fs::read(&to_full).unwrap());

    // A BUILD edit cannot be derived: a full generation runs and still records the tree.
    let build = workspace.path().join("BUILD");
    let original = fs::read_to_string(&build).unwrap();
    fs::write(&build, format!("{original}\n# touched\n")).unwrap();
    let rebuilt = path("rebuilt.json");
    let log = generated(
        &rebuilt,
        &[
            "--partialGraph",
            "--baseHashes",
            &to_derived.display().to_string(),
        ],
    );
    assert!(log.contains("full generation"), "{log}");
    assert!(log.contains("graph-defining file BUILD"), "{log}");
    let rebuilt: Value = serde_json::from_slice(&fs::read(&rebuilt).unwrap()).unwrap();
    assert_eq!(rebuilt["metadata"]["tree"].as_str().unwrap().len(), 40);
    assert_ne!(rebuilt["metadata"]["tree"], tree["metadata"]["tree"]);
    assert!(rebuilt["metadata"]["partialGraph"].is_object());
}

/// `--partialGraph` must not derive across an edit to a file a repository rule reads while
/// loading: `dep_repo/hello` is a regular source file to git, but its content sits in the
/// `//external:dep_repo` node's definition, so the entry records the local repository's root
/// and a change under it forces a full generation, which still reports `//:consume`.
#[test]
fn serve_partial_graph_rebuilds_for_local_repository_edits() {
    let workspace = copy_workspace("bzlmod_local_repo");
    let from = init_git(workspace.path());
    fs::write(workspace.path().join("dep_repo/hello"), "changed\n").unwrap();
    let to = commit(workspace.path(), "local repository edit");
    let cache = tempfile::tempdir().unwrap();
    let server = ServerProcess::start(workspace.path(), cache.path(), &["--partialGraph"]);
    let (status, body) = server
        .get(&format!(
            "/impacted_targets?from={from}&to={to}&profile=true"
        ))
        .unwrap();
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    let generation = &body["profile"]["hashRetrievals"][1]["generation"];
    assert_eq!(generation["mode"], "full", "{body}");
    let reason = generation["partialFallbackReason"].as_str().unwrap();
    assert!(
        reason.contains("read while loading") && reason.contains("dep_repo"),
        "{reason}"
    );
    assert!(
        body["impactedTargets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|label| label == "//:consume"),
        "{body}"
    );
}

#[test]
fn serve_macro_digest_is_revision_scoped() {
    let workspace = copy_workspace("serve_bzl_cache");
    let from = init_git(workspace.path());
    fs::OpenOptions::new()
        .append(true)
        .open(workspace.path().join("defs.bzl"))
        .unwrap()
        .write_all(b"\n# second revision\n")
        .unwrap();
    let to = commit(workspace.path(), "change macro");
    let cache = tempfile::tempdir().unwrap();
    let server = ServerProcess::start(workspace.path(), cache.path(), &[]);
    let (_, body) = server
        .get(&format!("/impacted_targets?from={from}&to={to}"))
        .unwrap();
    let body: Value = serde_json::from_str(&body).unwrap();
    assert!(body["impactedTargets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|label| label.as_str().unwrap().trim_start_matches("@@") == "//:generated"));
}

use std::io::Write;
