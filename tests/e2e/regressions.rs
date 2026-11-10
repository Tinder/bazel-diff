use crate::support::*;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

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
    let ordinary = diff_workspaces(first.path(), second.path(), &[]);
    assert_eq!(
        ordinary,
        BTreeSet::from([
            "//consumer:use_thing".to_owned(),
            "//consumer:use_thing.txt".to_owned(),
            "//lib:allowed".to_owned(),
            "//lib:consumers".to_owned(),
            "//lib:thing".to_owned(),
            "//lib:thing.txt".to_owned(),
        ])
    );
    // Keep both revisions analyzable for cquery while changing only the package group.
    edit(&second.path().join("lib/defs.bzl"), |text| {
        text.replace(
            "ALLOWED = []",
            "ALLOWED = [\"//consumer\", \"//additional\"]",
        )
    });
    let impacted = diff_workspaces(first.path(), second.path(), &["--useCquery"]);
    let local_labels: BTreeSet<_> = impacted
        .iter()
        .map(|label| {
            label
                .strip_prefix("@@")
                .or_else(|| label.strip_prefix('@'))
                .unwrap_or(label)
        })
        .collect();
    assert_eq!(local_labels, ordinary.iter().map(String::as_str).collect());
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

/// The #510 chain under `--useCquery`, where every label is canonical: `@@facade+//:export`
/// names `@@middle+//:wrapped`, which names `@@leaf+//:value`. `leaf` is root-visible, so its
/// node is `//external:leaf` and the canonical `leaf+` has to be translated to reach it; `middle`
/// is hidden, so its node keeps the canonical name `//external:middle+`. The impacted set alone
/// cannot tell the two apart (the `middle` node also depends on the `leaf` node), so the edges
/// are asserted directly.
#[test]
fn hidden_wrapper_external_repo_change_reaches_main_consumer_cquery() {
    let first = copy_workspace("hidden_wrapper_external_repo");
    let second = copy_workspace("hidden_wrapper_external_repo");
    fs::write(second.path().join("leaf/value.txt"), "after\n").unwrap();
    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let edges_file = output.path().join("edges.json");
    let result = output.path().join("impacted.txt");
    let args = ["--useCquery"];
    assert!(generate(first.path(), &from, &args).status.success());
    assert!(generate(
        second.path(),
        &to,
        &[
            "--useCquery",
            "--depEdgesFile",
            edges_file.to_str().unwrap()
        ],
    )
    .status
    .success());
    assert!(impacted(second.path(), &from, &to, &result, &[])
        .status
        .success());
    let edges = dep_edges(&edges_file);
    assert!(
        edges["@@middle+//:wrapped"].contains("//external:leaf"),
        "{:?}",
        edges["@@middle+//:wrapped"]
    );
    assert!(
        edges["@@facade+//:export"].contains("//external:middle+"),
        "{:?}",
        edges["@@facade+//:export"]
    );
    let impacted = read_lines(&result);
    assert!(
        impacted
            .iter()
            .any(|label| label == "//:consumer" || label == "@@//:consumer"),
        "{impacted:?}"
    );
    assert!(
        !impacted.iter().any(|label| label.ends_with(":control")),
        "{impacted:?}"
    );
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

/// Regression for https://github.com/Tinder/bazel-diff/issues/522: a module patched through
/// `single_version_override(patches = [...])`, where the patch rewrites a plain data file and no
/// `.bzl`. The patch is hashed into the module's `//external:dep` node
/// (`_bazel_diff_local_inputs_hash`), and `//:consumer` reaches that node through its input
/// label: `@dep//:data` under query, `@@dep+//:data` under cquery. The canonical form used to
/// miss the node, which is named by the apparent name `dep`, so the reporter's cquery run saw no
/// impacted targets for a patch edit while plain query did. The query-mode run is the control
/// that proves the fixture carries the signal at all.
fn single_version_override_patch_change(generate_args: &[&str]) {
    if !supports_mod_show_repo() {
        return;
    }
    let first = copy_workspace("single_version_override_patch");
    let second = copy_workspace("single_version_override_patch");
    write_local_registry(first.path());
    write_local_registry(second.path());
    edit(&second.path().join("dep.patch"), |text| {
        text.replace("patched v1", "patched v2")
    });
    let impacted = diff_workspaces(first.path(), second.path(), generate_args);
    assert!(
        impacted
            .iter()
            .any(|label| label == "//:consumer" || label == "@@//:consumer"),
        "{generate_args:?}: {impacted:?}"
    );
    assert!(
        !impacted.iter().any(|label| label.ends_with(":control")),
        "{generate_args:?}: {impacted:?}"
    );
}

#[test]
fn single_version_override_patch_change_reaches_consumer_query() {
    single_version_override_patch_change(&[]);
}

/// The reporter's exact invocation: `--useCquery` with a `--cqueryExpression` that excludes
/// manual targets, on a Bzlmod-only workspace where the `//external:all-targets` query is
/// dropped with the "//external is not available" warning.
#[test]
fn single_version_override_patch_change_reaches_consumer_cquery() {
    single_version_override_patch_change(&[
        "--useCquery",
        "--cqueryExpression",
        "deps((//...) except attr(tags,manual,//...))",
    ]);
}

/// A MODULE version change remains an independent obligation for a reader first requested at
/// the head, even when its configured hash was already present and unchanged at the base.
#[test]
fn configured_roots_preserve_module_obligations() {
    if !supports_mod_show_repo() {
        return;
    }
    struct Snapshot {
        workspace: TestWorkspace,
        hashes: PathBuf,
        edges: PathBuf,
        ordinary: PathBuf,
        ordinary_edges: PathBuf,
        data: Value,
        product: Vec<u8>,
        overlap_product: Vec<u8>,
    }
    let output = tempfile::tempdir().unwrap();
    let capture = |name: &str, version: &str, change_source: bool, head: bool| {
        let workspace = copy_workspace("single_version_override_patch");
        if version != "1.0" {
            for path in ["MODULE.bazel", "dep_src/MODULE.bazel"] {
                edit(&workspace.path().join(path), |text| {
                    text.replace("version = \"1.0\"", &format!("version = \"{version}\""))
                });
            }
        }
        // Select dep transitively so ordinary query emits its canonical base-repository label,
        // which is the existing MODULE overlay's repository inventory.
        edit(&workspace.path().join("MODULE.bazel"), |text| {
            text.replace(
                &format!("bazel_dep(name = \"dep\", version = \"{version}\")"),
                "bazel_dep(name = \"carrier\", version = \"1.0\")\nlocal_path_override(module_name = \"carrier\", path = \"carrier\")",
            )
        });
        if version != "1.0" {
            edit(&workspace.path().join("carrier/MODULE.bazel"), |text| {
                text.replace(
                    "bazel_dep(name = \"dep\", version = \"1.0\")",
                    &format!("bazel_dep(name = \"dep\", version = \"{version}\")"),
                )
            });
        }
        edit(&workspace.path().join("BUILD.bazel"), |text| {
            text.replace("\"@dep//:data\"", "\"@@dep+//:data\"")
        });
        if change_source {
            fs::write(workspace.path().join("control.txt"), "changed\n").unwrap();
        }
        write_local_registry_version(workspace.path(), version);
        let built = std::process::Command::new(bazel())
            .current_dir(workspace.path())
            .args(["build", "//:module_wrapper", "//:module_overlap"])
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let product = fs::read(workspace.path().join("bazel-bin/module_product.out")).unwrap();
        let overlap_product =
            fs::read(workspace.path().join("bazel-bin/module_overlap.out")).unwrap();
        let hashes = output.path().join(format!("{name}.json"));
        let edges = output.path().join(format!("{name}-edges.json"));
        let roots = if head {
            "set(//:module_wrapper //:module_overlap //:module_product)"
        } else {
            "set(//:module_wrapper //:module_overlap)"
        };
        let edges_arg = edges.display().to_string();
        let mut args = vec![
            "--useCquery",
            "--cqueryRootExpression",
            roots,
            "--fineGrainedHashExternalRepos",
            "@@dep+",
            "--includeTargetType",
        ];
        if head {
            args.extend(["-d", &edges_arg]);
        }
        let generated = generate(workspace.path(), &hashes, &args);
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let data = serde_json::from_slice(&fs::read(&hashes).unwrap()).unwrap();
        let ordinary = output.path().join(format!("{name}-ordinary.json"));
        let ordinary_edges = output.path().join(format!("{name}-ordinary-edges.json"));
        let ordinary_edges_arg = ordinary_edges.display().to_string();
        let mut args = vec![
            "--fineGrainedHashExternalRepos",
            "@@dep+",
            "--excludeTargetsQuery=set(//:module_product //:module_product.out)",
            "--includeTargetType",
        ];
        if head {
            args.extend(["-d", &ordinary_edges_arg]);
        }
        let generated = generate(workspace.path(), &ordinary, &args);
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        Snapshot {
            workspace,
            hashes,
            edges,
            ordinary,
            ordinary_edges,
            data,
            product,
            overlap_product,
        }
    };
    let base = capture("base", "1.0", false, false);
    let module_only = capture("module-only", "2.0", false, true);
    let changed = capture("module-and-source", "2.0", true, true);
    assert_eq!(base.product, module_only.product);
    assert_eq!(base.product, changed.product);
    assert_eq!(base.overlap_product, module_only.overlap_product);
    assert_ne!(base.overlap_product, changed.overlap_product);
    assert!(base.data["metadata"]["moduleGraphJson"].is_string());
    assert!(module_only.data["metadata"]["moduleGraphJson"].is_string());
    assert_ne!(
        base.data["metadata"]["moduleGraphJson"],
        module_only.data["metadata"]["moduleGraphJson"]
    );
    let root_key = |data: &Value, name: &str| {
        data["metadata"]["configuredRoots"]["roots"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(label, _)| label.ends_with(name))
            .unwrap()
            .1
            .as_str()
            .unwrap()
            .to_owned()
    };
    let product = root_key(&module_only.data, "//:module_product");
    let base_product = &base.data["metadata"]["configuredRoots"]["hashes"][&product];
    assert!(!base_product.is_null());
    assert_eq!(
        base_product,
        &module_only.data["metadata"]["configuredRoots"]["hashes"][&product]
    );
    let overlap_key = root_key(&base.data, "//:module_overlap");
    assert_eq!(
        overlap_key,
        root_key(&module_only.data, "//:module_overlap")
    );
    assert_eq!(overlap_key, root_key(&changed.data, "//:module_overlap"));
    assert_ne!(
        base.data["metadata"]["configuredRoots"]["hashes"][&overlap_key],
        changed.data["metadata"]["configuredRoots"]["hashes"][&overlap_key]
    );
    let compare = |head: &Snapshot| {
        let report_path = output.path().join("module-report.json");
        let compared = impacted(
            head.workspace.path(),
            &base.hashes,
            &head.hashes,
            &report_path,
            &["-d", head.edges.to_str().unwrap(), "--reportImpactGraph"],
        );
        assert!(
            compared.status.success(),
            "{}",
            String::from_utf8_lossy(&compared.stderr)
        );
        let report: Value = serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
        (
            report,
            String::from_utf8_lossy(&compared.stderr).into_owned(),
        )
    };
    let has_path = |report: &Value, kind: &str, name: &str| {
        report["paths"].as_array().unwrap().iter().any(|path| {
            path["kind"] == kind
                && path["roots"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|label| label.ends_with(name))
        })
    };
    let (report, stderr) = compare(&module_only);
    assert!(!stderr.contains("Unioned rdeps query failed"), "{stderr}");
    assert!(has_path(&report, "forced", "//:module_product"), "{report}");
    assert!(!has_path(&report, "shared", "//:module_product"));
    let product = report["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["label"]
                .as_str()
                .unwrap()
                .ends_with("//:module_product")
        })
        .unwrap();
    assert_eq!(product["targetDistance"], 0);
    assert_eq!(product["packageDistance"], 0);

    let build_path = module_only.workspace.path().join("BUILD.bazel");
    let build = fs::read(&build_path).unwrap();
    fs::write(&build_path, "syntax error !\n").unwrap();
    let (fallback, stderr) = compare(&module_only);
    fs::write(build_path, build).unwrap();
    assert!(stderr.contains("Unioned rdeps query failed"), "{stderr}");
    assert!(has_path(&fallback, "forced", "//:module_product"));

    let (overlap, stderr) = compare(&changed);
    assert!(!stderr.contains("Unioned rdeps query failed"), "{stderr}");
    assert!(has_path(&overlap, "configured", "//:module_overlap"));
    assert!(has_path(&overlap, "forced", "//:module_overlap"));

    let ordinary_base: Value = serde_json::from_slice(&fs::read(&base.ordinary).unwrap()).unwrap();
    let ordinary_head: Value =
        serde_json::from_slice(&fs::read(&changed.ordinary).unwrap()).unwrap();
    assert!(ordinary_head["hashes"]
        .as_object()
        .unwrap()
        .keys()
        .any(|label| label.starts_with("@@dep+//")));
    let consumer = ordinary_head["hashes"]
        .as_object()
        .unwrap()
        .keys()
        .find(|label| label.ends_with("//:consumer"))
        .unwrap();
    assert_eq!(
        ordinary_base["hashes"][consumer],
        ordinary_head["hashes"][consumer]
    );
    assert!(ordinary_head["hashes"]
        .as_object()
        .unwrap()
        .keys()
        .all(|label| !label.ends_with("//:module_product")));
    let queried = std::process::Command::new(bazel())
        .current_dir(changed.workspace.path())
        .args(["query", "rdeps(//..., @@dep+//...)", "--output=label"])
        .output()
        .unwrap();
    assert!(
        queried.status.success(),
        "{}",
        String::from_utf8_lossy(&queried.stderr)
    );
    assert!(String::from_utf8_lossy(&queried.stdout)
        .lines()
        .any(|label| label.ends_with("//:module_product")));
    for target_type in [false, true] {
        let report_path = output.path().join("ordinary-module-report.json");
        let mut args = vec![
            "-d",
            changed.ordinary_edges.to_str().unwrap(),
            "--reportImpactGraph",
        ];
        if target_type {
            args.extend(["--targetType", "Rule"]);
        }
        let compared = impacted(
            changed.workspace.path(),
            &base.ordinary,
            &changed.ordinary,
            &report_path,
            &args,
        );
        assert!(
            compared.status.success(),
            "{}",
            String::from_utf8_lossy(&compared.stderr)
        );
        let report: Value = serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
        assert!(report["targets"].as_array().unwrap().iter().all(|row| {
            !row["label"]
                .as_str()
                .unwrap()
                .ends_with("//:module_product")
        }));
        assert!(!has_path(&report, "forced", "//:module_product"));
        assert!(
            has_path(&report, "ordinary", "//:module_overlap"),
            "{report}"
        );
        assert!(has_path(&report, "forced", "//:module_overlap"), "{report}");
        assert!(has_path(&report, "forced", "//:consumer"), "{report}");
        let ordinary = report["paths"]
            .as_array()
            .unwrap()
            .iter()
            .find(|path| path["kind"] == "ordinary")
            .unwrap();
        for field in ["changed", "direct"] {
            assert!(ordinary[field]
                .as_array()
                .unwrap()
                .iter()
                .all(|key| { !key.as_str().unwrap().ends_with("//:consumer") }));
        }
    }
}

