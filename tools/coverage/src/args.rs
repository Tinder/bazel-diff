//! Command-line contract with Bazel's `collect_coverage.sh`.
//!
//! In coverage mode Bazel wraps every test in `collect_coverage.sh`, which
//! finishes by invoking the configured LCOV merger roughly as:
//!
//! ```text
//! $LCOV_MERGER --coverage_dir=$COVERAGE_DIR \
//!              --output_file=$COVERAGE_OUTPUT_FILE \
//!              --filter_sources=/usr/bin/.+ ... \
//!              --source_file_manifest=$COVERAGE_MANIFEST
//! ```
//!
//! Unknown flags are collected as warnings instead of errors so that a newer
//! Bazel adding a flag degrades gracefully rather than failing every
//! coverage run.

#[derive(Debug, Default, PartialEq)]
pub struct Args {
    pub coverage_dir: Option<String>,
    pub output_file: Option<String>,
    pub filter_sources: Vec<String>,
    pub source_file_manifest: Option<String>,
    /// Unrecognised flags, reported as warnings.
    pub unknown: Vec<String>,
}

/// Parse argv (without the program name). Both `--flag=value` and
/// `--flag value` spellings are accepted.
pub fn parse(argv: &[String]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut iter = argv.iter().peekable();
    while let Some(arg) = iter.next() {
        let Some(flag) = arg.strip_prefix("--") else {
            return Err(format!("unexpected positional argument '{arg}'"));
        };
        let (name, value) = match flag.split_once('=') {
            Some((n, v)) => (n, v.to_string()),
            None => match iter.next() {
                Some(v) => (flag, v.clone()),
                None => return Err(format!("flag '--{flag}' is missing a value")),
            },
        };
        match name {
            "coverage_dir" => args.coverage_dir = Some(value),
            "output_file" => args.output_file = Some(value),
            "filter_sources" => args.filter_sources.push(value),
            "source_file_manifest" => args.source_file_manifest = Some(value),
            _ => args.unknown.push(format!("--{name}={value}")),
        }
    }
    if args.coverage_dir.is_none() {
        return Err("--coverage_dir is required".to_string());
    }
    if args.output_file.is_none() {
        return Err("--output_file is required".to_string());
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_vec(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_the_collect_coverage_invocation() {
        let args = parse(&to_vec(&[
            "--coverage_dir=/tmp/cov",
            "--output_file=/tmp/out.dat",
            "--filter_sources=/usr/bin/.+",
            "--filter_sources=/usr/lib/.+",
            "--source_file_manifest=/tmp/manifest.txt",
        ]))
        .unwrap();
        assert_eq!(args.coverage_dir.as_deref(), Some("/tmp/cov"));
        assert_eq!(args.output_file.as_deref(), Some("/tmp/out.dat"));
        assert_eq!(args.filter_sources, vec!["/usr/bin/.+", "/usr/lib/.+"]);
        assert_eq!(
            args.source_file_manifest.as_deref(),
            Some("/tmp/manifest.txt")
        );
        assert!(args.unknown.is_empty());
    }

    #[test]
    fn accepts_space_separated_values() {
        let args = parse(&to_vec(&["--coverage_dir", "d", "--output_file", "o"])).unwrap();
        assert_eq!(args.coverage_dir.as_deref(), Some("d"));
        assert_eq!(args.output_file.as_deref(), Some("o"));
    }

    #[test]
    fn unknown_flags_are_warnings_not_errors() {
        let args = parse(&to_vec(&[
            "--coverage_dir=d",
            "--output_file=o",
            "--legacy_branch_coverage=true",
        ]))
        .unwrap();
        assert_eq!(args.unknown, vec!["--legacy_branch_coverage=true"]);
    }

    #[test]
    fn missing_required_flags_and_positionals_are_errors() {
        assert!(parse(&to_vec(&["--output_file=o"])).is_err());
        assert!(parse(&to_vec(&["--coverage_dir=d"])).is_err());
        assert!(parse(&to_vec(&["stray"])).is_err());
        assert!(parse(&to_vec(&["--coverage_dir"])).is_err());
    }
}
