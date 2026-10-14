use crate::support::*;
use std::collections::BTreeSet;
use std::fs;

fn diff_workspaces(
    first: &std::path::Path,
    second: &std::path::Path,
    generate_args: &[&str],
) -> BTreeSet<String> {
    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let result = output.path().join("impacted.txt");
    assert!(generate(first, &from, generate_args).status.success());
    assert!(generate(second, &to, generate_args).status.success());
    assert!(impacted(second, &from, &to, &result, &[]).status.success());
    read_lines(&result)
}

#[test]
fn undeclared_workspace_read_is_not_impacted_without_opt_in() {
    let first = copy_workspace("always_affected_external");
    let second = copy_workspace("always_affected_external");
    fs::write(second.path().join("scanned_data.txt"), "v2\n").unwrap();
    fs::write(second.path().join("declared_src.txt"), "goodbye\n").unwrap();
    let impacted = diff_workspaces(first.path(), second.path(), &[]);
    assert!(impacted.contains("//:hermetic"));
    assert!(!impacted.contains("//:scanner"));
}

#[test]
fn always_affected_tag_marks_non_hermetic_target() {
    let first = copy_workspace("always_affected_external");
    let second = copy_workspace("always_affected_external");
    let impacted = diff_workspaces(
        first.path(),
        second.path(),
        &["--alwaysAffectedTags", "external"],
    );
    assert!(impacted.contains("//:scanner"));
}

#[test]
fn package_group_change_impacts_consumers() {
    let first = copy_workspace("package_group_dropped");
    let second = copy_workspace("package_group_dropped");
    edit(&second.path().join("lib/defs.bzl"), |text| {
        text.replace("ALLOWED = [\"//consumer\"]", "ALLOWED = []")
    });
    assert_eq!(
        diff_workspaces(first.path(), second.path(), &[]),
        BTreeSet::from([
            "//consumer:use_thing".to_owned(),
            "//consumer:use_thing.txt".to_owned(),
            "//lib:consumers".to_owned(),
            "//lib:thing".to_owned(),
            "//lib:thing.txt".to_owned(),
        ])
    );
}

#[test]
fn module_comment_and_lock_reformat_are_noops() {
    let first = copy_workspace("module_bazel_comment");
    let commented = copy_workspace("module_bazel_comment");
    let module = commented.path().join("MODULE.bazel");
    fs::write(
        &module,
        format!("# no-op\n{}", fs::read_to_string(&module).unwrap()),
    )
    .unwrap();
    assert!(diff_workspaces(first.path(), commented.path(), &[]).is_empty());

    let reformatted = copy_workspace("module_bazel_comment");
    let lock = reformatted.path().join("MODULE.bazel.lock");
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&lock).unwrap()).unwrap();
    fs::write(&lock, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(diff_workspaces(first.path(), reformatted.path(), &[]).is_empty());
}

#[test]
fn hashes_are_hermetic_across_workspace_paths() {
    let first = copy_workspace("module_bazel_comment");
    let second = copy_workspace("module_bazel_comment");
    let first_output = first.path().join("hashes.json");
    let second_output = second.path().join("hashes.json");
    assert!(generate(first.path(), &first_output, &[]).status.success());
    assert!(generate(second.path(), &second_output, &[])
        .status
        .success());
    assert_eq!(hashes(&first_output), hashes(&second_output));
}

#[test]
fn local_path_override_works_for_query_and_cquery() {
    let workspace = copy_workspace("bzlmod_local_repo");
    let query = workspace.path().join("query.json");
    let cquery = workspace.path().join("cquery.json");
    assert!(generate(workspace.path(), &query, &[]).status.success());
    assert!(hashes(&query)
        .keys()
        .any(|label| label.ends_with("//:consume") || label == "//:consume"));
    assert!(generate(workspace.path(), &cquery, &["--useCquery"])
        .status
        .success());
    let cquery_text = fs::read_to_string(cquery).unwrap();
    assert!(cquery_text.contains("consume"));
    assert!(cquery_text.contains("dep_repo"));
}

#[test]
fn macro_bzl_change_impacts_callers() {
    let first = copy_workspace("macro_invalidation");
    let second = copy_workspace("macro_invalidation");
    edit(&second.path().join("miniature.bzl"), |text| {
        text.replace(
            "def miniature(name, src, **kwargs):",
            "def miniature(name, src, **kwargs):\n    print(\"miniature: \" + name)",
        )
    });
    let impacted = diff_workspaces(first.path(), second.path(), &[]);
    assert!(impacted
        .iter()
        .any(|label| label.ends_with("//:logo_miniature")));
}