/// A repository built from a directory in the workspace that its rule names by absolute path, as
/// `rules_rs` local crates and Gazelle's `local_path` do. Two identical checkouts at different
/// paths must impact nothing, with or without the repository fine-grained: `//:tool_user` depends
/// on `ext`, whose node has an edge to `@vendored`. Fine-grained, an edit to the directory still
/// reaches `//:consumer`.
#[test]
fn repository_named_by_workspace_path_does_not_depend_on_its_path() {
    if !supports_mod_show_repo() {
        return;
    }
    let first = copy_workspace("workspace_path_repository");
    let second = copy_workspace("workspace_path_repository");
    let edited = copy_workspace("workspace_path_repository");
    fs::write(edited.path().join("vendored/data.txt"), "v2\n").unwrap();
    let fine_grained = ["--fineGrainedHashExternalRepos=@vendored"];
    for args in [&[][..], &fine_grained[..]] {
        let impacted = diff_workspaces(first.path(), second.path(), args);
        assert!(impacted.is_empty(), "{args:?}: {impacted:?}");
    }

    let impacted = diff_workspaces(first.path(), edited.path(), &fine_grained);
    assert!(
        impacted
            .iter()
            .any(|label| label == "//:consumer" || label == "@@//:consumer"),
        "{impacted:?}"
    );
    assert!(!impacted.contains("//:tool_user"), "{impacted:?}");
    assert!(!impacted.contains("//:control"), "{impacted:?}");
}

/// A repository built from `<workspace>/../data`, a directory next to the workspace rather than
/// in it. The path starts with the workspace's, but the two checkouts' `data` directories differ,
/// so the consumer must be impacted: the path cannot be hashed relative to the workspace.
#[test]
fn repository_outside_the_workspace_keeps_its_path() {
    if !supports_mod_show_repo() {
        return;
    }
    let first = copy_workspace("workspace_outside_repository");
    let second = copy_workspace("workspace_outside_repository");
    fs::write(second.path().join("data/data.txt"), "v2\n").unwrap();
    let impacted = diff_workspaces(&first.path().join("ws"), &second.path().join("ws"), &[]);
    assert!(
        impacted
            .iter()
            .any(|label| label == "//:consumer" || label == "@@//:consumer"),
        "{impacted:?}"
    );
}

/// Regression for https://github.com/Tinder/bazel-diff/issues/544: the local registry lives inside
/// the workspace (`write_local_registry` registers it by its absolute path, which is what
/// `%workspace%` expands to), so the `file://` URLs of `dep`'s registry patch and `MODULE.bazel`
/// name each checkout's absolute path. Two identical checkouts at different paths must impact
/// nothing, while a changed registry patch, which changes its integrity, still reaches
/// `//:consumer`.
#[test]
fn registry_urls_under_the_workspace_do_not_depend_on_its_path() {
    if !supports_mod_show_repo() {
        return;
    }
    // A fresh copy per state instead of editing one in place: a Bazel server caches a local
    // registry's files and would not see an edited patch.
    let first = copy_workspace("single_version_override_patch");
    let second = copy_workspace("single_version_override_patch");
    let changed = copy_workspace("single_version_override_patch");
    for (workspace, content) in [
        (first.path(), "v1"),
        (second.path(), "v1"),
        (changed.path(), "v2"),
    ] {
        write_local_registry(workspace);
        add_registry_patch(workspace, content);
    }
    let impacted = diff_workspaces(first.path(), second.path(), &[]);
    assert!(impacted.is_empty(), "{impacted:?}");

    let impacted = diff_workspaces(first.path(), changed.path(), &[]);
    assert!(
        impacted
            .iter()
            .any(|label| label == "//:consumer" || label == "@@//:consumer"),
        "{impacted:?}"
    );
    assert!(!impacted.contains("//:control"), "{impacted:?}");
}

/// Adds a patch that creates `registry.txt` containing `content` to `dep`'s registry entry (the
/// `source.json` that `write_local_registry` writes).
fn add_registry_patch(workspace: &Path, content: &str) {
    use sha2::{Digest, Sha256};

    let module = workspace.join("registry/modules/dep/1.0");
    let patch = format!("--- /dev/null\n+++ b/registry.txt\n@@ -0,0 +1 @@\n+{content}\n");
    fs::create_dir_all(module.join("patches")).unwrap();
    fs::write(module.join("patches/registry.patch"), &patch).unwrap();
    let source_path = module.join("source.json");
    let source = fs::read_to_string(&source_path).unwrap();
    let source = source.trim_end().strip_suffix('}').unwrap();
    fs::write(
        &source_path,
        format!(
            "{source}, \"patches\": {{\"registry.patch\": \"sha256-{}\"}}, \"patch_strip\": 1}}\n",
            base64(&Sha256::digest(patch.as_bytes()))
        ),
    )
    .unwrap();
}

/// Turns the `single_version_override_patch` fixture into a working Bzlmod registry: zips
/// `dep_src/` into the archive the registry's `source.json` points at (a `file://` URL and its
/// integrity depend on the temp path, so neither is checked in) and adds the registry to the
/// workspace's `.bazelrc` ahead of the BCR, which still serves Bazel's implicit dependencies.
fn write_local_registry(workspace: &std::path::Path) {
    write_local_registry_version(workspace, "1.0");
}

