//! bazel-diff's LCOV merger: a drop-in replacement for Bazel's built-in
//! `@bazel_tools//tools/test:lcov_merger` (wired up via
//! `coverage --coverage_output_generator=//tools/coverage:lcov_merger` in
//! `.bazelrc`) that can additionally enforce a per-target line-coverage
//! minimum.
//!
//! Bazel only invokes this binary during `bazel coverage` — plain
//! `bazel test` runs never see it — so enforcement engages exclusively in
//! coverage runs. See `README.md` in this package for the user-facing story.

pub mod args;
pub mod enforce;
pub mod lcov;
pub mod pattern;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Exit code for a target that produced coverage below its declared minimum.
/// Distinct from `1` (operational failure) so the two are distinguishable in
/// test logs.
pub const EXIT_BELOW_MINIMUM: i32 = 33;

/// Entry point shared by `main` and the integration tests. Returns the
/// process exit code; all diagnostics go to `log` (stderr in production,
/// captured buffers in tests).
pub fn run(argv: &[String], env: &BTreeMap<String, String>, log: &mut dyn Write) -> i32 {
    let args = match args::parse(argv) {
        Ok(args) => args,
        Err(msg) => {
            let _ = writeln!(log, "lcov_merger: error: {msg}");
            return 1;
        }
    };
    for unknown in &args.unknown {
        let _ = writeln!(log, "lcov_merger: warning: ignoring unknown flag {unknown}");
    }

    let coverage_dir = args.coverage_dir.as_deref().unwrap();
    let output_file = args.output_file.as_deref().unwrap();

    // 1. Merge every LCOV tracefile found under the coverage directory.
    let mut report = lcov::Report::new();
    for path in find_tracefiles(Path::new(coverage_dir), log) {
        let text = match fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(err) => {
                let _ = writeln!(
                    log,
                    "lcov_merger: warning: skipping unreadable tracefile {}: {err}",
                    path.display()
                );
                continue;
            }
        };
        let (parsed, warnings) = lcov::parse(&text);
        for warning in warnings {
            let _ = writeln!(log, "lcov_merger: warning: {}: {warning}", path.display());
        }
        lcov::merge_reports(&mut report, parsed);
    }

    // 2. Drop sources matching the --filter_sources patterns (system paths
    //    like /usr/bin/.+ in Bazel's default invocation).
    for source in &args.filter_sources {
        match pattern::Pattern::compile(source) {
            Ok(pattern) => report.retain(|path, _| !pattern.matches(path)),
            Err(msg) => {
                let _ = writeln!(
                    log,
                    "lcov_merger: warning: ignoring --filter_sources pattern: {msg}"
                );
            }
        }
    }

    // 3. Restrict to the sources Bazel declared as instrumented, mirroring
    //    the built-in merger: manifest entries that are not instrumentation
    //    metadata (.gcno/.em) name the files the target owns. This is what
    //    keeps test sources themselves out of the merged report.
    if let Some(manifest_path) = args.source_file_manifest.as_deref() {
        match fs::read_to_string(manifest_path) {
            Ok(manifest) => {
                let sources: std::collections::BTreeSet<&str> = manifest
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.ends_with(".gcno") && !l.ends_with(".em"))
                    .collect();
                if !sources.is_empty() {
                    let before = report.len();
                    report.retain(|path, _| sources.contains(path.as_str()));
                    if report.is_empty() && before > 0 {
                        let _ = writeln!(
                            log,
                            "lcov_merger: warning: the source file manifest {manifest_path} \
                             matched none of the {before} covered file(s); the merged report \
                             is empty. Tracefile paths and manifest paths likely disagree."
                        );
                    }
                }
            }
            Err(err) => {
                let _ = writeln!(
                    log,
                    "lcov_merger: warning: cannot read source file manifest {manifest_path}: {err}"
                );
            }
        }
    }

    // 4. Publish the merged tracefile — always, even when enforcement fails
    //    below, so the report is available to debug the failure.
    if let Err(err) = fs::write(output_file, lcov::emit(&report)) {
        let _ = writeln!(
            log,
            "lcov_merger: error: cannot write merged report to {output_file}: {err}"
        );
        return 1;
    }

    // 5. Enforce the target's coverage minimum, if it declared one.
    match enforce::config_from_env(env) {
        Ok(None) => 0,
        Ok(Some(config)) => {
            let outcome = enforce::check(&report, &config);
            let _ = writeln!(
                log,
                "lcov_merger: line-coverage minimum declared for this target \
                 (min {:.2}%):\n\n{}",
                config.min_line_coverage, outcome.report
            );
            if outcome.passed {
                0
            } else {
                EXIT_BELOW_MINIMUM
            }
        }
        Err(msg) => {
            let _ = writeln!(log, "lcov_merger: error: {msg}");
            1
        }
    }
}