#[test]
fn unrelated_macro_does_not_overinvalidate() {
    let first = copy_workspace("bzl_seed_overtrigger");
    let second = copy_workspace("bzl_seed_overtrigger");
    fs::create_dir_all(second.path().join("macros")).unwrap();
    fs::write(
        second.path().join("macros/defs.bzl"),
        "def my_macro(name, **kwargs):\n    native.genrule(name=name, outs=[name + \".txt\"], cmd=\"echo hi > $@\", **kwargs)\n",
    )
    .unwrap();
    fs::write(
        second.path().join("macros/BUILD"),
        "load(\"//macros:defs.bzl\", \"my_macro\")\n",
    )
    .unwrap();
    let impacted = diff_workspaces(first.path(), second.path(), &[]);
    assert!(!impacted.contains("//pkg:a"));
    assert!(!impacted.contains("//pkg:b"));
}

#[test]
fn rule_bzl_change_is_scoped_to_consumer() {
    let first = copy_workspace("rule_bzl_overtrigger");
    let second = copy_workspace("rule_bzl_overtrigger");
    edit(&second.path().join("defs/thing.bzl"), |text| {
        text.replace(
            "ctx.actions.write(out, \"thing\")",
            "ctx.actions.write(out, \"thing\")  # edited",
        )
    });
    let impacted = diff_workspaces(first.path(), second.path(), &[]);
    assert!(impacted.iter().any(|label| label.ends_with("//app:t")));
    assert!(!impacted.iter().any(|label| {
        label.ends_with("//app:native_gen")
            || label.ends_with("//app:g.txt")
            || label.ends_with("//app:fg")
            || label.ends_with("//app:data.txt")
    }));
}

fn cross_file(edit_path: &str, before: &str, after: &str) {
    let first = copy_workspace("cross_file_macro_rule");
    let second = copy_workspace("cross_file_macro_rule");
    edit(&second.path().join(edit_path), |text| {
        text.replace(before, after)
    });
    let impacted = diff_workspaces(first.path(), second.path(), &[]);
    assert!(impacted.iter().any(|label| label.ends_with("//app:s")));
    assert!(!impacted
        .iter()
        .any(|label| { label.ends_with("//app:unrelated_gen") || label.ends_with("//app:u.txt") }));
}

#[test]
fn cross_file_macro_change_is_scoped() {
    cross_file(
        "defs/macro.bzl",
        "def split(name, **kwargs):",
        "def split(name, **kwargs):\n    print(\"split: \" + name)",
    );
}

#[test]
fn cross_file_rule_change_is_scoped() {
    cross_file(
        "defs/rule.bzl",
        "ctx.actions.write(out, \"split\")",
        "ctx.actions.write(out, \"split\")  # edited",
    );
}

#[test]
fn wrapped_external_repo_change_reaches_main_consumer() {
    let first = copy_workspace("wrapped_external_repo");
    let second = copy_workspace("wrapped_external_repo");
    fs::write(second.path().join("inner_repo/data.txt"), "version two\n").unwrap();
    let impacted = diff_workspaces(
        first.path(),
        second.path(),
        &["--fineGrainedHashExternalRepos", "@inner_repo"],
    );
    assert!(
        impacted
            .iter()
            .any(|label| label == "//:consumer" || label == "@@//:consumer"),
        "{impacted:?}"
    );
}

/// Regression for https://github.com/Tinder/bazel-diff/issues/510: like the
/// #197 case above, but the wrapper module (`middle`) is only declared by
/// `facade`, under a private apparent name, so it is not visible from the root.
#[test]
fn hidden_wrapper_external_repo_change_reaches_main_consumer() {
    let first = copy_workspace("hidden_wrapper_external_repo");
    let second = copy_workspace("hidden_wrapper_external_repo");
    fs::write(second.path().join("leaf/value.txt"), "after\n").unwrap();
    let impacted = diff_workspaces(
        first.path(),
        second.path(),
        &["--fineGrainedHashExternalRepos", "@leaf"],
    );
    assert!(
        impacted
            .iter()
            .any(|label| label == "//:consumer" || label == "@@//:consumer"),
        "{impacted:?}"
    );
    assert!(!impacted.contains("//:control"), "{impacted:?}");
}

/// Control for the issue 510 regression: naming every repository in the chain
/// already propagates the change, so the fixture's edges are real and the gap
/// is in discovering `@@middle+` from `@leaf` alone.
#[test]
fn hidden_wrapper_external_repo_change_propagates_with_explicit_chain() {
    let first = copy_workspace("hidden_wrapper_external_repo");
    let second = copy_workspace("hidden_wrapper_external_repo");
    fs::write(second.path().join("leaf/value.txt"), "after\n").unwrap();
    let impacted = diff_workspaces(
        first.path(),
        second.path(),
        &["--fineGrainedHashExternalRepos", "@leaf,@facade,@@middle+"],
    );
    assert!(
        impacted
            .iter()
            .any(|label| label == "//:consumer" || label == "@@//:consumer"),
        "{impacted:?}"
    );
    assert!(!impacted.contains("//:control"), "{impacted:?}");
}