fn write_local_registry_version(workspace: &std::path::Path, version: &str) {
    use sha2::{Digest, Sha256};
    use std::io::Write;

    let archive = workspace.join("dep.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for name in ["MODULE.bazel", "BUILD.bazel", "data.txt"] {
        zip.start_file(format!("dep/{name}"), options).unwrap();
        zip.write_all(&fs::read(workspace.join("dep_src").join(name)).unwrap())
            .unwrap();
    }
    zip.finish().unwrap();
    let digest = Sha256::digest(fs::read(&archive).unwrap());
    fs::write(
        workspace.join(format!("registry/modules/dep/{version}/source.json")),
        format!(
            "{{\"url\": \"file://{}\", \"integrity\": \"sha256-{}\", \"strip_prefix\": \"dep\"}}\n",
            archive.display(),
            base64(&digest)
        ),
    )
    .unwrap();
    fs::write(
        workspace.join(".bazelrc"),
        format!(
            "common --registry=file://{}\ncommon --registry=https://bcr.bazel.build\n",
            workspace.join("registry").display()
        ),
    )
    .unwrap();
}

/// Standard base64 with padding, as Subresource Integrity strings use.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut word = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            word |= u32::from(*byte) << (16 - 8 * index);
        }
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(ALPHABET[((word >> (18 - 6 * index)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
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

#[test]
fn ordinary_query_cycles_propagate_outgoing_changes() {
    let workspace = copy_workspace("ordinary_query_cycle");
    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let from_edges = output.path().join("from-edges.json");
    let to_edges = output.path().join("to-edges.json");
    for (value, hashes, edges) in [
        ("before\n", &from, &from_edges),
        ("after\n", &to, &to_edges),
    ] {
        fs::write(workspace.path().join("leaf.txt"), value).unwrap();
        let build = std::process::Command::new(bazel())
            .current_dir(workspace.path())
            .args(["build", "//:consumer"])
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("bazel-bin/consumer.txt")).unwrap(),
            value
        );
        assert!(generate(
            workspace.path(),
            hashes,
            &["--includeTargetType", "-d", edges.to_str().unwrap()],
        )
        .status
        .success());
    }

    let before = hashes(&from);
    let after = hashes(&to);
    for label in ["//:a", "//:b", "//:c", "//:consumer"] {
        assert_eq!(
            before[label].rsplit_once('~').unwrap().1,
            after[label].rsplit_once('~').unwrap().1,
            "{label} changed directly"
        );
    }
    assert_ne!(
        before["//:leaf"].rsplit_once('~').unwrap().1,
        after["//:leaf"].rsplit_once('~').unwrap().1
    );
    assert_eq!(before["//:unrelated"], after["//:unrelated"]);
    let edges = dep_edges(&to_edges);
    assert_eq!(dep_edges(&from_edges), edges);
    for (from, to) in [
        ("//:a", "//:c"),
        ("//:a", "//:leaf"),
        ("//:b", "//:a"),
        ("//:c", "//:b"),
        ("//:consumer", "//:c"),
    ] {
        assert!(edges[from].contains(to), "missing {from} -> {to}");
    }

    let result = output.path().join("impacted.json");
    assert!(impacted(
        workspace.path(),
        &from,
        &to,
        &result,
        &["-d", to_edges.to_str().unwrap(), "-tt", "Rule"]
    )
    .status
    .success());
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&fs::read(result).unwrap()).unwrap();
    let distances: BTreeMap<_, _> = rows
        .iter()
        .map(|row| {
            (
                row["label"].as_str().unwrap(),
                (
                    row["targetDistance"].as_u64().unwrap(),
                    row["packageDistance"].as_u64().unwrap(),
                ),
            )
        })
        .filter(|(label, _)| {
            [
                "//:leaf",
                "//:a",
                "//:b",
                "//:c",
                "//:consumer",
                "//:unrelated",
            ]
            .contains(label)
        })
        .collect();
    assert_eq!(
        distances,
        BTreeMap::from([
            ("//:leaf", (0, 0)),
            ("//:a", (1, 0)),
            ("//:b", (2, 0)),
            ("//:c", (3, 0)),
            ("//:consumer", (4, 0)),
        ])
    );
}

/// Regression test for <https://github.com/Tinder/bazel-diff/issues/512>.
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

#[test]
fn cquery_verifies_aspect_free_selected_toolchain() {
    let workspace = copy_workspace("cquery_nested_toolchains");
    let root = workspace.path();
    edit(&root.join("BUILD"), |text| {
        format!("{text}\ngenrule(name = \"only_x_product\", srcs = [\":only_x\"], outs = [\"only_x_product.txt\"], cmd = \"cat $< > $@\")\n")
    });
    fs::write(root.join("x.txt"), "X1").unwrap();
    fs::write(root.join("y.txt"), "Y1").unwrap();
    let capability = std::process::Command::new(bazel())
        .current_dir(root)
        .args([
            "cquery",
            "deps(config(//:only_x_product, target))",
            "--experimental_explicit_aspects",
            "--implicit_deps",
            "--tool_deps",
            "--nodep_deps",
            "--include_aspects",
            "--transitions=lite",
            "--proto:include_configurations",
            "--proto:rule_inputs_and_outputs",
            "--consistent_labels",
            "--output=jsonproto",
        ])
        .output()
        .unwrap();
    if !capability.status.success() {
        let stderr = String::from_utf8_lossy(&capability.stderr);
        if stderr.contains("Unrecognized option: --experimental_explicit_aspects") {
            return;
        }
        panic!("explicit-aspect capability query failed: {stderr}");
    }
    let capability: serde_json::Value = serde_json::from_slice(&capability.stdout).unwrap();
    if capability["explicitAspectInputs"] != true {
        return;
    }
    assert!(capability["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["target"]["rule"]["name"]
            .as_str()
            .is_some_and(|name| name.trim_start_matches('@') == "//:only_x_product")));
    for field in ["aspects", "aspectResults"] {
        assert!(capability
            .get(field)
            .is_none_or(|rows| rows.as_array().is_some_and(Vec::is_empty)));
    }
    let out = tempfile::tempdir().unwrap();
    let base = out.path().join("base.json");
    let artifacts = std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR").map(std::path::PathBuf::from);
    for (name, x, y) in [
        ("base", "X1", "Y1"),
        ("sibling", "X1", "Y2"),
        ("compiler", "X2", "Y1"),
    ] {
        fs::write(root.join("x.txt"), x).unwrap();
        fs::write(root.join("y.txt"), y).unwrap();
        let hashes = out.path().join(format!("{name}.json"));
        let edges = out.path().join(format!("{name}-edges.json"));
        let generated = generate(
            root,
            &hashes,
            &[
                "--useCquery",
                "--cqueryRootExpression",
                "//:only_x_product",
                "--verifyConfiguredRoots",
                "--includeTargetType",
                "-d",
                edges.to_str().unwrap(),
            ],
        );
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let build = std::process::Command::new(bazel())
            .current_dir(root)
            .args(["build", "//:only_x_product"])
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        assert_eq!(
            fs::read_to_string(root.join("bazel-bin/only_x_product.txt")).unwrap(),
            x
        );
        let report = out.path().join(format!("{name}-report.json"));
        let compared = impacted(
            root,
            &base,
            &hashes,
            &report,
            &["-d", edges.to_str().unwrap(), "--reportImpactGraph"],
        );
        assert!(
            compared.status.success(),
            "{}",
            String::from_utf8_lossy(&compared.stderr)
        );
        let result: serde_json::Value =
            serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
        let changed = result["targets"].as_array().unwrap().iter().any(|target| {
            target["label"].as_str().unwrap().trim_start_matches('@') == "//:only_x_product"
        });
        let unaffected = result["unaffectedRoots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|label| label.as_str().unwrap().trim_start_matches('@') == "//:only_x_product");
        assert_eq!(changed, name == "compiler", "{name}: {result}");
        assert_eq!(unaffected, name != "compiler", "{name}: {result}");
        if let Some(artifacts) = &artifacts {
            fs::copy(&hashes, artifacts.join(format!("{name}-hashes.json"))).unwrap();
            fs::copy(&edges, artifacts.join(format!("{name}-edges.json"))).unwrap();
            fs::copy(&report, artifacts.join(format!("{name}-report.json"))).unwrap();
        }
    }
}

#[test]
fn cquery_rejects_indistinguishable_toolchain_instances() {
    let workspace = copy_workspace("cquery_nested_toolchains");
    let workspace = workspace.path();
    let output = tempfile::tempdir().unwrap();
    let rooted = |root: &str, hashes: &Path| {
        generate(
            workspace,
            hashes,
            &["--useCquery", "--cqueryRootExpression", root],
        )
    };

    for (state, x, y) in [("before", "X1", "Y1"), ("x", "X2", "Y1"), ("y", "X1", "Y2")] {
        fs::write(workspace.join("x.txt"), x).unwrap();
        fs::write(workspace.join("y.txt"), y).unwrap();
        let build = std::process::Command::new(bazel())
            .current_dir(workspace)
            .args(["build", "//:joined"])
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        assert_eq!(
            fs::read_to_string(workspace.join("bazel-bin/joined.txt")).unwrap(),
            format!("{x}{y}")
        );

        // Either reader alone reaches just one instance of the forwarding toolchain.
        for (branch, root) in [("x", "//:only_x"), ("y", "//:only_y")] {
            let hashes = output.path().join(format!("{state}-{branch}.json"));
            let result = rooted(root, &hashes);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            if state == "before" {
                continue;
            }
            let before = output.path().join(format!("before-{branch}.json"));
            let labels = output.path().join("impacted.txt");
            let result = impacted(workspace, &before, &hashes, &labels, &[]);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let selected: Vec<_> = read_lines(&labels)
                .into_iter()
                .map(|label| label.trim_start_matches('@').to_owned())
                .collect();
            let expected: Vec<_> = (state == branch).then_some(root).into_iter().collect();
            assert_eq!(selected, expected);
        }
    }

    // A union and a single parent both reach two outer_impl rows with the same public key.
    for root in ["set(//:only_x //:only_y)", "//:joined"] {
        let result = rooted(root, &output.path().join("ambiguous.json"));
        assert!(
            !result.status.success(),
            "accepted ambiguous configured inputs for {root}"
        );
        assert!(String::from_utf8_lossy(&result.stderr).contains("//:outer_impl"));
    }
}

#[test]
fn cquery_reports_only_requested_configured_roots() {
    let workspace = copy_workspace("cquery_transition_dedup");
    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let from_edges = output.path().join("from-edges.json");
    let to_edges = output.path().join("to-edges.json");
    let legacy_from = output.path().join("legacy-from.json");
    let legacy_to = output.path().join("legacy-to.json");
    let roots = "set(//:shared //:root_alias //:input.txt)";
    let roots_file = output.path().join("roots.query");
    fs::write(&roots_file, roots).unwrap();
    let closure = format!("deps(config(({roots}), target))");
    for (value, hashes, edges, legacy, root_flag, root_value) in [
        (
            "before\n",
            &from,
            &from_edges,
            &legacy_from,
            "--cqueryRootExpression",
            roots,
        ),
        (
            "after\n",
            &to,
            &to_edges,
            &legacy_to,
            "--cqueryRootExpressionFile",
            roots_file.to_str().unwrap(),
        ),
    ] {
        fs::write(workspace.path().join("input.txt"), value).unwrap();
        let build = std::process::Command::new(bazel())
            .current_dir(workspace.path())
            .args(["build", "//:shared", "//:root_alias", "//:input.txt"])
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("bazel-bin/shared.out")).unwrap(),
            "fixed\n"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("bazel-bin/wrapper.out")).unwrap(),
            value
        );
        let generated = generate(
            workspace.path(),
            hashes,
            &[
                "--useCquery",
                root_flag,
                root_value,
                "--cqueryCommandOptions=--noproto:include_configurations",
                "--cqueryCommandOptions=--output=files",
                "--cqueryCommandOptions=--transitions=none",
                "--cqueryCommandOptions=--noproto:rule_inputs_and_outputs",
                "--cqueryCommandOptions=--output_file=/dev/null",
                "--includeTargetType",
                "-d",
                edges.to_str().unwrap(),
            ],
        );
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let generated = generate(
            workspace.path(),
            legacy,
            &[
                "--useCquery",
                "--cqueryExpression",
                &closure,
                "--includeTargetType",
            ],
        );
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
    }

    for path in [&from, &to] {
        let data: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let labels: BTreeSet<_> = data["hashes"]
            .as_object()
            .unwrap()
            .keys()
            .map(|label| label.trim_start_matches('@'))
            .collect();
        assert_eq!(
            labels,
            BTreeSet::from(["//:input.txt", "//:root_alias", "//:shared"])
        );
        let roster = data["metadata"]["configuredRoots"]["roots"]
            .as_object()
            .unwrap();
        let (label, key) = roster
            .iter()
            .find(|(label, _)| label.ends_with("//:input.txt"))
            .unwrap();
        assert_eq!(key.as_str().unwrap(), label);
    }
    let result = output.path().join("impacted.txt");
    assert!(impacted(workspace.path(), &from, &to, &result, &[])
        .status
        .success());
    let selected: BTreeSet<_> = read_lines(&result)
        .into_iter()
        .map(|label| label.trim_start_matches('@').to_owned())
        .collect();
    assert_eq!(
        selected,
        BTreeSet::from(["//:input.txt".to_owned(), "//:root_alias".to_owned()])
    );
    assert!(
        impacted(workspace.path(), &legacy_from, &legacy_to, &result, &[])
            .status
            .success()
    );
    assert!(read_lines(&result)
        .iter()
        .any(|label| label.ends_with("//:shared")));

    assert!(impacted(
        workspace.path(),
        &from,
        &to,
        &result,
        &["-d", to_edges.to_str().unwrap()]
    )
    .status
    .success());
    let distance_rows: Vec<Value> = serde_json::from_slice(&fs::read(&result).unwrap()).unwrap();
    let distances: BTreeMap<_, _> = distance_rows
        .iter()
        .map(|row| {
            (
                row["label"].as_str().unwrap().trim_start_matches('@'),
                (
                    row["targetDistance"].as_u64().unwrap(),
                    row["packageDistance"].as_u64().unwrap(),
                ),
            )
        })
        .collect();
    assert_eq!(
        distances,
        BTreeMap::from([("//:input.txt", (0, 0)), ("//:root_alias", (3, 0))])
    );

    assert!(impacted(
        workspace.path(),
        &from,
        &to,
        &result,
        &["-d", to_edges.to_str().unwrap(), "--reportImpactGraph"]
    )
    .status
    .success());
    let report: Value = serde_json::from_slice(&fs::read(&result).unwrap()).unwrap();
    assert_eq!(report["targets"], Value::Array(distance_rows));
    let path = report["paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|path| path["kind"] == "configured")
        .unwrap();
    assert!(path["roots"]
        .as_object()
        .unwrap()
        .keys()
        .any(|label| label.ends_with("//:root_alias")));

    // Both new roots were already inputs at the base. The fixed source stays unchanged, while
    // wrapper's product changes through the shared configured instance. Enrollment alone must
    // not select the source or hide the wrapper's measured path.
    let newly_requested = output.path().join("newly-requested.json");
    let newly_requested_edges = output.path().join("newly-requested-edges.json");
    let generated = generate(
        workspace.path(),
        &newly_requested,
        &[
            "--useCquery",
            "--cqueryRootExpression",
            "set(//:shared //:root_alias //:input.txt //:fixed.txt //:wrapper)",
            "--includeTargetType",
            "-d",
            newly_requested_edges.to_str().unwrap(),
        ],
    );
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let base_data: Value = serde_json::from_slice(&fs::read(&from).unwrap()).unwrap();
    let head_data: Value = serde_json::from_slice(&fs::read(&newly_requested).unwrap()).unwrap();
    let fixed_key = head_data["metadata"]["configuredRoots"]["roots"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(label, _)| label.ends_with("//:fixed.txt"))
        .unwrap()
        .1
        .as_str()
        .unwrap();
    let fixed_hash = &base_data["metadata"]["configuredRoots"]["hashes"][fixed_key];
    assert!(!fixed_hash.is_null());
    assert_eq!(
        fixed_hash,
        &head_data["metadata"]["configuredRoots"]["hashes"][fixed_key]
    );
    let wrapper_key = head_data["metadata"]["configuredRoots"]["roots"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(label, _)| label.ends_with("//:wrapper"))
        .unwrap()
        .1
        .as_str()
        .unwrap();
    let base_wrapper = base_data["metadata"]["configuredRoots"]["hashes"][wrapper_key]
        .as_str()
        .unwrap();
    let head_wrapper = head_data["metadata"]["configuredRoots"]["hashes"][wrapper_key]
        .as_str()
        .unwrap();
    assert_ne!(base_wrapper, head_wrapper);
    assert_eq!(
        base_wrapper.rsplit_once('~').unwrap().1,
        head_wrapper.rsplit_once('~').unwrap().1
    );
    assert!(impacted(
        workspace.path(),
        &from,
        &newly_requested,
        &result,
        &["-d", newly_requested_edges.to_str().unwrap()]
    )
    .status
    .success());
    let legacy: Vec<Value> = serde_json::from_slice(&fs::read(&result).unwrap()).unwrap();
    assert!(legacy.iter().any(|row| {
        row["label"].as_str().unwrap().ends_with("//:fixed.txt") && row["targetDistance"] == 0
    }));
    assert!(impacted(
        workspace.path(),
        &from,
        &newly_requested,
        &result,
        &[
            "-d",
            newly_requested_edges.to_str().unwrap(),
            "--reportImpactGraph",
        ]
    )
    .status
    .success());
    let report: Value = serde_json::from_slice(&fs::read(&result).unwrap()).unwrap();
    assert!(report["targets"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| !row["label"].as_str().unwrap().ends_with("//:fixed.txt")));
    let wrapper = report["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["label"].as_str().unwrap().ends_with("//:wrapper"))
        .unwrap();
    assert_eq!(wrapper["targetDistance"], 2);
    assert_eq!(wrapper["packageDistance"], 0);
    assert!(report["paths"].as_array().unwrap().iter().any(|path| {
        path["kind"] == "shared"
            && path["roots"]
                .as_object()
                .unwrap()
                .keys()
                .any(|label| label.ends_with("//:wrapper"))
    }));

    let empty = output.path().join("empty.json");
    let generated = generate(
        workspace.path(),
        &empty,
        &[
            "--useCquery",
            "--cqueryRootExpression",
            "set()",
            "--cqueryCommandOptions=--output=files",
            "--cqueryCommandOptions=--output_file=/dev/null",
        ],
    );
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let data: Value = serde_json::from_slice(&fs::read(&empty).unwrap()).unwrap();
    assert!(data["hashes"].as_object().unwrap().is_empty());
    assert!(data["metadata"]["configuredRoots"]["roots"]
        .as_object()
        .unwrap()
        .is_empty());

    let missing = output.path().join("missing.json");
    let generated = generate(
        workspace.path(),
        &missing,
        &[
            "--useCquery",
            "--cqueryRootExpression",
            "//:missing",
            "--keep_going",
            "--cqueryCommandOptions=--output_file=/dev/null",
        ],
    );
    assert!(!generated.status.success());
    assert!(!missing.exists());
}

/// A fresh typed view can verify an independent main-source product while leaving a reached
/// aspect unresolved. The changed product still uses the normal configured source path.
#[test]
fn cquery_verifies_covered_configured_roots() {
    let workspace = copy_workspace("cquery_transition_dedup");
    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("B-hashes.json");
    let to = output.path().join("H-hashes.json");
    let edges = output.path().join("H-edges.json");
    let renamed = output.path().join("R-hashes.json");
    let renamed_edges = output.path().join("R-edges.json");
    let featured = output.path().join("F-hashes.json");
    let featured_edges = output.path().join("F-edges.json");
    let artifacts = std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR").map(std::path::PathBuf::from);
    // Keep both sources outside the explicit fine-repository list. Separate modules let the
    // stable product remain independent of the changed module's repository definition.
    for repo in ["stable_source", "changed_source"] {
        let dir = workspace.path().join(repo);
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join("MODULE.bazel"),
            format!("module(name = \"{repo}\")\n"),
        )
        .unwrap();
        fs::write(
            dir.join("BUILD.bazel"),
            "exports_files([\"payload.txt\"])\n",
        )
        .unwrap();
        fs::write(dir.join("payload.txt"), "fixed external\n").unwrap();
        let module = workspace.path().join("MODULE.bazel");
        let text = fs::read_to_string(&module).unwrap();
        fs::write(&module, format!("{text}\nbazel_dep(name = \"{repo}\")\nlocal_path_override(module_name = \"{repo}\", path = \"{repo}\")\n")).unwrap();
        let file = workspace.path().join("BUILD");
        let text = fs::read_to_string(&file).unwrap();
        fs::write(
            &file,
            format!("{text}\nobserved_copy(name = \"{repo}\", src = \"@{repo}//:payload.txt\")\n"),
        )
        .unwrap();
    }
    let mut reports = BTreeMap::new();
    let mut typed = false;
    for (name, value, hashes, head_edges) in [
        ("B", "verified-before\n", &from, None),
        ("H", "verified-after\n", &to, Some(&edges)),
        ("R", "verified-after\n", &renamed, Some(&renamed_edges)),
        ("F", "verified-after\n", &featured, Some(&featured_edges)),
    ] {
        if name == "R" {
            fs::rename(
                workspace.path().join("BUILD"),
                workspace.path().join("BUILD.bazel"),
            )
            .unwrap();
        } else if name == "F" {
            let build = workspace.path().join("BUILD.bazel");
            let contents = fs::read_to_string(&build).unwrap();
            let updated = contents.replacen(
                "\nbootstrap_flag(",
                "\npackage(features = [\"probe\"])\n\nbootstrap_flag(",
                1,
            );
            assert_ne!(updated, contents);
            fs::write(build, updated).unwrap();
        }
        fs::write(workspace.path().join("input.txt"), value).unwrap();
        fs::write(workspace.path().join("changed_source/payload.txt"), value).unwrap();
        let build = std::process::Command::new(bazel())
            .current_dir(workspace.path())
            .args([
                "build",
                "//:observed",
                "//:observed_changed",
                "//:stable_source",
                "//:changed_source",
            ])
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let build_file = if matches!(name, "B" | "H") {
            "BUILD"
        } else {
            "BUILD.bazel"
        };
        let feature = if name == "F" { "on" } else { "off" };
        assert_eq!(
            fs::read_to_string(workspace.path().join("bazel-bin/observed.out")).unwrap(),
            format!("{build_file}|{feature}|fixed\n")
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("bazel-bin/observed_changed.out")).unwrap(),
            format!("{build_file}|{feature}|{value}")
        );
        for (target, input) in [
            ("stable_source", "fixed external\n"),
            ("changed_source", value),
        ] {
            assert_eq!(
                fs::read_to_string(workspace.path().join(format!("bazel-bin/{target}.out")))
                    .unwrap(),
                format!("{build_file}|{feature}|{input}")
            );
        }
        if name == "B" {
            let capability = std::process::Command::new(bazel())
                .current_dir(workspace.path())
                .args([
                    "cquery",
                    "deps(config(set(//:observed //:observed_changed //:stable_source //:changed_source), target))",
                    "--experimental_explicit_aspects",
                    "--implicit_deps",
                    "--tool_deps",
                    "--nodep_deps",
                    "--include_aspects",
                    "--transitions=lite",
                    "--proto:include_configurations",
                    "--proto:rule_inputs_and_outputs",
                    "--consistent_labels",
                    "--output=jsonproto",
                ])
                .output()
                .unwrap();
            let response = serde_json::from_slice::<Value>(&capability.stdout).ok();
            typed = capability.status.success()
                && response.as_ref().is_some_and(|response| {
                    response["aspects"]
                        .as_array()
                        .is_some_and(|rows| !rows.is_empty())
                        && response["aspectResults"]
                            .as_array()
                            .is_some_and(|rows| !rows.is_empty())
                });
            if let Some(artifacts) = &artifacts {
                fs::write(artifacts.join("typed-response"), capability.stdout).unwrap();
                fs::write(artifacts.join("typed-response.stderr"), capability.stderr).unwrap();
            }
        }
        let mut arguments = vec![
            "-v",
            "--useCquery",
            "--cqueryRootExpression",
            "set(//:observed //:observed_changed //:stable_source //:changed_source)",
            "--verifyConfiguredRoots",
            "--includeTargetType",
        ];
        if let Some(edges) = head_edges {
            arguments.extend(["-d", edges.to_str().unwrap()]);
        }
        let generated = generate(workspace.path(), hashes, &arguments);
        if let Some(artifacts) = &artifacts {
            fs::write(
                artifacts.join(format!("{name}-generation.stderr")),
                &generated.stderr,
            )
            .unwrap();
        }
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        if let Some(artifacts) = &artifacts {
            fs::copy(hashes, artifacts.join(format!("{name}-hashes.json"))).unwrap();
        }
        if let Some(head_edges) = head_edges {
            let (before, report_name) = match name {
                "H" => (&from, "report.json"),
                "R" => (&to, "rename-report.json"),
                "F" => (&renamed, "feature-report.json"),
                _ => unreachable!(),
            };
            let path = output.path().join(report_name);
            let compared = impacted(
                workspace.path(),
                before,
                hashes,
                &path,
                &["-d", head_edges.to_str().unwrap(), "--reportImpactGraph"],
            );
            assert!(
                compared.status.success(),
                "{}",
                String::from_utf8_lossy(&compared.stderr)
            );
            if let Some(artifacts) = &artifacts {
                fs::copy(head_edges, artifacts.join(format!("{name}-edges.json"))).unwrap();
                fs::copy(&path, artifacts.join(report_name)).unwrap();
            }
            let result = serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap();
            if name == "H" {
                let standard = output.path().join("standard-H.json");
                let standard_edges = output.path().join("standard-H-edges.json");
                let generated = generate(
                    workspace.path(),
                    &standard,
                    &[
                        "--useCquery",
                        "--cqueryRootExpression",
                        "set(//:observed //:observed_changed //:stable_source //:changed_source)",
                        "--includeTargetType",
                        "-d",
                        standard_edges.to_str().unwrap(),
                    ],
                );
                assert!(
                    generated.status.success(),
                    "{}",
                    String::from_utf8_lossy(&generated.stderr)
                );
                let mut verified: Value =
                    serde_json::from_slice(&fs::read(hashes).unwrap()).unwrap();
                verified["metadata"]["configuredRoots"]
                    .as_object_mut()
                    .unwrap()
                    .remove("verifiedRoots");
                let standard_data: Value =
                    serde_json::from_slice(&fs::read(&standard).unwrap()).unwrap();
                assert_eq!(verified, standard_data);
                assert_eq!(
                    fs::read(head_edges).unwrap(),
                    fs::read(&standard_edges).unwrap()
                );
                let one_sided = output.path().join("one-sided-report.json");
                assert!(impacted(
                    workspace.path(),
                    &from,
                    &standard,
                    &one_sided,
                    &[
                        "-d",
                        standard_edges.to_str().unwrap(),
                        "--reportImpactGraph"
                    ]
                )
                .status
                .success());
                let one_sided_result: Value =
                    serde_json::from_slice(&fs::read(&one_sided).unwrap()).unwrap();
                assert_eq!(one_sided_result["targets"], result["targets"]);
                assert!(one_sided_result["unaffectedRoots"]
                    .as_array()
                    .unwrap()
                    .is_empty());
                if let Some(artifacts) = &artifacts {
                    fs::copy(standard, artifacts.join("standard-H.json")).unwrap();
                    fs::copy(standard_edges, artifacts.join("standard-H-edges.json")).unwrap();
                    fs::copy(one_sided, artifacts.join("one-sided-report.json")).unwrap();
                }
            }
            reports.insert(name, result);
        }
    }
    let report = &reports["H"];
    let selected = report["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["label"].as_str().unwrap().trim_start_matches('@'))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        selected,
        BTreeSet::from(["//:observed_changed", "//:changed_source"])
    );
    let unaffected = report["unaffectedRoots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|label| label.as_str().unwrap().trim_start_matches('@'))
        .collect::<BTreeSet<_>>();
    if typed {
        let mut expected = BTreeSet::from(["//:observed"]);
        if cfg!(unix) {
            expected.insert("//:stable_source");
        }
        assert_eq!(unaffected, expected);
        for hashes in [&from, &to] {
            let data: Value = serde_json::from_slice(&fs::read(hashes).unwrap()).unwrap();
            let verified = data["metadata"]["configuredRoots"]["verifiedRoots"]
                .as_object()
                .unwrap()
                .keys()
                .map(|label| label.trim_start_matches('@'))
                .collect::<BTreeSet<_>>();
            let mut expected = BTreeSet::from(["//:observed"]);
            if cfg!(unix) {
                expected.extend(["//:stable_source", "//:changed_source"]);
            }
            assert_eq!(verified, expected);
        }
    } else {
        assert!(unaffected.is_empty());
    }

    let read = |path: &std::path::Path| -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    };
    let observed = |values: &Value| {
        values
            .as_object()
            .unwrap()
            .iter()
            .find(|(label, _)| label.ends_with("//:observed"))
            .map(|(_, value)| value.clone())
    };
    let head = read(&to);
    let rename = read(&renamed);
    let feature = read(&featured);
    if typed {
        assert_eq!(observed(&head["hashes"]), observed(&rename["hashes"]));
        assert_eq!(observed(&rename["hashes"]), observed(&feature["hashes"]));
        assert_ne!(
            observed(&head["metadata"]["configuredRoots"]["verifiedRoots"]),
            observed(&rename["metadata"]["configuredRoots"]["verifiedRoots"])
        );
        assert!(observed(&feature["metadata"]["configuredRoots"]["verifiedRoots"]).is_none());
    }
    for name in ["R", "F"] {
        let result = &reports[name];
        assert!(result["unaffectedRoots"].as_array().unwrap().is_empty());
        if typed {
            assert!(result["targets"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| { !row["label"].as_str().unwrap().ends_with("//:observed") }));
        }
    }

    let malformed_from = output.path().join("malformed-B.json");
    let malformed_to = output.path().join("malformed-H.json");
    for (source, destination) in [(&from, &malformed_from), (&to, &malformed_to)] {
        let mut data = read(source);
        data["metadata"]["moduleGraphJson"] = Value::String(String::new());
        fs::write(destination, serde_json::to_vec(&data).unwrap()).unwrap();
    }
    let malformed_report = output.path().join("malformed-report.json");
    assert!(impacted(
        workspace.path(),
        &malformed_from,
        &malformed_to,
        &malformed_report,
        &["-d", edges.to_str().unwrap(), "--reportImpactGraph"]
    )
    .status
    .success());
    let malformed = read(&malformed_report);
    assert!(malformed["unaffectedRoots"].as_array().unwrap().is_empty());
    assert_eq!(malformed["targets"], report["targets"]);
}

