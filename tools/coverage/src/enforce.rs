//! Per-target line-coverage enforcement.
//!
//! The merger runs *inside* the test action (Bazel only spawns it for
//! `bazel coverage`, never for `bazel test`), so it inherits the test
//! target's `env` attribute. That is the channel targets use to opt in:
//!
//! - `LCOV_MERGER_MIN_LINE_COVERAGE`: minimum overall line coverage
//!   percentage (0-100) for the target's merged report. Presence of this
//!   variable is what activates enforcement.
//! - `LCOV_MERGER_COVERAGE_INCLUDE`: optional comma-separated path prefixes;
//!   when set, only `SF:` paths starting with one of them count.
//! - `LCOV_MERGER_COVERAGE_EXCLUDE`: optional comma-separated path prefixes
//!   removed from the computation (applied after includes).
//!
//! `//tools/coverage:defs.bzl` provides a macro that injects these for any
//! test rule; nothing here is rule- or language-specific.

use crate::lcov::Report;
use std::collections::BTreeMap;
use std::fmt::Write as _;

pub const MIN_LINE_COVERAGE_ENV: &str = "LCOV_MERGER_MIN_LINE_COVERAGE";
pub const COVERAGE_INCLUDE_ENV: &str = "LCOV_MERGER_COVERAGE_INCLUDE";
pub const COVERAGE_EXCLUDE_ENV: &str = "LCOV_MERGER_COVERAGE_EXCLUDE";

/// Enforcement configuration, parsed from the test action's environment.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub min_line_coverage: f64,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// Read enforcement config from an environment map. Returns:
/// - `Ok(None)` when enforcement is not requested (no min set),
/// - `Ok(Some(config))` when it is,
/// - `Err` when the variables are present but unusable — that is a bug in
///   the target's BUILD file and must fail the coverage action loudly.
pub fn config_from_env(env: &BTreeMap<String, String>) -> Result<Option<Config>, String> {
    let Some(raw) = env.get(MIN_LINE_COVERAGE_ENV) else {
        return Ok(None);
    };
    let min: f64 = raw.trim().parse().map_err(|_| {
        format!("{MIN_LINE_COVERAGE_ENV}='{raw}' is not a number (expected 0-100)")
    })?;
    if !(0.0..=100.0).contains(&min) {
        return Err(format!(
            "{MIN_LINE_COVERAGE_ENV}={min} is outside the valid range 0-100"
        ));
    }
    let split = |key: &str| -> Vec<String> {
        env.get(key)
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(Some(Config {
        min_line_coverage: min,
        include: split(COVERAGE_INCLUDE_ENV),
        exclude: split(COVERAGE_EXCLUDE_ENV),
    }))
}

/// The outcome of an enforcement check. `report` is always populated so the
/// per-file breakdown lands in the test log for passing runs too.
#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub passed: bool,
    pub report: String,
}

