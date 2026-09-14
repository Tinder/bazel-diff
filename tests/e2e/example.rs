//! Runs `bazel-diff-example.sh`, the script the README's "Getting Started"
//! section hands a new user, the way that section says to: a workspace, a
//! Bazel, and two revisions.
//!
//! The script normally builds bazel-diff itself with `bazel run :bazel-diff`.
//! That step cannot happen here -- under `bazel test` the source tree is not on
//! disk, and the binary under test is already built -- so the test points the
//! script's `BAZEL_DIFF_BINARY` override at it. The build-from-source step stays
//! covered by the `example-script` CI job, which runs the script unmodified
//! against this repository.

use crate::support::*;
use std::collections::BTreeSet;
use std::fs;
use std::process::Command;

#[test]
fn example_script_reports_impacted_targets() {
    let workspace = copy_workspace("distance_metrics");
    let from = init_git(workspace.path());
    fs::write(workspace.path().join("lib.sh"), "echo changed\n").unwrap();
    let to = commit(workspace.path(), "change");
    // Park the workspace on the starting revision, as a user would be.
    git(workspace.path(), &["checkout", "--quiet", &from]);
    let output_dir = tempfile::tempdir().unwrap();

    let output = Command::new("bash")
        .arg(repo_root().join("bazel-diff-example.sh"))
        .arg(workspace.path())
        .arg(bazel())
        .arg(&from)
        .arg(&to)
        .env("BAZEL_DIFF_BINARY", binary())
        .env("BAZEL_DIFF_OUTPUT_DIR", output_dir.path())
        .output()
        .expect("run bazel-diff-example.sh");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "example script failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The script checks out the final revision and leaves the workspace there.
    assert_eq!(git(workspace.path(), &["rev-parse", "HEAD"]), to);

    // The file the script writes, and the summary it prints, must agree, and
    // both must name the target whose source changed.
    let written = read_lines(&output_dir.path().join("impacted_targets.txt"));
    let header = format!("Impacted Targets between {from} and {to}:");
    let printed = stdout
        .lines()
        .skip_while(|line| *line != header)
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    assert!(
        stdout.contains(&header),
        "summary header missing from stdout:\n{stdout}"
    );
    assert_eq!(printed, written, "stdout:\n{stdout}");
    assert!(
        written
            .iter()
            .any(|label| label.trim_start_matches("@@") == "//:lib"),
        "expected //:lib among impacted targets, got {written:?}\nstdout:\n{stdout}"
    );
}