/// Regression for https://github.com/Tinder/bazel-diff/issues/522 and hashbench's
/// `toolchain_patch_edit` scenario: a repository created by a module extension that no
/// module imports (`go_sdk.download(patches = ...)`) is reached by toolchain resolution
/// only. Editing its patch changes what every user of the toolchain builds, so the users
/// must be impacted even though `bazel query` shows no edge from them to the SDK repo.
#[test]
fn extension_repo_patch_edit_impacts_toolchain_user() {
    if !supports_mod_show_repo() {
        return;
    }
    let first = copy_workspace("extension_repo_patch");
    let second = copy_workspace("extension_repo_patch");
    edit(&second.path().join("patches/sdk.patch"), |text| {
        text.replace("+1.0.0-patched", "+1.0.0-repatched")
    });
    let impacted = diff_workspaces(first.path(), second.path(), &[]);
    assert!(
        impacted
            .iter()
            .any(|label| label == "//:app" || label == "@@//:app"),
        "{impacted:?}"
    );
    assert!(
        impacted
            .iter()
            .all(|label| label.trim_start_matches('@') != "//:control"),
        "{impacted:?}"
    );

    // An alias in the fine-grained repository reaches sdk_impl. Its BUILD also
    // loads defs.bzl, so buildfiles can return a synthetic row for the SDK BUILD.
    // Renaming a transitive load must change that BUILD's hash at fixed BUILD bytes.
    let sdk_build_hash = |load: &str| {
        let workspace = copy_workspace("extension_repo_patch");
        let root = workspace.path();
        edit(&root.join("MODULE.bazel"), |text| {
            text + "\nuse_repo(sdk, \"sdk_aliases\")\n"
        });
        edit(&root.join("BUILD"), |text| {
            text + "\nfilegroup(name = \"sdk_probe\", srcs = [\"@sdk_aliases//:implementation\"])\n"
        });
        edit(&root.join("sdk_rules/extensions.bzl"), |text| {
            let generated = r#"rctx.file("BUILD.bazel", 'exports_files(["version.txt"])\n')"#;
            let alias = r#"rctx.file("BUILD.bazel", 'load("%s", "SdkInfo")\nexports_files(["version.txt"])\nalias(name = "implementation", actual = "%s", visibility = ["//visibility:public"])\n' % (Label("//:defs.bzl"), Label("//:sdk_impl")))"#;
            let sdk = "    _sdk_repo(name = \"default_sdk\"";
            text.replace(generated, alias).replace(
                sdk,
                &format!("    _sdk_repo(name = \"sdk_aliases\", version = \"1\")\n{sdk}"),
            )
        });
        edit(&root.join("sdk_rules/defs.bzl"), |text| {
            text.replace(
                "SdkInfo = provider",
                &format!("load(\":{load}.bzl\", \"marker\")\n\nSdkInfo = provider"),
            )
        });
        fs::write(root.join(format!("sdk_rules/{load}.bzl")), "marker = 1\n").unwrap();
        let output = tempfile::tempdir().unwrap();
        let file = output.path().join("hashes.json");
        let generated = generate(
            root,
            &file,
            &[
                "--excludeExternalTargets",
                "--includeTargetType",
                "--bazelCommandOptions=--nokeep_going --noproto:locations",
                "--fineGrainedHashExternalRepos=@sdk_aliases",
            ],
        );
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        hashes(&file)
            .remove("@sdk_rules//:BUILD")
            .expect("acquired SDK BUILD")
    };
    assert_ne!(sdk_build_hash("first"), sdk_build_hash("renamed"));
}

