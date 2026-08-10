//! LCOV tracefile parsing, merging, and emission.
//!
//! Bazel's per-test coverage post-processor receives a directory of raw
//! per-runner LCOV tracefiles (Jacoco for JVM targets, rules_go's native LCOV
//! conversion for Go, llvm-cov's LCOV export for Rust) and must merge them
//! into the single tracefile Bazel publishes as the test's `coverage.dat`.
//!
//! Merging is keyed by `SF:` path. Line (`DA`) and function (`FNDA`) hit
//! counts are summed; branch (`BRDA`) "taken" counts are summed except that
//! `-` (branch never evaluated) only survives when no tracefile evaluated the
//! branch. `LF`/`LH`/`FNF`/`FNH`/`BRF`/`BRH` totals are recomputed from the
//! merged data rather than trusted from the inputs.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Branch execution state for one `BRDA` record: `None` means the branch was
/// never evaluated (`-` in LCOV), `Some(n)` means it was taken `n` times.
pub type BranchTaken = Option<u64>;

/// Coverage data for a single source file (one `SF:` record).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileCoverage {
    /// Function name -> starting line (`FN`). First definition wins.
    pub function_lines: BTreeMap<String, u64>,
    /// Function name -> execution count (`FNDA`).
    pub function_hits: BTreeMap<String, u64>,
    /// Line number -> execution count (`DA`).
    pub line_hits: BTreeMap<u64, u64>,
    /// (line, block id, branch id) -> taken count (`BRDA`).
    pub branches: BTreeMap<(u64, String, String), BranchTaken>,
}

impl FileCoverage {
    pub fn lines_found(&self) -> u64 {
        self.line_hits.len() as u64
    }

    pub fn lines_hit(&self) -> u64 {
        self.line_hits.values().filter(|&&hits| hits > 0).count() as u64
    }

    /// Fold `other` into `self`, summing hit counts.
    pub fn merge(&mut self, other: &FileCoverage) {
        for (name, line) in &other.function_lines {
            self.function_lines
                .entry(name.clone())
                .or_insert(*line);
        }
        for (name, hits) in &other.function_hits {
            *self.function_hits.entry(name.clone()).or_insert(0) += hits;
        }
        for (line, hits) in &other.line_hits {
            *self.line_hits.entry(*line).or_insert(0) += hits;
        }
        for (key, taken) in &other.branches {
            let entry = self.branches.entry(key.clone()).or_insert(None);
            *entry = match (*entry, *taken) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
            };
        }
    }
}

/// A full report: source file path -> coverage, ordered by path for
/// deterministic output.
pub type Report = BTreeMap<String, FileCoverage>;

/// Parse one LCOV tracefile. Returns the parsed report plus human-readable
/// warnings for lines that could not be understood (the record is skipped,
/// never fatal: one malformed runner output must not fail the whole merge).
pub fn parse(text: &str) -> (Report, Vec<String>) {
    let mut report = Report::new();
    let mut warnings = Vec::new();
    let mut current: Option<(String, FileCoverage)> = None;

    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some(path) = line.strip_prefix("SF:") {
            if let Some((prev_path, prev)) = current.take() {
                // Tolerate a missing end_of_record before the next SF.
                merge_into(&mut report, prev_path, prev);
            }
            current = Some((path.to_string(), FileCoverage::default()));
            continue;
        }
        if line == "end_of_record" {
            if let Some((path, cov)) = current.take() {
                merge_into(&mut report, path, cov);
            }
            continue;
        }
        let Some((_, cov)) = current.as_mut() else {
            // TN: records (and stray junk) outside an SF block are ignored.
            continue;
        };
        if let Some(rest) = line.strip_prefix("DA:") {
            match parse_da(rest) {
                Some((line_no, hits)) => {
                    *cov.line_hits.entry(line_no).or_insert(0) += hits;
                }
                None => warnings.push(format!("line {}: unparseable DA record '{line}'", idx + 1)),
            }
        } else if let Some(rest) = line.strip_prefix("FN:") {
            match rest.split_once(',') {
                // Function names may themselves contain commas (e.g. C++
                // templates), so only the first comma delimits.
                Some((line_no, name)) => match line_no.parse::<u64>() {
                    Ok(n) => {
                        cov.function_lines.entry(name.to_string()).or_insert(n);
                    }
                    Err(_) => warnings
                        .push(format!("line {}: unparseable FN record '{line}'", idx + 1)),
                },
                None => warnings.push(format!("line {}: unparseable FN record '{line}'", idx + 1)),
            }
        } else if let Some(rest) = line.strip_prefix("FNDA:") {
            match rest.split_once(',') {
                Some((hits, name)) => match hits.parse::<u64>() {
                    Ok(h) => *cov.function_hits.entry(name.to_string()).or_insert(0) += h,
                    Err(_) => warnings
                        .push(format!("line {}: unparseable FNDA record '{line}'", idx + 1)),
                },
                None => {
                    warnings.push(format!("line {}: unparseable FNDA record '{line}'", idx + 1))
                }
            }
        } else if let Some(rest) = line.strip_prefix("BRDA:") {
            match parse_brda(rest) {
                Some((key, taken)) => {
                    let entry = cov.branches.entry(key).or_insert(None);
                    *entry = match (*entry, taken) {
                        (None, None) => None,
                        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
                    };
                }
                None => {
                    warnings.push(format!("line {}: unparseable BRDA record '{line}'", idx + 1))
                }
            }
        }
        // LF/LH/FNF/FNH/BRF/BRH are recomputed on output; TN and anything
        // else (e.g. lcov 2.x extensions) is intentionally ignored.
    }
    if let Some((path, cov)) = current.take() {
        merge_into(&mut report, path, cov);
    }
    (report, warnings)
}

