use crate::support::*;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

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
        workspace.join("registry/modules/dep/1.0/source.json"),
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
    assert!(!impacted.contains("//:control"), "{impacted:?}");
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