/// The cquery side of the case above: the resolved toolchain is a real configured input
/// there, so the extension repo's node alone carries the patch edit to `//:app`.
#[test]
fn extension_repo_patch_edit_impacts_toolchain_user_cquery() {
    if !supports_mod_show_repo() {
        return;
    }
    let first = copy_workspace("extension_repo_patch");
    let second = copy_workspace("extension_repo_patch");
    edit(&second.path().join("patches/sdk.patch"), |text| {
        text.replace("+1.0.0-patched", "+1.0.0-repatched")
    });
    let impacted = diff_workspaces(first.path(), second.path(), &["--useCquery"]);
    assert!(
        impacted
            .iter()
            .any(|label| label == "//:app" || label == "@@//:app"),
        "{impacted:?}"
    );
    assert!(!impacted.contains("//:control"), "{impacted:?}");
}

/// Without a change nothing is impacted: the extension repo nodes and their stand-in edges
/// hash deterministically across two checkouts of the same workspace.
#[test]
fn extension_repo_nodes_are_stable_without_a_change() {
    if !supports_mod_show_repo() {
        return;
    }
    let first = copy_workspace("extension_repo_patch");
    let second = copy_workspace("extension_repo_patch");
    let impacted = diff_workspaces(first.path(), second.path(), &[]);
    assert!(impacted.is_empty(), "{impacted:?}");
}