fn merge_into(report: &mut Report, path: String, cov: FileCoverage) {
    report.entry(path).or_default().merge(&cov);
}

/// `DA:<line>,<hits>[,<checksum>]`
fn parse_da(rest: &str) -> Option<(u64, u64)> {
    let mut parts = rest.splitn(3, ',');
    let line_no = parts.next()?.parse().ok()?;
    let hits = parts.next()?.parse().ok()?;
    Some((line_no, hits))
}

/// `BRDA:<line>,<block>,<branch>,<taken>` where taken is `-` or a count.
fn parse_brda(rest: &str) -> Option<((u64, String, String), BranchTaken)> {
    let mut parts = rest.splitn(4, ',');
    let line_no = parts.next()?.parse().ok()?;
    let block = parts.next()?.to_string();
    let branch = parts.next()?.to_string();
    let taken = match parts.next()? {
        "-" => None,
        n => Some(n.parse().ok()?),
    };
    Some(((line_no, block, branch), taken))
}

/// Merge `incoming` into `merged`.
pub fn merge_reports(merged: &mut Report, incoming: Report) {
    for (path, cov) in incoming {
        merge_into(merged, path, cov);
    }
}

/// Serialise a report back to LCOV text, mirroring the record order of
/// Bazel's built-in CoverageOutputGenerator (SF, FN, FNDA, FNF, FNH, BRDA,
/// BRF, BRH, DA, LH, LF) so downstream consumers see no format change.
pub fn emit(report: &Report) -> String {
    let mut out = String::new();
    for (path, cov) in report {
        let _ = writeln!(out, "SF:{path}");
        let mut functions: Vec<(&String, &u64)> = cov.function_lines.iter().collect();
        functions.sort_by_key(|(name, line)| (**line, (*name).clone()));
        for (name, line) in &functions {
            let _ = writeln!(out, "FN:{line},{name}");
        }
        for (name, _) in &functions {
            let hits = cov.function_hits.get(*name).copied().unwrap_or(0);
            let _ = writeln!(out, "FNDA:{hits},{name}");
        }
        if !cov.function_lines.is_empty() {
            let _ = writeln!(out, "FNF:{}", cov.function_lines.len());
            let hit = cov
                .function_lines
                .keys()
                .filter(|name| cov.function_hits.get(*name).copied().unwrap_or(0) > 0)
                .count();
            let _ = writeln!(out, "FNH:{hit}");
        }
        if !cov.branches.is_empty() {
            for ((line, block, branch), taken) in &cov.branches {
                let taken = match taken {
                    None => "-".to_string(),
                    Some(n) => n.to_string(),
                };
                let _ = writeln!(out, "BRDA:{line},{block},{branch},{taken}");
            }
            let _ = writeln!(out, "BRF:{}", cov.branches.len());
            let hit = cov.branches.values().filter(|t| t.unwrap_or(0) > 0).count();
            let _ = writeln!(out, "BRH:{hit}");
        }
        for (line, hits) in &cov.line_hits {
            let _ = writeln!(out, "DA:{line},{hits}");
        }
        let _ = writeln!(out, "LH:{}", cov.lines_hit());
        let _ = writeln!(out, "LF:{}", cov.lines_found());
        let _ = writeln!(out, "end_of_record");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = "TN:\nSF:pkg/a.go\nFN:3,Foo\nFNDA:2,Foo\nDA:3,2\nDA:4,0\nBRDA:3,0,0,1\nBRDA:3,0,1,-\nLH:1\nLF:2\nend_of_record\n";

    #[test]
    fn parses_a_simple_tracefile() {
        let (report, warnings) = parse(SIMPLE);
        assert!(warnings.is_empty(), "{warnings:?}");
        let cov = &report["pkg/a.go"];
        assert_eq!(cov.line_hits[&3], 2);
        assert_eq!(cov.line_hits[&4], 0);
        assert_eq!(cov.function_lines["Foo"], 3);
        assert_eq!(cov.function_hits["Foo"], 2);
        assert_eq!(cov.branches[&(3, "0".into(), "0".into())], Some(1));
        assert_eq!(cov.branches[&(3, "0".into(), "1".into())], None);
        assert_eq!(cov.lines_found(), 2);
        assert_eq!(cov.lines_hit(), 1);
    }

    #[test]
    fn merges_hit_counts_across_tracefiles() {
        let (a, _) = parse("SF:x.rs\nDA:1,1\nDA:2,0\nFNDA:1,f\nFN:1,f\nBRDA:1,0,0,-\nend_of_record\n");
        let (b, _) = parse("SF:x.rs\nDA:1,3\nDA:5,1\nFNDA:4,f\nFN:1,f\nBRDA:1,0,0,2\nend_of_record\n");
        let mut merged = Report::new();
        merge_reports(&mut merged, a);
        merge_reports(&mut merged, b);
        let cov = &merged["x.rs"];
        assert_eq!(cov.line_hits[&1], 4);
        assert_eq!(cov.line_hits[&2], 0);
        assert_eq!(cov.line_hits[&5], 1);
        assert_eq!(cov.function_hits["f"], 5);
        // '-' + 2 = 2: the branch was evaluated by the second run.
        assert_eq!(cov.branches[&(1, "0".into(), "0".into())], Some(2));
        assert_eq!(cov.lines_found(), 3);
        assert_eq!(cov.lines_hit(), 2);
    }

    #[test]
    fn dash_branches_stay_dash_only_when_never_evaluated() {
        let (a, _) = parse("SF:x.rs\nBRDA:1,0,0,-\nend_of_record\n");
        let (b, _) = parse("SF:x.rs\nBRDA:1,0,0,-\nend_of_record\n");
        let mut merged = Report::new();
        merge_reports(&mut merged, a);
        merge_reports(&mut merged, b);
        assert_eq!(merged["x.rs"].branches[&(1, "0".into(), "0".into())], None);
    }

    #[test]
    fn tolerates_missing_end_of_record_and_crlf() {
        let (report, _) = parse("SF:a.kt\r\nDA:1,1\r\nSF:b.kt\r\nDA:2,0\r\n");
        assert_eq!(report.len(), 2);
        assert_eq!(report["a.kt"].line_hits[&1], 1);
        assert_eq!(report["b.kt"].line_hits[&2], 0);
    }

    #[test]
    fn malformed_records_warn_but_do_not_fail() {
        let (report, warnings) =
            parse("SF:a.kt\nDA:notanumber,1\nFN:x\nFN:y,f\nFNDA:z,f\nBRDA:1,0\nDA:7,1\nend_of_record\n");
        assert_eq!(report["a.kt"].line_hits, BTreeMap::from([(7, 1)]));
        assert_eq!(warnings.len(), 5);
    }

    #[test]
    fn function_names_may_contain_commas() {
        let (report, warnings) = parse("SF:a.cc\nFN:1,f<a, b>\nFNDA:2,f<a, b>\nend_of_record\n");
        assert!(warnings.is_empty());
        assert_eq!(report["a.cc"].function_lines["f<a, b>"], 1);
        assert_eq!(report["a.cc"].function_hits["f<a, b>"], 2);
    }

    #[test]
    fn emit_recomputes_summary_counters_and_round_trips() {
        let (report, _) = parse(SIMPLE);
        let text = emit(&report);
        assert_eq!(
            text,
            "SF:pkg/a.go\nFN:3,Foo\nFNDA:2,Foo\nFNF:1\nFNH:1\nBRDA:3,0,0,1\nBRDA:3,0,1,-\nBRF:2\nBRH:1\nDA:3,2\nDA:4,0\nLH:1\nLF:2\nend_of_record\n"
        );
        let (reparsed, _) = parse(&text);
        assert_eq!(reparsed, report);
    }

    #[test]
    fn emit_skips_function_and_branch_blocks_when_absent() {
        let (report, _) = parse("SF:a.py\nDA:1,0\nend_of_record\n");
        assert_eq!(emit(&report), "SF:a.py\nDA:1,0\nLH:0\nLF:1\nend_of_record\n");
    }
}