/// Recursively collect LCOV tracefiles (`*.dat` / `*.info`) under `dir`,
/// sorted for deterministic merge order. Other files (e.g. `.profraw`
/// leftovers or gcov intermediates) are ignored: every language rule this
/// repository uses converts to LCOV before the merger runs.
fn find_tracefiles(dir: &Path, log: &mut dyn Write) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(err) => {
                let _ = writeln!(
                    log,
                    "lcov_merger: warning: cannot list {}: {err}",
                    current.display()
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.ends_with(".dat") || name.ends_with(".info") {
                    found.push(path);
                }
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique scratch directory per test, under TEST_TMPDIR when Bazel
    /// provides it.
    fn scratch_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var("TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let dir = base.join(format!(
            "lcov_merger_test_{}_{label}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct Invocation {
        code: i32,
        log: String,
        merged: String,
    }

    /// Drive `run()` against a synthetic COVERAGE_DIR the way
    /// collect_coverage.sh would.
    fn invoke(
        label: &str,
        tracefiles: &[(&str, &str)],
        manifest: Option<&str>,
        env: &[(&str, &str)],
        extra_args: &[&str],
    ) -> Invocation {
        let dir = scratch_dir(label);
        let coverage_dir = dir.join("coverage");
        fs::create_dir_all(coverage_dir.join("nested")).unwrap();
        for (name, content) in tracefiles {
            fs::write(coverage_dir.join(name), content).unwrap();
        }
        let output_file = dir.join("coverage.dat");
        let mut argv = vec![
            format!("--coverage_dir={}", coverage_dir.display()),
            format!("--output_file={}", output_file.display()),
        ];
        if let Some(manifest_content) = manifest {
            let manifest_path = dir.join("manifest.txt");
            fs::write(&manifest_path, manifest_content).unwrap();
            argv.push(format!(
                "--source_file_manifest={}",
                manifest_path.display()
            ));
        }
        argv.extend(extra_args.iter().map(|s| s.to_string()));
        let env: BTreeMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut log = Vec::new();
        let code = run(&argv, &env, &mut log);
        Invocation {
            code,
            log: String::from_utf8(log).unwrap(),
            merged: fs::read_to_string(&output_file).unwrap_or_default(),
        }
    }

    #[test]
    fn merges_multiple_runner_tracefiles() {
        let result = invoke(
            "merge",
            &[
                ("a.dat", "SF:pkg/lib.go\nDA:1,1\nDA:2,0\nend_of_record\n"),
                ("nested/b.dat", "SF:pkg/lib.go\nDA:2,3\nend_of_record\n"),
                ("ignored.profraw", "not lcov"),
            ],
            None,
            &[],
            &[],
        );
        assert_eq!(result.code, 0, "{}", result.log);
        assert_eq!(
            result.merged,
            "SF:pkg/lib.go\nDA:1,1\nDA:2,3\nLH:2\nLF:2\nend_of_record\n"
        );
    }

    #[test]
    fn filters_system_sources_and_ignores_bad_patterns() {
        let result = invoke(
            "filters",
            &[(
                "a.dat",
                "SF:/usr/bin/tool\nDA:1,1\nend_of_record\nSF:pkg/lib.go\nDA:1,1\nend_of_record\n",
            )],
            None,
            &[],
            &["--filter_sources=/usr/bin/.+", "--filter_sources=a|b"],
        );
        assert_eq!(result.code, 0);
        assert!(!result.merged.contains("/usr/bin/tool"));
        assert!(result.merged.contains("SF:pkg/lib.go"));
        assert!(result.log.contains("ignoring --filter_sources pattern"));
    }

    #[test]
    fn manifest_restricts_to_instrumented_sources() {
        let result = invoke(
            "manifest",
            &[(
                "a.dat",
                "SF:pkg/lib.go\nDA:1,1\nend_of_record\nSF:pkg/lib_test.go\nDA:1,1\nend_of_record\n",
            )],
            Some("pkg/lib.go\npkg/meta.gcno\npkg/other.em\n"),
            &[],
            &[],
        );
        assert_eq!(result.code, 0);
        assert!(result.merged.contains("SF:pkg/lib.go"));
        assert!(!result.merged.contains("lib_test.go"));
    }

    #[test]
    fn metadata_only_manifest_does_not_restrict() {
        let result = invoke(
            "manifest_meta",
            &[("a.dat", "SF:pkg/lib.cc\nDA:1,1\nend_of_record\n")],
            Some("pkg/meta.gcno\n"),
            &[],
            &[],
        );
        assert_eq!(result.code, 0);
        assert!(result.merged.contains("SF:pkg/lib.cc"));
    }

    #[test]
    fn mismatched_manifest_warns_about_empty_report() {
        let result = invoke(
            "manifest_mismatch",
            &[("a.dat", "SF:pkg/lib.go\nDA:1,1\nend_of_record\n")],
            Some("other/file.go\n"),
            &[],
            &[],
        );
        assert_eq!(result.code, 0);
        assert_eq!(result.merged, "");
        assert!(result.log.contains("matched none"));
    }

    #[test]
    fn enforcement_passes_and_reports_in_the_log() {
        let result = invoke(
            "enforce_pass",
            &[(
                "a.dat",
                "SF:pkg/lib.go\nDA:1,1\nDA:2,1\nDA:3,0\nend_of_record\n",
            )],
            None,
            &[(enforce::MIN_LINE_COVERAGE_ENV, "60")],
            &[],
        );
        assert_eq!(result.code, 0, "{}", result.log);
        assert!(result.log.contains("Target line coverage: 66.67%"));
        assert!(result.log.contains("PASS"));
    }

    #[test]
    fn enforcement_fails_with_distinct_exit_code() {
        let result = invoke(
            "enforce_fail",
            &[(
                "a.dat",
                "SF:pkg/lib.go\nDA:1,1\nDA:2,0\nDA:3,0\nend_of_record\n",
            )],
            None,
            &[
                (enforce::MIN_LINE_COVERAGE_ENV, "90"),
                (enforce::COVERAGE_INCLUDE_ENV, "pkg/"),
            ],
            &[],
        );
        assert_eq!(result.code, EXIT_BELOW_MINIMUM);
        assert!(result.log.contains("FAIL: line coverage 33.33%"));
        // The merged report is still written for debugging.
        assert!(result.merged.contains("SF:pkg/lib.go"));
    }

    #[test]
    fn enforcement_with_no_coverage_data_fails() {
        let result = invoke(
            "enforce_empty",
            &[],
            None,
            &[(enforce::MIN_LINE_COVERAGE_ENV, "1")],
            &[],
        );
        assert_eq!(result.code, EXIT_BELOW_MINIMUM);
        assert!(result.log.contains("No instrumented lines matched"));
    }

    #[test]
    fn bad_enforcement_config_is_a_hard_error() {
        let result = invoke(
            "enforce_bad",
            &[("a.dat", "SF:x\nDA:1,1\nend_of_record\n")],
            None,
            &[(enforce::MIN_LINE_COVERAGE_ENV, "lots")],
            &[],
        );
        assert_eq!(result.code, 1);
        assert!(result.log.contains("is not a number"));
    }

    #[test]
    fn no_tracefiles_still_writes_an_empty_report() {
        let result = invoke("no_data", &[], None, &[], &[]);
        assert_eq!(result.code, 0);
        assert_eq!(result.merged, "");
    }

    #[test]
    fn unknown_flags_warn_and_bad_argv_errors() {
        let result = invoke("unknown_flag", &[], None, &[], &["--future_flag=1"]);
        assert_eq!(result.code, 0);
        assert!(result.log.contains("ignoring unknown flag --future_flag=1"));

        let mut log = Vec::new();
        let code = run(&["positional".to_string()], &BTreeMap::new(), &mut log);
        assert_eq!(code, 1);
        assert!(String::from_utf8(log).unwrap().contains("error"));
    }

    #[test]
    fn unwritable_output_is_a_hard_error() {
        let dir = scratch_dir("unwritable");
        let argv = vec![
            format!("--coverage_dir={}", dir.display()),
            format!("--output_file={}/no_such_dir/out.dat", dir.display()),
        ];
        let mut log = Vec::new();
        let code = run(&argv, &BTreeMap::new(), &mut log);
        assert_eq!(code, 1);
        assert!(String::from_utf8(log).unwrap().contains("cannot write"));
    }

    #[test]
    fn missing_coverage_dir_warns_but_produces_empty_report() {
        let dir = scratch_dir("missing_dir");
        let out = dir.join("out.dat");
        let argv = vec![
            format!("--coverage_dir={}/does_not_exist", dir.display()),
            format!("--output_file={}", out.display()),
        ];
        let mut log = Vec::new();
        let code = run(&argv, &BTreeMap::new(), &mut log);
        assert_eq!(code, 0);
        assert!(String::from_utf8(log).unwrap().contains("cannot list"));
        assert_eq!(fs::read_to_string(out).unwrap(), "");
    }
}