/// Reproducer for https://github.com/Tinder/bazel-diff/issues/531: a transitive module's
/// `use_extension` is broken (it imports a repository the extension no longer generates),
/// but nothing in `//...` reaches that extension, so Bazel's lazy evaluation keeps
/// `bazel build //...` green. `bazel mod graph` and `bazel mod show_repo` evaluate every
/// extension in the graph eagerly and exit 2 instead. bazel-diff reads the failing
/// `bazel mod graph` as "Bzlmod is disabled" and falls back to legacy hashing, so every
/// `//external:` node vanishes without an error -- the one for `@direct_dep`, which `//:app`
/// does use, included. The reporter saw the same eager evaluation surface as a hard exit 2.
#[test]
fn unused_transitive_extension_failure_keeps_bzlmod_repos_hashed() {
    if !supports_mod_show_repo() {
        return;
    }
    let workspace = copy_workspace("unused_transitive_extension");
    // The build's own loading never reaches the extension. A `deps` query loads the same
    // packages the build would without needing the toolchain repositories a build fetches.
    let query = std::process::Command::new(bazel())
        .args(["query", "deps(//...:all-targets)"])
        .current_dir(workspace.path())
        .output()
        .expect("run bazel query");
    assert!(
        query.status.success(),
        "the broken extension must not break loading the workspace itself: {}",
        String::from_utf8_lossy(&query.stderr)
    );
    let output = workspace.path().join("hashes.json");
    let result = generate(
        workspace.path(),
        &output,
        &[
            "--no-keep_going",
            "--excludeExternalTargets",
            "--includeTargetType",
        ],
    );
    assert!(result.status.success(), "generate-hashes must not fail");
    let hashes = hashes(&output);
    assert!(
        hashes.contains_key("//external:direct_dep"),
        "the module repository //:app depends on must still be hashed, got {:?}",
        hashes.keys().collect::<Vec<_>>()
    );
}

/// The user-visible consequence of the case above: with the `//external:direct_dep` node
/// gone, a change to the direct dependency's module definition no longer reaches `//:app`,
/// so the dependency upgrade that a pull request makes goes untested.
#[test]
fn unused_transitive_extension_failure_hides_direct_dependency_change() {
    if !supports_mod_show_repo() {
        return;
    }
    let first = copy_workspace("unused_transitive_extension");
    let second = copy_workspace("unused_transitive_extension");
    edit(&second.path().join("direct_dep/MODULE.bazel"), |text| {
        text.replace("version = \"1.0\",", "version = \"1.1\",")
    });
    let impacted = diff_workspaces(first.path(), second.path(), &["--no-keep_going"]);
    assert!(
        impacted
            .iter()
            .any(|label| label == "//:app" || label == "@@//:app"),
        "{impacted:?}"
    );
}

/// The counterpart of the two cases above: once a target does depend on a repository of
/// the failed extension, the hashes cannot cover that input. `bazel query //...:all-targets`
/// lists the target without loading its dependencies, so it is bazel-diff that has to say
/// so: an error under `--no-keep_going`, a warning and a hash set without the input
/// otherwise.
#[test]
fn referenced_failing_extension_fails_without_keep_going() {
    if !supports_mod_show_repo() {
        return;
    }
    let workspace = copy_workspace("unused_transitive_extension");
    edit(&workspace.path().join("MODULE.bazel"), |text| {
        text + "\nbazel_dep(name = \"transitive_dep\", version = \"1.0\")\n\
            broken = use_extension(\"@transitive_dep//:extensions.bzl\", \"broken\")\n\
            use_repo(broken, \"missing_repo\")\n"
    });
    edit(&workspace.path().join("BUILD"), |text| {
        text
            + "\nfilegroup(\n    name = \"uses_missing\",\n    srcs = [\"@missing_repo//:x\"],\n)\n"
    });
    let output = workspace.path().join("hashes.json");
    let strict = generate(workspace.path(), &output, &["--no-keep_going"]);
    assert!(
        !strict.status.success(),
        "the missing input must be an error"
    );
    let stderr = String::from_utf8_lossy(&strict.stderr);
    assert!(
        stderr.contains("//:uses_missing depends on @missing_repo//:x")
            && stderr.contains("extensions.bzl%broken"),
        "{stderr}"
    );
    let lenient = generate(workspace.path(), &output, &["--keep_going"]);
    assert!(lenient.status.success());
    let hashes = hashes(&output);
    assert!(
        hashes.contains_key("//:uses_missing") && hashes.contains_key("//external:direct_dep"),
        "{:?}",
        hashes.keys().collect::<Vec<_>>()
    );
}

