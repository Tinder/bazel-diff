use crate::support::*;
use serde_json::Value;
use std::fs;

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