/// Check the merged report against `config`.
pub fn check(report: &Report, config: &Config) -> Outcome {
    let in_scope = |path: &str| -> bool {
        (config.include.is_empty() || config.include.iter().any(|p| path.starts_with(p)))
            && !config.exclude.iter().any(|p| path.starts_with(p))
    };

    let mut rows: Vec<(f64, u64, u64, &str)> = Vec::new();
    let mut total_found = 0u64;
    let mut total_hit = 0u64;
    for (path, cov) in report {
        if !in_scope(path) {
            continue;
        }
        let found = cov.lines_found();
        // Skip files with no instrumented lines: LCOV has nothing measurable
        // for them, and counting them as 0% would punish files the language's
        // instrumentation simply did not visit.
        if found == 0 {
            continue;
        }
        let hit = cov.lines_hit();
        total_found += found;
        total_hit += hit;
        rows.push((hit as f64 / found as f64 * 100.0, hit, found, path));
    }

    let mut text = String::new();
    let _ = writeln!(text, "{:>8}  {:>17}  FILE", "COV%", "LINES (hit/total)");
    rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.3.cmp(b.3)));
    for (pct, hit, found, path) in &rows {
        let _ = writeln!(text, "{pct:>7.2}%  {hit:>7} / {found:<7}  {path}");
    }

    if total_found == 0 {
        let _ = writeln!(
            text,
            "\nNo instrumented lines matched the coverage scope (include={:?}, exclude={:?}).",
            config.include, config.exclude
        );
        let _ = writeln!(
            text,
            "A minimum of {:.2}% was requested, so this fails: either the scope is wrong or \
             coverage instrumentation did not run for this target.",
            config.min_line_coverage
        );
        return Outcome {
            passed: false,
            report: text,
        };
    }

    let overall = total_hit as f64 / total_found as f64 * 100.0;
    let _ = writeln!(
        text,
        "\nTarget line coverage: {overall:.2}% ({total_hit} / {total_found} lines)"
    );
    let _ = writeln!(
        text,
        "Required minimum:     {:.2}%",
        config.min_line_coverage
    );
    // Epsilon so a value that only misses the threshold by floating-point
    // noise (e.g. 89.999999999) still passes a 90 minimum.
    let passed = overall + 1e-9 >= config.min_line_coverage;
    let _ = writeln!(
        text,
        "{}",
        if passed {
            "PASS: coverage minimum satisfied.".to_string()
        } else {
            format!(
                "FAIL: line coverage {overall:.2}% is below the required minimum {:.2}%.",
                config.min_line_coverage
            )
        }
    );
    Outcome {
        passed,
        report: text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lcov::parse;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn no_env_means_no_enforcement() {
        assert_eq!(config_from_env(&BTreeMap::new()), Ok(None));
        // Include/exclude alone do not activate enforcement.
        assert_eq!(
            config_from_env(&env(&[(COVERAGE_INCLUDE_ENV, "src/")])),
            Ok(None)
        );
    }

    #[test]
    fn parses_full_config() {
        let cfg = config_from_env(&env(&[
            (MIN_LINE_COVERAGE_ENV, "87.5"),
            (COVERAGE_INCLUDE_ENV, "src/, lib/ ,"),
            (COVERAGE_EXCLUDE_ENV, "src/generated/"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(cfg.min_line_coverage, 87.5);
        assert_eq!(cfg.include, vec!["src/", "lib/"]);
        assert_eq!(cfg.exclude, vec!["src/generated/"]);
    }

    #[test]
    fn rejects_malformed_minimums() {
        assert!(config_from_env(&env(&[(MIN_LINE_COVERAGE_ENV, "ninety")])).is_err());
        assert!(config_from_env(&env(&[(MIN_LINE_COVERAGE_ENV, "101")])).is_err());
        assert!(config_from_env(&env(&[(MIN_LINE_COVERAGE_ENV, "-1")])).is_err());
    }

    fn sample_report() -> Report {
        let (report, _) = parse(concat!(
            "SF:src/a.rs\nDA:1,1\nDA:2,1\nDA:3,0\nDA:4,1\nend_of_record\n",
            "SF:src/b.rs\nDA:1,0\nDA:2,0\nend_of_record\n",
            "SF:tests/t.rs\nDA:1,1\nend_of_record\n",
            "SF:src/empty.rs\nend_of_record\n",
        ));
        report
    }

    fn cfg(min: f64, include: &[&str], exclude: &[&str]) -> Config {
        Config {
            min_line_coverage: min,
            include: include.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn passes_at_or_above_the_minimum() {
        // src/ scope: a.rs 3/4 + b.rs 0/2 = 3/6 lines = 50%.
        let outcome = check(&sample_report(), &cfg(49.5, &["src/"], &[]));
        assert!(outcome.passed, "{}", outcome.report);
        assert!(outcome.report.contains("Target line coverage: 50.00%"));
        assert!(outcome.report.contains("PASS"));
        // Threshold exactly at the measured value passes (epsilon).
        assert!(check(&sample_report(), &cfg(50.0, &["src/"], &[])).passed);
    }

    #[test]
    fn fails_below_the_minimum_with_per_file_breakdown() {
        let outcome = check(&sample_report(), &cfg(90.0, &["src/"], &[]));
        assert!(!outcome.passed);
        assert!(outcome.report.contains("FAIL: line coverage 50.00%"));
        // Worst file sorts first; the zero-line file is skipped entirely.
        assert!(outcome.report.contains("src/b.rs"));
        assert!(!outcome.report.contains("src/empty.rs"));
        assert!(!outcome.report.contains("tests/t.rs"));
    }

    #[test]
    fn exclude_prefixes_narrow_the_scope() {
        // Excluding the all-miss file lifts coverage to 3/4 = 75%.
        let outcome = check(&sample_report(), &cfg(75.0, &["src/"], &["src/b.rs"]));
        assert!(outcome.passed, "{}", outcome.report);
        // No include: everything counts (4/7 = 57.14%).
        let outcome = check(&sample_report(), &cfg(57.0, &[], &[]));
        assert!(outcome.passed, "{}", outcome.report);
        assert!(outcome.report.contains("tests/t.rs"));
    }

    #[test]
    fn empty_scope_fails_loudly() {
        let outcome = check(&sample_report(), &cfg(50.0, &["nonexistent/"], &[]));
        assert!(!outcome.passed);
        assert!(outcome.report.contains("No instrumented lines matched"));
    }
}