#[test]
fn partial_extension_without_id_checks_references() {
    if !supports_mod_show_repo() {
        return;
    }
    let workspace = copy_workspace("partial_extension_without_id");
    let output = tempfile::tempdir().unwrap();
    let healthy_output = output.path().join("healthy.json");
    let healthy_result = generate(workspace.path(), &healthy_output, &[]);
    assert!(
        healthy_result.status.success(),
        "an unused failed extension must preserve healthy repositories: {}",
        String::from_utf8_lossy(&healthy_result.stderr)
    );
    let healthy_hashes = hashes(&healthy_output);
    assert!(
        healthy_hashes.contains_key("//:uses_healthy")
            && healthy_hashes.contains_key("//external:healthy"),
        "{:?}",
        healthy_hashes.keys().collect::<Vec<_>>()
    );

    edit(&workspace.path().join("BUILD"), |text| {
        text + "\nfilegroup(\n    name = \"uses_missing\",\n    srcs = [\"@@+broken+generated//:data\"],\n)\n"
    });
    for mode in ["--no-keep_going", "--keep_going"] {
        let path = output.path().join(format!("{mode}.json"));
        let result = generate(workspace.path(), &path, &[mode]);
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            !result.status.success(),
            "{mode} accepted the missing mapping: {error}"
        );
        assert!(
            error.contains("bazel mod dump_repo_mapping failed")
                && error.contains("error evaluating module extension broken in"),
            "{mode}: {error}"
        );
        assert!(
            !path.exists(),
            "{mode} published hashes after a mapping error"
        );
    }
}

#[test]
fn same_symbol_extension_failure_keeps_healthy_repo_hashed() {
    if !supports_mod_show_repo() {
        return;
    }
    let workspace = copy_workspace("same_symbol_extension_failure");
    let query = std::process::Command::new(bazel())
        .args(["query", "deps(//:uses_healthy)"])
        .current_dir(workspace.path())
        .output()
        .expect("run bazel query");
    assert!(
        query.status.success(),
        "the healthy repository must remain loadable: {}",
        String::from_utf8_lossy(&query.stderr)
    );
    assert!(String::from_utf8_lossy(&query.stdout).contains("healthy_repo//:x"));

    for mode in ["--keep_going", "--no-keep_going"] {
        let output = workspace.path().join(format!("{mode}.json"));
        let result = generate(
            workspace.path(),
            &output,
            &[mode, "--excludeExternalTargets", "--includeTargetType"],
        );
        assert!(
            result.status.success(),
            "generate-hashes {mode} must not fail"
        );
        assert!(
            hashes(&output).contains_key("//external:healthy_repo"),
            "{mode} must hash the healthy repository"
        );
    }
}

/// Reproducer for https://github.com/Tinder/bazel-diff/issues/535: the root maps the apparent
/// name `dep` to a repository rule (`+_repo_rules+dep`) and, under the apparent name
/// `module_dep`, also depends on a *module* named `dep`, whose directory is `external/dep+`.
/// `//:consumer` reads `@dep` only. Fine-grained hashing guessed the repository's directory
/// from its apparent name before consulting `bazel mod dump_repo_mapping`, so as soon as
/// anything materialized the unrelated `external/dep+` -- here a query of `@module_dep` --
/// `@dep//:data` was hashed from the wrong checkout and `//:consumer` flipped on edits that
/// cannot reach it.
#[test]
fn fine_grained_repo_rule_is_not_shadowed_by_a_same_named_module() {
    let workspace = copy_workspace("same_name_module_and_repo_rule");
    let args = ["--fineGrainedHashExternalRepos", "@dep"];
    let hash = |name: &str| {
        let output = workspace.path().join(name);
        assert!(generate(workspace.path(), &output, &args).status.success());
        output
    };
    let impacted_between = |from: &Path, to: &Path| {
        let result = workspace.path().join("impacted.txt");
        assert!(impacted(workspace.path(), from, to, &result, &[])
            .status
            .success());
        read_lines(&result)
    };

    // Nothing in //... reaches @module_dep, so the first hash is taken before Bazel has
    // materialized `external/dep+`, exactly as the reporter's first measurement was.
    let before = hash("before.json");
    let query = std::process::Command::new(bazel())
        .args(["query", "@module_dep//:data"])
        .current_dir(workspace.path())
        .output()
        .expect("run bazel query");
    assert!(
        query.status.success(),
        "{}",
        String::from_utf8_lossy(&query.stderr)
    );
    let materialized = hash("materialized.json");
    assert_eq!(
        impacted_between(&before, &materialized),
        BTreeSet::new(),
        "materializing the unrelated module must not change any hash"
    );

    // With both directories present: an edit inside the module is invisible to //:consumer...
    edit(&workspace.path().join("shadow/value.txt"), |text| {
        text + "edited\n"
    });
    let shadow_edited = hash("shadow-edited.json");
    assert_eq!(
        impacted_between(&materialized, &shadow_edited),
        BTreeSet::new()
    );

    // ...and an edit inside the repository rule's checkout reaches it.
    edit(&workspace.path().join("primary/value.txt"), |text| {
        text + "edited\n"
    });
    let primary_edited = hash("primary-edited.json");
    let impacted = impacted_between(&shadow_edited, &primary_edited);
    assert!(
        impacted.contains("//:consumer") && impacted.contains("@dep//:data"),
        "{impacted:?}"
    );
}

/// Removes the `keep/LibraryTests` package from `workspace` and edits `keep/keep.sh`, the
/// source both that package and `//consumer` depend on. The edit is what makes the diff
/// non-empty: at the "to" revision `//keep` and `//consumer` are impacted, and
/// `//keep/LibraryTests` would have been too, had it still existed.
fn delete_library_tests_package(workspace: &Path) {
    fs::remove_dir_all(workspace.join("keep/LibraryTests")).unwrap();
    edit(&workspace.join("keep/keep.sh"), |text| {
        text.replace("echo keep", "echo keep v2")
    });
}

/// The impacted labels every path must report for [`delete_library_tests_package`]: the
/// edited source, its rule, and the one surviving dependent. Nothing under
/// `//keep/LibraryTests`.
fn expected_after_deletion() -> BTreeSet<String> {
    BTreeSet::from([
        "//consumer:consumer".to_owned(),
        "//keep:keep".to_owned(),
        "//keep:keep.sh".to_owned(),
    ])
}

fn assert_no_deleted_labels<'a>(labels: impl IntoIterator<Item = &'a str>, context: &str) {
    let stale = labels
        .into_iter()
        .filter(|label| label.starts_with("//keep/LibraryTests"))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "{context}: labels from the deleted package were reported as impacted: {stale:?}"
    );
}

/// Feeds `labels` to `bazel query --query_file` at `workspace`, the way a CI consumer turns an
/// impacted-targets file into a target set. A single label whose package no longer exists
/// fails the whole query with `no such package`, which is the failure the report describes.
fn assert_labels_resolve_at(workspace: &Path, labels: &BTreeSet<String>) {
    let query_file = workspace.join("impacted.query");
    fs::write(
        &query_file,
        format!(
            "set({})\n",
            labels.iter().cloned().collect::<Vec<_>>().join(" ")
        ),
    )
    .unwrap();
    let query = std::process::Command::new(bazel())
        .args(["query", &format!("--query_file={}", query_file.display())])
        .current_dir(workspace)
        .output()
        .expect("run bazel query");
    assert!(
        query.status.success(),
        "bazel query over the impacted labels failed at the \"to\" revision: {}",
        String::from_utf8_lossy(&query.stderr)
    );
    let resolved = String::from_utf8_lossy(&query.stdout)
        .lines()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    assert_eq!(&resolved, labels);
}