#[test]
fn fine_grained_external_hashes_are_hermetic() {
    let first = copy_workspace("wrapped_external_repo");
    let second = copy_workspace("wrapped_external_repo");
    let first_output = first.path().join("hashes.json");
    let second_output = second.path().join("hashes.json");
    let args = ["--fineGrainedHashExternalRepos", "@inner_repo"];
    assert!(generate(first.path(), &first_output, &args)
        .status
        .success());
    assert!(generate(second.path(), &second_output, &args)
        .status
        .success());
    let first = hashes(&first_output)
        .into_iter()
        .filter(|(label, _)| !label.starts_with("//external:"))
        .collect::<BTreeMap<_, _>>();
    let second = hashes(&second_output)
        .into_iter()
        .filter(|(label, _)| !label.starts_with("//external:"))
        .collect::<BTreeMap<_, _>>();
    assert!(first
        .keys()
        .any(|label| label.contains("inner_repo") && label.contains("data.txt")));
    assert_eq!(first, second);
}

#[test]
fn go_mod_update_impacts_go_targets() {
    let first = copy_workspace("go_mod_change");
    let second = copy_workspace("go_mod_change");
    edit(&second.path().join("go.mod"), |text| {
        text.replace(
            "github.com/pkg/errors v0.9.1",
            "github.com/pkg/errors v0.9.0",
        )
    });
    let impacted = diff_workspaces(first.path(), second.path(), &[]);
    assert!(impacted
        .iter()
        .any(|label| label.ends_with("//:lib") || label == "//:lib"));
    assert!(impacted
        .iter()
        .any(|label| label.ends_with("//:lib_test") || label == "//:lib_test"));
}

#[test]
fn external_go_dependencies_appear_in_hashes() {
    let workspace = copy_workspace("go_mod_change");
    let output = workspace.path().join("hashes.json");
    assert!(generate(
        workspace.path(),
        &output,
        &["--fineGrainedHashExternalRepos", "@com_github_pkg_errors"]
    )
    .status
    .success());
    assert!(hashes(&output)
        .keys()
        .any(|label| label.contains("com_github_pkg_errors")));
}

use std::collections::BTreeMap;

/// Reproducer for <https://github.com/Tinder/bazel-diff/issues/512>.
///
/// `deps(//:app)` under cquery contains `//:app` twice: in the default
/// configuration, where it depends on `//:helper`, and in the bootstrap
/// configuration that `//:helper` transitions it into, where it has no deps.
/// Hashing by label keeps only one instance, and every edge into `//:app`
/// resolves to it. When the default instance wins, `//:helper`'s edge to the
/// bootstrap `//:app` closes a false cycle and generate-hashes fails. When the
/// bootstrap instance wins, a change to `input.txt` reaches `//:helper` but
/// never `//:app`, and the dep-edge output for `//:app` omits `//:helper`.
#[test]
fn cquery_keeps_dependencies_of_every_configured_instance() {
    let first = copy_workspace("cquery_transition_dedup");
    let second = copy_workspace("cquery_transition_dedup");
    fs::write(second.path().join("input.txt"), "after\n").unwrap();

    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let edges = output.path().join("edges.json");
    let result = output.path().join("impacted.txt");
    let cquery = ["--useCquery", "--cqueryExpression", "deps(//:app)"];

    let mut with_edges = cquery.to_vec();
    with_edges.extend(["--depEdgesFile", edges.to_str().unwrap()]);
    for (workspace, output, args) in [
        (first.path(), &from, &with_edges),
        (second.path(), &to, &cquery.to_vec()),
    ] {
        let generated = generate(workspace, output, args);
        assert!(
            generated.status.success(),
            "generate-hashes --useCquery failed (a false cycle means the two //:app \
             instances were collapsed by label): {}",
            String::from_utf8_lossy(&generated.stderr)
        );
    }
    assert!(impacted(second.path(), &from, &to, &result, &[])
        .status
        .success());

    let dep_edges: BTreeMap<String, Vec<String>> =
        serde_json::from_slice(&fs::read(&edges).unwrap()).unwrap();
    let app_edges = dep_edges
        .iter()
        .find(|(label, _)| label.ends_with("//:app"))
        .map(|(_, deps)| deps.clone())
        .expect("//:app has dep edges");
    assert!(
        app_edges.iter().any(|dep| dep.ends_with("//:helper")),
        "//:app lost its default-configuration edge to //:helper: {app_edges:?}"
    );

    let impacted = read_lines(&result);
    assert!(impacted.iter().any(|label| label.ends_with("//:helper")));
    assert!(
        impacted.iter().any(|label| label.ends_with("//:app")),
        "//:app should be impacted through //:helper, got {impacted:?}"
    );
}