/// A package deleted between the two revisions (`keep/LibraryTests`, a leaf test package that
/// depends on the edited `//keep`) must not appear in `get-impacted-targets` output, with or
/// without distances, and the labels that are reported must all resolve with `bazel query`
/// at the "to" revision. Reported against a precomputed impacted-targets file whose labels
/// named `.../LibraryTests` packages the consumer's checkout no longer had, so its
/// `bazel query --query_file` aborted with `no such package` and discarded all 33k labels.
#[test]
fn deleted_package_labels_are_not_impacted() {
    let first = copy_workspace("deleted_package");
    let second = copy_workspace("deleted_package");
    delete_library_tests_package(second.path());

    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let deps = output.path().join("deps.json");
    assert!(generate(first.path(), &from, &["--includeTargetType"])
        .status
        .success());
    assert!(generate(
        second.path(),
        &to,
        &["--includeTargetType", "-d", deps.to_str().unwrap()],
    )
    .status
    .success());
    // The deleted package is in the "from" hashes and only there: that is the input shape the
    // report suspects of producing stale labels.
    let from_hashes = hashes(&from);
    let to_hashes = hashes(&to);
    assert!(from_hashes.contains_key("//keep/LibraryTests:keep_tests"));
    assert!(!to_hashes.contains_key("//keep/LibraryTests:keep_tests"));

    let result = output.path().join("impacted.txt");
    assert!(impacted(second.path(), &from, &to, &result, &[])
        .status
        .success());
    let labels = read_lines(&result);
    assert_no_deleted_labels(labels.iter().map(String::as_str), "get-impacted-targets");
    assert_eq!(labels, expected_after_deletion());

    let distances = output.path().join("impacted.json");
    assert!(impacted(
        second.path(),
        &from,
        &to,
        &distances,
        &["-d", deps.to_str().unwrap()],
    )
    .status
    .success());
    let entries: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&distances).unwrap()).unwrap();
    let distance_labels = entries
        .iter()
        .map(|entry| entry["label"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_no_deleted_labels(
        distance_labels.iter().map(String::as_str),
        "get-impacted-targets --depEdgesFile",
    );
    assert_eq!(distance_labels, expected_after_deletion());

    assert_labels_resolve_at(second.path(), &labels);
}

/// The same deletion through `serve`, whose answers are what a precompute consumer fetches:
/// the default mode and `--partialGraph` must both leave the deleted package out, and the
/// partial mode must fall back to a full generation for the deletion rather than deriving
/// the "to" hashes from a base that still contains the package.
#[test]
fn serve_deleted_package_labels_are_not_impacted() {
    let workspace = copy_workspace("deleted_package");
    let from = init_git(workspace.path());
    delete_library_tests_package(workspace.path());
    let to = commit(
        workspace.path(),
        "delete keep/LibraryTests and edit keep.sh",
    );

    let query = |server: &ServerProcess, path: &str| -> serde_json::Value {
        let (status, body) = server.get(path).unwrap();
        assert_eq!(status, 200, "{path}: {body}");
        serde_json::from_str(&body).unwrap()
    };
    let labels_of = |body: &serde_json::Value| -> BTreeSet<String> {
        body["impactedTargets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|label| {
                label
                    .as_str()
                    .or_else(|| label["label"].as_str())
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };

    {
        let cache = tempfile::tempdir().unwrap();
        let server = ServerProcess::start(workspace.path(), cache.path(), &["--trackDeps"]);
        let labels = labels_of(&query(
            &server,
            &format!("/impacted_targets?from={from}&to={to}"),
        ));
        assert_no_deleted_labels(labels.iter().map(String::as_str), "serve /impacted_targets");
        assert_eq!(labels, expected_after_deletion());
        let labels = labels_of(&query(
            &server,
            &format!("/impacted_targets_with_distances?from={from}&to={to}"),
        ));
        assert_no_deleted_labels(
            labels.iter().map(String::as_str),
            "serve /impacted_targets_with_distances",
        );
        assert_eq!(labels, expected_after_deletion());
    }
    {
        let cache = tempfile::tempdir().unwrap();
        let server = ServerProcess::start(workspace.path(), cache.path(), &["--partialGraph"]);
        let body = query(
            &server,
            &format!("/impacted_targets?from={from}&to={to}&profile=true"),
        );
        let labels = labels_of(&body);
        assert_no_deleted_labels(
            labels.iter().map(String::as_str),
            "serve --partialGraph /impacted_targets",
        );
        assert_eq!(labels, expected_after_deletion());
        let generation = &body["profile"]["hashRetrievals"][1]["generation"];
        assert_eq!(generation["mode"], "full", "{body}");
        assert!(
            generation["partialFallbackReason"]
                .as_str()
                .unwrap()
                .contains("deleted"),
            "{body}"
        );
    }

    // The serve process left the working tree at `to`; the labels it reported must resolve there.
    assert_eq!(git(workspace.path(), &["rev-parse", "HEAD"]), to);
    assert_labels_resolve_at(workspace.path(), &expected_after_deletion());
}

/// The mechanism that can put a deleted package's labels into a consumer's hands: `serve`
/// switches revisions with `git checkout --force`, which leaves untracked and ignored files in
/// place, so a `BUILD` file left behind in the working tree (a generator's leftovers) is loaded
/// by `bazel query` at every revision. The generation must fail and name the package instead
/// of caching it under a revision that does not contain it; `--allowUntrackedPackages` restores
/// the old behaviour, and shows the stale labels that behaviour reports.
#[test]
fn serve_rejects_packages_the_revision_does_not_track() {
    let workspace = copy_workspace("deleted_package");
    let from = init_git(workspace.path());
    delete_library_tests_package(workspace.path());
    let to = commit(
        workspace.path(),
        "delete keep/LibraryTests and edit keep.sh",
    );
    // Present in the working tree, tracked by neither revision.
    let stale = workspace.path().join("keep/StaleTests");
    fs::create_dir_all(&stale).unwrap();
    fs::write(
        stale.join("BUILD"),
        "load(\"@rules_shell//shell:sh_test.bzl\", \"sh_test\")\n\
         sh_test(name = \"stale_tests\", srcs = [\"stale_tests.sh\"], deps = [\"//keep\"])\n",
    )
    .unwrap();
    fs::write(stale.join("stale_tests.sh"), "#!/bin/bash\nexit 0\n").unwrap();

    {
        let cache = tempfile::tempdir().unwrap();
        let server = ServerProcess::start(workspace.path(), cache.path(), &[]);
        let (status, body) = server
            .get(&format!("/impacted_targets?from={from}&to={to}"))
            .unwrap();
        assert_ne!(status, 200, "{body}");
        assert!(
            body.contains("//keep/StaleTests") && body.contains("--allowUntrackedPackages"),
            "{body}"
        );
        assert!(
            fs::read_dir(cache.path())
                .unwrap()
                .flatten()
                .all(|entry| entry.path().extension().is_none_or(|ext| ext != "json")),
            "a rejected generation must not be cached"
        );
        let (status, body) = server.get("/metrics").unwrap();
        assert_eq!(status, 200);
        let metrics: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(metrics["untrackedPackages"]["rejections"], 1, "{body}");
    }
    {
        let cache = tempfile::tempdir().unwrap();
        let server = ServerProcess::start(
            workspace.path(),
            cache.path(),
            &["--allowUntrackedPackages=//keep"],
        );
        let (status, body) = server
            .get(&format!("/impacted_targets?from={from}&to={to}"))
            .unwrap();
        assert_ne!(status, 200, "{body}");
        assert!(body.contains("//keep/StaleTests"), "{body}");
    }
    {
        let cache = tempfile::tempdir().unwrap();
        let server = ServerProcess::start(
            workspace.path(),
            cache.path(),
            &["--allowUntrackedPackages=//keep/StaleTests"],
        );
        let (status, body) = server
            .get(&format!("/impacted_targets?from={from}&to={to}"))
            .unwrap();
        assert_eq!(status, 200, "{body}");
    }
    {
        let cache = tempfile::tempdir().unwrap();
        let server = ServerProcess::start(
            workspace.path(),
            cache.path(),
            &["--allowUntrackedPackages"],
        );
        let (status, body) = server
            .get(&format!("/impacted_targets?from={from}&to={to}"))
            .unwrap();
        assert_eq!(status, 200, "{body}");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        let labels = body["impactedTargets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|label| label.as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>();
        // The report's symptom: a label from a package no checkout of `to` has.
        assert!(
            labels.contains("//keep/StaleTests:stale_tests"),
            "{labels:?}"
        );
        assert!(labels.is_superset(&expected_after_deletion()), "{labels:?}");
    }
}

/// Control for <https://github.com/Tinder/bazel-diff/issues/612>: in query mode a change
/// behind a main-repository `alias` reaches the target that uses the alias.
#[test]
fn main_repo_alias_change_reaches_consumer_query() {
    let first = copy_workspace("cquery_main_repo_alias");
    let second = copy_workspace("cquery_main_repo_alias");
    fs::write(second.path().join("bar/baz.txt"), "after\n").unwrap();
    let impacted = diff_workspaces(first.path(), second.path(), &[]);
    for name in ["//bar:baz.txt", "//bar:baz", "//:tool", "//:wrapper"] {
        assert!(
            impacted
                .iter()
                .any(|label| label == name || label.trim_start_matches("@@") == name),
            "{name} should be impacted through the alias, got {impacted:?}"
        );
    }
}

/// Reproducer for <https://github.com/Tinder/bazel-diff/issues/612>: under `--useCquery`
/// the compatible-target cquery names a main-repository alias by `repr(target)`, which
/// Bazel prints with its apparent label (`//:tool`) even under `--consistent_labels`,
/// while the target itself is named canonically (`@@//:tool`). The two never match, so
/// the alias is filtered out as if it were platform-incompatible and takes its edges with
/// it: `//:wrapper` has no node for `//:tool` to depend on and is never impacted.
#[test]
fn main_repo_alias_change_reaches_consumer_cquery() {
    let first = copy_workspace("cquery_main_repo_alias");
    let second = copy_workspace("cquery_main_repo_alias");
    fs::write(second.path().join("bar/baz.txt"), "after\n").unwrap();

    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let edges_file = output.path().join("edges.json");
    let result = output.path().join("impacted.txt");
    assert!(generate(first.path(), &from, &["--useCquery"])
        .status
        .success());
    assert!(generate(
        second.path(),
        &to,
        &[
            "--useCquery",
            "--depEdgesFile",
            edges_file.to_str().unwrap()
        ],
    )
    .status
    .success());
    assert!(impacted(second.path(), &from, &to, &result, &[])
        .status
        .success());

    let hashes = hashes(&to);
    assert!(
        hashes.keys().any(|label| label.ends_with("//:tool")),
        "the alias //:tool was dropped from the cquery hashes: {:?}",
        hashes.keys().collect::<Vec<_>>()
    );
    let edges = dep_edges(&edges_file);
    let wrapper_edges = edges
        .iter()
        .find(|(label, _)| label.ends_with("//:wrapper"))
        .map(|(_, deps)| deps.clone())
        .expect("//:wrapper has dep edges");
    assert!(
        wrapper_edges.iter().any(|dep| dep.ends_with("//:tool")),
        "//:wrapper lost its edge to the alias //:tool: {wrapper_edges:?}"
    );

    let impacted = read_lines(&result);
    for name in ["//bar:baz.txt", "//bar:baz", "//:tool", "//:wrapper"] {
        assert!(
            impacted
                .iter()
                .any(|label| label == name || label.trim_start_matches("@@") == name),
            "{name} should be impacted through the alias under cquery, got {impacted:?}"
        );
    }
}

/// Follow-up to <https://github.com/Tinder/bazel-diff/issues/612> for WORKSPACE mode: there
/// `--consistent_labels` prints main-repository targets as `@//:tool`, not the Bzlmod
/// `@@//:tool`, so prefixing the alias's apparent label with `@@` still left it unmatched,
/// dropped as if it were platform-incompatible together with every edge through it.
#[test]
fn main_repo_alias_change_reaches_consumer_cquery_workspace_mode() {
    let first = copy_workspace("cquery_main_repo_alias_workspace");
    let second = copy_workspace("cquery_main_repo_alias_workspace");
    fs::write(second.path().join("bar/baz.txt"), "after\n").unwrap();

    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let edges_file = output.path().join("edges.json");
    let result = output.path().join("impacted.txt");
    assert!(generate(first.path(), &from, &["--useCquery"])
        .status
        .success());
    assert!(generate(
        second.path(),
        &to,
        &[
            "--useCquery",
            "--depEdgesFile",
            edges_file.to_str().unwrap()
        ],
    )
    .status
    .success());
    assert!(impacted(second.path(), &from, &to, &result, &[])
        .status
        .success());

    let hashes = hashes(&to);
    assert!(
        hashes.keys().any(|label| label.ends_with("//:tool")),
        "the alias //:tool was dropped from the cquery hashes: {:?}",
        hashes.keys().collect::<Vec<_>>()
    );
    let edges = dep_edges(&edges_file);
    let wrapper_edges = edges
        .iter()
        .find(|(label, _)| label.ends_with("//:wrapper"))
        .map(|(_, deps)| deps.clone())
        .expect("//:wrapper has dep edges");
    assert!(
        wrapper_edges.iter().any(|dep| dep.ends_with("//:tool")),
        "//:wrapper lost its edge to the alias //:tool: {wrapper_edges:?}"
    );

    let impacted = read_lines(&result);
    for name in ["//bar:baz.txt", "//bar:baz", "//:tool", "//:wrapper"] {
        assert!(
            impacted
                .iter()
                .any(|label| label == name || label.trim_start_matches('@') == name),
            "{name} should be impacted through the alias under cquery, got {impacted:?}"
        );
    }
}
