mod toolchain_readers;
use crate::cancel::{self, Cancellation};
use crate::proto::analysis::{
    ConfiguredAspect, ConfiguredAspectResult, ConfiguredTarget, CqueryResult,
};
use crate::proto::blaze_query::{
    target, Attribute, ConfiguredRuleInput, PackageGroup, Repository, Rule, Target,
};
use anyhow::{anyhow, bail, Context, Result};
use buffa::Message;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::time::Instant;
use tempfile::NamedTempFile;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct BazelVersion(u32, u32, u32);

#[derive(Debug)]
struct QueryFailure {
    code: i32,
    message: String,
}

impl std::fmt::Display for QueryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for QueryFailure {}

/// The flag that decides whether the `//external` package exists.
const WORKSPACE_FLAG: &str = "enable_workspace";

/// What one `bazel info release output_base starlark-semantics` reports. Generation asks for
/// these together before querying so that the same call replaces `bazel version`, the later
/// `bazel info output_base` of the hashing pass, and -- when WORKSPACE is known to be disabled --
/// the `//external:all-targets` query that would only fail and be retried without it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ServerInfo {
    version: Option<BazelVersion>,
    pub(crate) output_base: Option<PathBuf>,
    /// Whether `//external` exists. `None` when it cannot be told from `bazel info` alone, which
    /// keeps the query probing for it.
    external_package: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct BazelOptions {
    pub workspace: PathBuf,
    pub bazel: PathBuf,
    pub startup_options: Vec<String>,
    pub command_options: Vec<String>,
    pub cquery_options: Vec<String>,
    pub use_cquery: bool,
    pub cquery_expression: Option<String>,
    pub cquery_root_expression: Option<String>,
    pub keep_going: bool,
    pub fine_grained_external_repos: BTreeSet<String>,
    /// Module extensions, as `bazel mod show_extension` names them
    /// (`@rules_rs//rs:extensions.bzl%crate`), whose repositories use each other only through
    /// labels: no target resolves a toolchain from one of them. The module that owns such an
    /// extension then does not stand in for its repositories (see `lower_repositories`).
    pub label_only_module_extensions: BTreeSet<String>,
    /// A root-isolated extension import whose external consumers are discoverable through
    /// ordinary target inputs and BUILD loads.
    pub isolated_dependency_root: Option<String>,
    pub exclude_external_targets: bool,
    pub exclude_targets_query: Option<String>,
    pub no_bazelrc: bool,
    pub verbose: bool,
    /// Set by `bazel-diff serve` for the query being answered: every Bazel subprocess is run
    /// through it so an abandoned query can kill them. `None` for the CLI commands.
    pub cancellation: Option<Arc<Cancellation>>,
}

impl BazelOptions {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.bazel);
        command.current_dir(&self.workspace);
        if self.no_bazelrc {
            command.arg(if cfg!(windows) {
                "--bazelrc=NUL"
            } else {
                "--bazelrc=/dev/null"
            });
        }
        command.args(&self.startup_options);
        command
    }

    fn run_capture(&self, args: &[&str]) -> Result<Output> {
        let mut command = self.command();
        command.args(args).stdin(Stdio::null());
        if self.verbose {
            eprintln!("[Info] Command: {command:?}");
        }
        self.run(&mut command)
            .with_context(|| format!("failed to execute {}", self.bazel.display()))
    }

    /// Runs a Bazel command like `Command::output`, through the request's cancellation token when
    /// there is one.
    fn run(&self, command: &mut Command) -> io::Result<Output> {
        cancel::output(self.cancellation.as_deref(), command)
    }

    /// [`run`](Self::run) for a command whose stdout is redirected to a file.
    fn run_configured(&self, command: &mut Command) -> io::Result<Output> {
        cancel::output_configured(self.cancellation.as_deref(), command)
    }

    /// Fails once the query these options belong to has been cancelled, so a phase without a
    /// subprocess to kill still stops promptly.
    pub fn check_cancelled(&self) -> Result<()> {
        if let Some(token) = &self.cancellation {
            token.check()?;
        }
        Ok(())
    }

    /// Whether the workspace resolves its dependencies through Bzlmod.
    ///
    /// Probed with `bazel mod dump_repo_mapping ""`, which needs module resolution only.
    /// `bazel mod graph` evaluates every module extension in the dependency graph first, so a
    /// broken extension no target uses -- one a lazy `bazel build //...` never touches -- made
    /// it exit 2 and read as "Bzlmod disabled", silently dropping every `//external:` node
    /// (#531). `mod graph` stays as the fallback for a Bazel without `dump_repo_mapping`,
    /// taking the partial answer it gives over such an extension.
    pub fn is_bzlmod_enabled(&self) -> bool {
        let (enabled, _root_mapping) = self.probe_bzlmod();
        enabled
    }

    fn probe_bzlmod(&self) -> (bool, Option<Vec<u8>>) {
        if let Ok(output) = self.run_capture(&["mod", "dump_repo_mapping", ""]) {
            if output.status.success() {
                return (true, Some(output.stdout));
            }
        }
        let enabled = self
            .run_capture(&["mod", "graph"])
            .map(|output| {
                output.status.success()
                    || extension_failure(&String::from_utf8_lossy(&output.stderr))
                        .is_some_and(|failure| failure.answered)
            })
            .unwrap_or(false);
        (enabled, None)
    }

    /// For a module workspace the hashing pass needs the graph and the root and module
    /// mappings. Ask Bazel for those mappings together when the graph's module names fit
    /// in one call. Only actual mapping answers are used; a rejected or incomplete batch
    /// falls back to the independent root probe, including for a broken unused extension.
    fn probe_query_repositories(
        &self,
        supported: bool,
        survey: &mut ModuleSurvey,
    ) -> (bool, Option<Vec<u8>>) {
        if supported && self.workspace.join("MODULE.bazel").is_file() {
            survey.graph = self.run_capture(&["mod", "graph", "--output=json"]).ok();
            if let Some(prefetched) = survey
                .graph
                .as_ref()
                .and_then(|graph| self.prefetch_module_mappings(graph))
            {
                let (root, mappings) = prefetched;
                survey.mappings = mappings;
                return (true, Some(root));
            }
        }
        self.probe_bzlmod()
    }

    fn prefetch_module_mappings(
        &self,
        graph: &Output,
    ) -> Option<(Vec<u8>, BTreeMap<String, RepoMapping>)> {
        let complete = |output: &Output| {
            output.status.success() && !String::from_utf8_lossy(&output.stderr).contains("ERROR:")
        };
        if !complete(graph) {
            return None;
        }
        let candidates =
            hidden_module_repo_names(&String::from_utf8_lossy(&graph.stdout), &BTreeMap::new())
                .into_values()
                .filter(|name| !is_extension_repo(name))
                .collect::<BTreeSet<_>>();
        let size = 1 + candidates.iter().map(|name| name.len() + 1).sum::<usize>();
        if candidates.is_empty() || size > mod_argument_byte_budget() {
            return None;
        }
        let mut args = vec!["mod", "dump_repo_mapping", ""];
        args.extend(candidates.iter().map(String::as_str));
        let output = self.run_capture(&args).ok()?;
        if !complete(&output) {
            return None;
        }
        // Bazel prints one JSON object per argument, in order. Keep the root's
        // original bytes for external hashing and reject partial or extra rows.
        let mut rows = output.stdout.split_inclusive(|&byte| byte == b'\n');
        let root = rows.next()?;
        serde_json::from_slice::<RepoMapping>(root).ok()?;
        let mut mappings = BTreeMap::new();
        for name in candidates {
            mappings.insert(name, serde_json::from_slice(rows.next()?).ok()?);
        }
        rows.next().is_none().then(|| (root.to_vec(), mappings))
    }

    /// The module graph for the hash file's metadata. A failed module extension is reported
    /// by the hashing pass, so this one takes the partial answer silently.
    pub fn module_graph_json(&self) -> Option<String> {
        let output = self.run_capture(&["mod", "graph", "--output=json"]).ok()?;
        let answered = output.status.success()
            || extension_failure(&String::from_utf8_lossy(&output.stderr))
                .is_some_and(|failure| failure.answered);
        answered.then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// `bazel mod graph --output=json`. The module graph proper survives a module extension
    /// Bazel cannot evaluate: the whole graph is printed before the extension is reported and
    /// the command exits 2.
    fn module_graph_json_tolerating(
        &self,
        output: Option<Output>,
        failed: &mut FailedExtensions,
    ) -> Result<(String, bool)> {
        let output = match output {
            Some(output) => output,
            None => self.run_capture(&["mod", "graph", "--output=json"])?,
        };
        let complete = output.status.success();
        let stdout = self.accept_mod_output(output, "graph --output=json", failed)?;
        let json = if self.isolated_dependency_root.is_some() {
            String::from_utf8(stdout).context("module graph is not valid UTF-8")?
        } else {
            String::from_utf8_lossy(&stdout).into_owned()
        };
        Ok((json.trim().to_owned(), complete))
    }

    pub fn dependency_fingerprint(&self) -> Result<String> {
        const STREAMED_VERSION: &str = "bzlmod-streamed-proto-v2";
        const TEXT_FALLBACK_VERSION: &str = "bzlmod-show-repo-text-v1";
        let total_start = Instant::now();
        let mut hasher = Sha256::new();
        if !self.is_bzlmod_enabled() {
            hasher.update(b"mode:legacy");
            eprintln!(
                "[BD-DBG][fingerprint-ms] mode=legacy version={TEXT_FALLBACK_VERSION} mapping=0 show_repo=0 total={}",
                total_start.elapsed().as_millis()
            );
            return Ok(hex::encode(hasher.finalize()));
        }

        hasher.update(b"mode:bzlmod\0");
        let module_graph = self.module_graph_json().unwrap_or_default();
        hasher.update(module_graph.as_bytes());
        hasher.update(b"\0");

        let mapping_start = Instant::now();
        let canonical_names = self.canonical_bzlmod_repo_names()?;
        let mapping_millis = mapping_start.elapsed().as_millis();

        let show_repo_start = Instant::now();
        let streamed = self.show_repo_streamed_proto(&canonical_names);
        let show_repo_millis = show_repo_start.elapsed().as_millis();
        if let Ok(repo_state) = streamed {
            hasher.update(&repo_state);
            eprintln!(
                "[BD-DBG][fingerprint-ms] mode=bzlmod version={STREAMED_VERSION} mapping={mapping_millis} show_repo={show_repo_millis} total={} repoCount={} fallback=false",
                total_start.elapsed().as_millis(),
                canonical_names.len(),
            );
            return Ok(hex::encode(hasher.finalize()));
        }

        let fallback_show_repo_start = Instant::now();
        let repo_state = self.show_repo_fingerprint_text_fallback(&canonical_names)?;
        let fallback_show_repo_millis = fallback_show_repo_start.elapsed().as_millis();
        hasher.update(repo_state.as_bytes());
        eprintln!(
            "[BD-DBG][fingerprint-ms] mode=bzlmod version={TEXT_FALLBACK_VERSION} mapping={mapping_millis} show_repo={fallback_show_repo_millis} total={} repoCount={} fallback=true",
            total_start.elapsed().as_millis(),
            canonical_names.len(),
        );
        Ok(hex::encode(hasher.finalize()))
    }

    /// Asks Bazel for its release, output base and Starlark semantics in one invocation. `None`
    /// when the call fails (an older Bazel without one of the keys, say), so callers fall back to
    /// the separate calls they made before.
    ///
    /// The call carries the startup options and bazelrc the query will use but not
    /// `--bazelCommandOptions`, which are `bazel query` flags `bazel info` may reject.
    fn server_info(&self) -> Option<ServerInfo> {
        let output = self
            .run_capture(&["info", "release", "output_base", "starlark-semantics"])
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let mut info = parse_server_info(&String::from_utf8_lossy(&output.stdout))?;
        // `bazel info` reads the `common`, `build` and `info` sections of a bazelrc and
        // `bazel query` the `common` and `query` ones, and the query alone gets
        // `--bazelCommandOptions`. Leave the question open unless neither can set the flag.
        if info.external_package.is_some()
            && (self
                .command_options
                .iter()
                .any(|option| option.contains(WORKSPACE_FLAG))
                || self.bazelrc_mentions(WORKSPACE_FLAG))
        {
            info.external_package = None;
        }
        Some(info)
    }

    /// Whether any bazelrc Bazel reads for this workspace, or one they import, names `flag`.
    /// Deliberately coarse: a comment or an unrelated config naming it also counts.
    fn bazelrc_mentions(&self, flag: &str) -> bool {
        if self.no_bazelrc {
            return false;
        }
        let mut pending = vec![
            self.workspace.join(".bazelrc"),
            PathBuf::from("/etc/bazel.bazelrc"),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            pending.push(PathBuf::from(home).join(".bazelrc"));
        }
        pending.extend(
            self.startup_options
                .iter()
                .filter_map(|option| option.strip_prefix("--bazelrc="))
                .map(PathBuf::from),
        );
        let mut seen = HashSet::new();
        while let Some(path) = pending.pop() {
            if seen.len() > 64 || !seen.insert(path.clone()) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            if text.contains(flag) {
                return true;
            }
            for line in text.lines() {
                let line = line.trim();
                let Some(import) = line
                    .strip_prefix("try-import")
                    .or_else(|| line.strip_prefix("import"))
                else {
                    continue;
                };
                let import = import
                    .trim()
                    .replace("%workspace%", &self.workspace.to_string_lossy());
                let import = PathBuf::from(import);
                pending.push(if import.is_relative() {
                    self.workspace.join(import)
                } else {
                    import
                });
            }
        }
        false
    }

    fn version(&self) -> Result<BazelVersion> {
        let output = self.run_capture(&["version"])?;
        if !output.status.success() {
            bail!("Bazel version command failed, exit code {}", output.status);
        }
        let text = String::from_utf8_lossy(&output.stdout);
        parse_bazel_version(&text)
            .ok_or_else(|| anyhow!("Bazel version command returned unexpected output: {text}"))
    }

    fn with_exclude_filter(&self, query: String) -> String {
        self.exclude_targets_query
            .as_ref()
            .filter(|query| !query.trim().is_empty())
            .map_or(query.clone(), |exclude| {
                format!("({query}) except ({exclude})")
            })
    }

    fn configured_root_query(expression: &str) -> String {
        format!("config(({expression}), target)")
    }

    pub fn query_all_targets(&self) -> Result<Vec<Target>> {
        self.check_raw_targets_supported()?;
        Ok(self
            .query_all_targets_with(materialize_repository_inputs_for_batch)?
            .targets)
    }

    pub(crate) fn check_raw_targets_supported(&self) -> Result<()> {
        if self.isolated_dependency_root.is_some() {
            bail!("--isolatedDependencyRoot requires generate_hashes to retain repository inputs");
        }
        if self.cquery_root_expression.is_some() {
            bail!("--cqueryRootExpression requires generate-hashes to retain configured roots");
        }
        Ok(())
    }

    pub(crate) fn query_all_targets_with(
        &self,
        mut transform: impl FnMut(&mut [Target], Option<RepositoryInputs>),
    ) -> Result<QueriedTargets> {
        self.query_all_targets_encoded(false, false, |batch, _, inputs, _ordinary_inputs| {
            transform(batch, inputs)
        })
    }

    /// [`Self::query_all_targets_with`], with ordinary query results decoded by
    /// [`decode_lean_target`] when `lean` is set: every rule outside `//external` reaches
    /// `transform` with only its `tags`, `visibility` and `toolchain` attributes decoded, alongside
    /// the bytes it was decoded from (see [`EncodedTargets`]). Targets decoded in full (repository
    /// rules) come with empty bytes, and batches that were not decoded lean (cquery, Bzlmod
    /// repositories, an isolated dependency root's strict query) with an empty slice.
    /// The final argument carries positively classified ordinary module dependencies only.
    pub(crate) fn query_all_targets_encoded(
        &self,
        lean: bool,
        physical_sources: bool,
        transform: impl FnMut(
            &mut [Target],
            &EncodedTargets<'_>,
            Option<RepositoryInputs>,
            Option<Vec<String>>,
        ),
    ) -> Result<QueriedTargets> {
        if self.cquery_root_expression.is_some() {
            if !self.use_cquery {
                bail!("--cqueryRootExpression requires --useCquery");
            }
            if self.cquery_expression.is_some() {
                bail!("--cqueryRootExpression cannot be combined with --cqueryExpression");
            }
            // A configured-root artifact records complete query results, so an empty roster
            // describes a successful empty selection rather than a partial keep-going result.
            return Self {
                keep_going: false,
                ..self.clone()
            }
            .query_all_targets_inner(transform, false, lean, physical_sources);
        }
        self.query_all_targets_inner(transform, false, lean, physical_sources)
    }

    pub(crate) fn query_all_targets_verified_with(
        &self,
        mut transform: impl FnMut(&mut [Target], Option<RepositoryInputs>),
    ) -> Result<QueriedTargets> {
        if !self.use_cquery
            || self.cquery_root_expression.is_none()
            || self.cquery_expression.is_some()
        {
            bail!("--verifyConfiguredRoots requires --useCquery and a configured root expression");
        }
        Self {
            keep_going: false,
            ..self.clone()
        }
        // Verification is a cquery; nothing it decodes is lean.
        .query_all_targets_inner(
            |batch, _, inputs, _ordinary_inputs| transform(batch, inputs),
            true,
            false,
            false,
        )
    }

    fn query_all_targets_inner(
        &self,
        mut transform: impl FnMut(
            &mut [Target],
            &EncodedTargets<'_>,
            Option<RepositoryInputs>,
            Option<Vec<String>>,
        ),
        verify: bool,
        lean: bool,
        physical_sources: bool,
    ) -> Result<QueriedTargets> {
        if let Some(root) = &self.isolated_dependency_root {
            if self.use_cquery {
                bail!("--isolatedDependencyRoot={root} requires ordinary query");
            }
        }
        let mut bzlmod = BzlmodRepos::default();
        let server_info = self.server_info().unwrap_or_default();
        let checked_version = match server_info.version {
            Some(version) => Ok(version),
            None => self.version(),
        };
        let supports_bzlmod = checked_version
            .as_ref()
            .is_ok_and(|found| *found >= BazelVersion(8, 6, 0) && *found != BazelVersion(9, 0, 0));
        let mut survey = ModuleSurvey::default();
        let (bzlmod_enabled, root_mapping) =
            self.probe_query_repositories(supports_bzlmod, &mut survey);
        let version = match checked_version {
            Ok(found) => Some(found),
            Err(error) => {
                if let Some(root) = &self.isolated_dependency_root {
                    return Err(error).with_context(|| {
                        format!(
                            "--isolatedDependencyRoot={root}: failed to determine Bazel version"
                        )
                    });
                }
                if bzlmod_enabled {
                    if !self.keep_going {
                        return Err(error);
                    }
                    eprintln!(
                        "[Warn] failed to check Bazel version for Bzlmod repositories: {error:#}"
                    );
                }
                None
            }
        };
        if let Some(root) = &self.isolated_dependency_root {
            if !supports_bzlmod {
                let BazelVersion(major, minor, patch) = version.expect("checked version");
                bail!("--isolatedDependencyRoot={root}: Bazel {major}.{minor}.{patch} does not support Bzlmod repository discovery");
            }
            if !bzlmod_enabled {
                bail!(
                    "--isolatedDependencyRoot={root}: Bzlmod repository discovery is unavailable"
                );
            }
        }
        if bzlmod_enabled && supports_bzlmod {
            let queried = self.query_bzlmod_repos(root_mapping, survey);
            let queried = if let Some(root) = &self.isolated_dependency_root {
                queried.with_context(|| format!("--isolatedDependencyRoot={root}"))
            } else {
                queried
            };
            match queried {
                Ok(repos) => bzlmod = repos,
                Err(error) if self.keep_going && self.isolated_dependency_root.is_none() => {
                    eprintln!("[Warn] failed to hash Bzlmod repositories: {error:#}");
                }
                Err(error) => return Err(error),
            }
        }
        let BzlmodRepos {
            mut definitions,
            mut failed_extensions,
            canonical_to_apparent,
            module_graph_json,
            module_graph_complete,
            root_mapping,
            scope,
        } = bzlmod;
        let observe = physical_sources
            && !self.use_cquery
            && self.isolated_dependency_root.is_none()
            && !self.fine_grained_external_repos.is_empty()
            && root_mapping.is_some();
        let mut primary_rules = toolchain_readers::PrimaryRules::default();
        let mut ordinary = self.clone();
        if observe {
            ordinary.command_options.push("--proto:rule_classes".into());
        }
        let initial_repositories = if observe {
            Vec::new()
        } else {
            definitions.take().map_or_else(Vec::new, |mut defs| {
                defs.lower(self, &canonical_to_apparent, |row| {
                    row.finish(&BTreeSet::new(), &mut |target, inputs, ordinary| {
                        transform(std::slice::from_mut(target), &[], inputs, ordinary);
                    })
                })
            })
        };
        let configured_roots = self
            .cquery_root_expression
            .as_deref()
            .map(|expression| self.query_configured_roots(expression, version))
            .transpose()?;
        let strict = (!scope.members.is_empty()).then(|| Self {
            keep_going: false,
            ..self.clone()
        });
        let mut loads = BTreeMap::new();
        let mut verification = None;
        let mut targets = if configured_roots.as_ref().is_some_and(BTreeMap::is_empty) {
            Vec::new()
        } else {
            self.collect_all_targets(
                initial_repositories,
                server_info.external_package,
                |expression, use_cquery| {
                    if let Some(strict) = &strict {
                        strict.query_with(
                            &self.with_exclude_filter(scope.expression(expression)),
                            false,
                            version,
                            false,
                            &mut |batch, _| {
                                scope.record_build_loads(batch, &mut loads);
                                transform(batch, &[], None, None);
                            },
                        )
                    } else if verify && use_cquery {
                        // Keep positive distances on the caller's normal view. The full typed
                        // view below qualifies coverage without adding positive impact paths.
                        let targets =
                            self.query_with(expression, true, version, false, &mut |batch, _| {
                                transform(batch, &[], None, None)
                            })?;
                        let graph = self.query_with_capture(
                            expression,
                            true,
                            version,
                            false,
                            &mut |_, _| {},
                            CqueryCapture::Verification,
                            |_| true,
                        );
                        match graph {
                            Ok(graph) => verification = graph.into_verified(),
                            Err(error) => {
                                eprintln!(
                                    "[Warn] configured-root verification unavailable: {error:#}"
                                );
                            }
                        }
                        Ok(targets)
                    } else {
                        ordinary.query_with(
                            expression,
                            use_cquery,
                            version,
                            lean,
                            &mut |batch, encoded| {
                                if observe {
                                    primary_rules.record(batch, encoded);
                                }
                                transform(batch, encoded, None, None);
                            },
                        )
                    }
                },
            )?
        };
        if let Some(strict) = &strict {
            // Direct users may name a target the hub doesn't export. Ask only about
            // edges actually seen in the ordinary graph, and keep the primary's rows.
            let mut coverage = ScopedCoverage::default();
            coverage.record(&scope, &targets);
            let mut additions = Vec::new();
            if !coverage.missing.is_empty() {
                let roots = query_labels(coverage.missing.iter())?;
                additions.push(format!(
                    "(let direct = deps({roots}) in $direct + buildfiles($direct))"
                ));
            }
            let readers: BTreeSet<_> = loads
                .iter()
                .filter(|(_, names)| {
                    names
                        .iter()
                        .filter_map(|name| scope.canonical(name))
                        .any(|name| !coverage.files.contains(&name))
                })
                .map(|(build, _)| build)
                .collect();
            if !readers.is_empty() {
                additions.push(format!(
                    "buildfiles({})",
                    query_labels(readers.into_iter())?
                ));
            }
            if !additions.is_empty() {
                let query = self.with_exclude_filter(additions.join(" + "));
                let mut extra =
                    strict.query_with(&query, false, version, false, &mut |batch, _| {
                        scope.record_build_loads(batch, &mut loads);
                    })?;
                retain_supplemental_targets(&mut extra, &targets);
                transform(&mut extra, &[], None, None);
                coverage.record(&scope, &extra);
                targets.extend(extra);
                if let Some(missing) = coverage.missing.first() {
                    bail!("--isolatedDependencyRoot: scoped input {missing} was not returned");
                }
            }
            scope.connect_external_readers(&loads, &mut targets, &coverage)?;
        }
        let mut alias_sources = HashMap::new();
        let mut filegroups = BTreeMap::new();
        let mut filegroup_sources = BTreeMap::new();
        let mut filegroup_parents = BTreeMap::new();
        if !self.use_cquery
            && !self.fine_grained_external_repos.is_empty()
            && self.isolated_dependency_root.is_none()
        {
            match self.external_filegroup_inputs(
                &targets,
                root_mapping.as_deref(),
                version,
                &mut filegroup_sources,
                &mut filegroup_parents,
            ) {
                Ok(result) => {
                    let (mut rows, observed) = result;
                    transform(&mut rows, &[], None, None);
                    targets.extend(rows);
                    filegroups.extend(observed);
                }
                Err(error) => {
                    self.check_cancelled()?;
                    eprintln!(
                        "[Warn] retaining repository fallbacks for unobserved filegroup inputs: {error:#}"
                    );
                }
            }
            if let Err(error) = self.acquire_alias_inputs(
                &mut targets,
                root_mapping.as_deref(),
                version,
                &mut alias_sources,
                &mut |batch, inputs| transform(batch, &[], inputs, None),
            ) {
                self.check_cancelled()?;
                if !self.keep_going {
                    return Err(error).context("failed to acquire ordinary alias inputs");
                }
                primary_rules.alias_discovery_failed = true;
                eprintln!("[Warn] keeping ordinary repository fallbacks for incomplete alias discovery: {error:#}");
            }
        }
        if let Some(mut defs) = definitions {
            let repos = defs.lower(self, &canonical_to_apparent, std::convert::identity);
            let mut observed = BTreeSet::new();
            if let Some(mapping) = root_mapping.as_deref() {
                let survey = (module_graph_complete
                    && defs.extensions.complete_mappings
                    && !failed_extensions.observed)
                    .then_some(&defs.extensions);
                match self.observe_external_inputs(
                    &targets,
                    primary_rules,
                    mapping,
                    survey,
                    &repos,
                    version,
                ) {
                    Ok(mut inputs) => {
                        retain_supplemental_targets(&mut inputs.sources, &targets);
                        transform(&mut inputs.sources, &[], None, None);
                        targets.extend(inputs.sources);
                        for rule in targets
                            .iter_mut()
                            .filter_map(|row| row.rule.as_option_mut())
                        {
                            if let Some(children) = inputs.bindings.remove(&rule.name) {
                                rule.rule_input.extend(children);
                                rule.rule_input.sort_unstable();
                                rule.rule_input.dedup();
                            }
                        }
                        observed = inputs.usages;
                    }
                    Err(error) => {
                        self.check_cancelled()?;
                        eprintln!("[Warn] retaining inferred external dependencies: {error:#}");
                    }
                }
            }
            targets.extend(repos.into_iter().map(|row| {
                row.finish(&observed, &mut |target, group, ordinary| {
                    transform(std::slice::from_mut(target), &[], group, ordinary)
                })
            }));
            targets = deduplicate_targets(targets)?;
        }
        self.check_failed_extension_references(
            &targets,
            &mut failed_extensions,
            &canonical_to_apparent,
        )?;
        Ok(QueriedTargets {
            targets,
            verification,
            configured_roots,
            module_graph_json,
            module_graph_complete,
            root_mapping,
            output_base: server_info.output_base,
            scoped_sources: ScopedSources {
                repositories: scope
                    .aliases
                    .into_iter()
                    .filter_map(|(alias, canonical)| {
                        scope.members.contains(&canonical).then_some(alias)
                    })
                    .chain(scope.members.iter().cloned())
                    .collect(),
                repository_inputs: scope.repository_inputs,
                queried_files: HashSet::new(),
                alias_sources,
                filegroups,
                filegroup_sources,
                filegroup_parents,
            },
        })
    }

    /// Follow explicit inputs into already observed external packages. An evaluated
    /// native filegroup can forward into another package, including through more
    /// filegroups. Other rule kinds keep their existing repository coverage.
    fn external_filegroup_inputs(
        &self,
        primary: &[Target],
        root_mapping: Option<&[u8]>,
        version: Option<BazelVersion>,
        filegroup_sources: &mut BTreeMap<String, AliasSource>,
        filegroup_parents: &mut BTreeMap<String, FilegroupWitness>,
    ) -> Result<(Vec<Target>, BTreeMap<String, FilegroupWitness>)> {
        let Some(root_mapping) = root_mapping else {
            return Ok(Default::default());
        };
        let mapping =
            crate::hash::parse_apparent_repo_mapping(&String::from_utf8_lossy(root_mapping));
        let mut present = HashSet::new();
        let mut packages = HashSet::new();
        let mut missing_loads = BTreeMap::new();
        for row in primary {
            let Some(name) = target_name(row) else {
                continue;
            };
            let id = canonical_query_label(name, &mapping);
            present.insert(id);
            let build = row.source_file.as_option().filter(|file| {
                matches!(
                    file.name.rsplit_once(':'),
                    Some((_, "BUILD" | "BUILD.bazel"))
                ) && file.package_contains_errors != Some(true)
            });
            if let Some(file) = build
                .filter(|file| file.package_contains_errors.is_none() && file.subinclude.is_empty())
            {
                if let Some(pkg) = file.name.strip_suffix(":BUILD.bazel") {
                    missing_loads.insert(canonical_query_label(pkg, &mapping), None);
                }
            }
            let loads = build
                .into_iter()
                .flat_map(|file| file.subinclude.iter().map(String::as_str));
            for label in std::iter::once(name).chain(loads) {
                let label = canonical_query_label(label, &mapping);
                if let Some(pkg) = external_query_package(&label) {
                    packages.insert(pkg.to_owned());
                }
            }
        }
        let mut pending = BTreeSet::new();
        let mut unseeded = BTreeSet::new();
        let mut referring_genrules = BTreeMap::<String, BTreeSet<&str>>::new();
        for row in primary {
            let Some(rule) = row.rule.as_option() else {
                continue;
            };
            if main_repo_label_key(&rule.name).starts_with("//external:") {
                continue;
            }
            let external_group = rule.name.starts_with('@')
                && !main_repo_label_key(&rule.name).starts_with("//")
                && rule.rule_class_key.as_deref() == Some("filegroup");
            for input in &rule.rule_input {
                let input = canonical_query_label(input, &mapping);
                if present.contains(input.as_ref()) {
                    continue;
                }
                let package = external_query_package(&input);
                let candidates =
                    if external_group || package.is_some_and(|pkg| packages.contains(pkg)) {
                        &mut pending
                    } else {
                        if package.is_none() {
                            continue;
                        }
                        let parent_package = rule
                            .name
                            .rsplit_once(':')
                            .map(|(pkg, _)| canonical_query_label(pkg, &mapping));
                        let Some(parent) = parent_package
                            .as_ref()
                            .and_then(|pkg| missing_loads.get_mut(pkg.as_ref()))
                        else {
                            continue;
                        };
                        parent.get_or_insert(rule.name.as_str());
                        &mut unseeded
                    };
                let input = input.into_owned();
                if rule.rule_class == "genrule" {
                    referring_genrules
                        .entry(input.clone())
                        .or_default()
                        .insert(rule.name.as_str());
                }
                candidates.insert(input);
            }
        }
        let mut strict = self.clone();
        strict.keep_going = false;
        // The structural witness must include dependencies the caller may hide.
        strict.command_options.extend([
            "--nokeep_going".into(),
            "--tool_deps".into(),
            "--implicit_deps".into(),
            "--proto:rule_classes".into(),
            "--proto:rule_inputs_and_outputs".into(),
            "--proto:output_rule_attrs=all".into(),
            "--noproto:flatten_selects".into(),
            "--proto:locations".into(),
            "--norelative_locations".into(),
        ]);
        if !unseeded.is_empty() {
            // Some Bazel formatters omit the subinclude list for BUILD.bazel. Only
            // actual .bzl labels from buildfiles() can supply the missing load here.
            let representatives = missing_loads
                .into_values()
                .flatten()
                .map(str::to_owned)
                .collect();
            let loads = query_set(&representatives).and_then(|roots| {
                let query = self.with_exclude_filter(format!("buildfiles({roots})"));
                strict.query_with_filtered(&query, false, version, false, &mut |_, _| {}, |row| {
                    row.source_file
                        .as_option()
                        .is_some_and(|file| file.name.ends_with(".bzl"))
                })
            });
            match loads {
                Ok(rows) => {
                    for name in rows.iter().filter_map(target_name) {
                        if let Some(pkg) =
                            external_query_package(&canonical_query_label(name, &mapping))
                        {
                            packages.insert(pkg.to_owned());
                        }
                    }
                    pending.extend(unseeded.into_iter().filter(|input| {
                        external_query_package(input).is_some_and(|pkg| packages.contains(pkg))
                    }));
                }
                Err(error) => {
                    self.check_cancelled()?;
                    eprintln!(
                        "[Warn] retaining repository inputs of unobserved BUILD.bazel loads: {error:#}"
                    );
                }
            }
        }
        let mut ordinary = Self {
            keep_going: false,
            ..self.clone()
        };
        // Class identity certifies the re-read row without changing its attribute projection.
        ordinary.command_options.push("--proto:rule_classes".into());
        let mut requested = BTreeSet::new();
        let mut filegroups = BTreeMap::new();
        let mut sources = BTreeMap::new();
        while !pending.is_empty() {
            let batch = std::mem::take(&mut pending);
            let query = query_set(&batch)?;
            let query = self.with_exclude_filter(query);
            let rows = strict.query_with_filtered(
                &query,
                false,
                version,
                false,
                &mut |_, _| {},
                |row| {
                    if let Some(source) = row.source_file.as_option() {
                        AliasSource::observe(source, &mut sources);
                    }
                    true
                },
            )?;
            present.extend(batch.iter().cloned().map(Cow::Owned));
            for row in rows {
                let Some(name) = target_name(&row) else {
                    continue;
                };
                if !batch.contains(canonical_query_label(name, &mapping).as_ref()) {
                    continue;
                }
                let name = name.to_owned();
                if let Some(rule) = row.rule.into_option() {
                    if rule.rule_class_key.as_deref() != Some("filegroup") {
                        continue;
                    }
                    for input in &rule.rule_input {
                        let id = canonical_query_label(input, &mapping);
                        if !present.contains(id.as_ref()) {
                            pending.insert(id.into_owned());
                        }
                    }
                    filegroups.insert(name.clone(), FilegroupWitness::new(rule));
                } else if row.source_file.as_option().is_none() {
                    continue;
                }
                requested.insert(name);
            }
        }
        let parent_names: BTreeSet<_> = filegroups
            .keys()
            .filter_map(|group| {
                referring_genrules.get(canonical_query_label(group, &mapping).as_ref())
            })
            .flatten()
            .map(|&name| name.to_owned())
            .collect();
        let mut previously_present = BTreeSet::new();
        if !parent_names.is_empty() {
            let parent_roots = query_set(&parent_names)?;
            let parent_query = self.with_exclude_filter(format!("deps({parent_roots}, 1)"));
            strict.command_options.push("--proto:default_values".into());
            let mut direct_sources = BTreeMap::new();
            let metadata = strict.query_with_filtered(
                &parent_query,
                false,
                version,
                false,
                &mut |_, _| {},
                |row| {
                    if let Some(source) = row.source_file.as_option() {
                        AliasSource::observe(source, &mut direct_sources);
                    }
                    true
                },
            );
            match metadata {
                Ok(parent_rows) => {
                    let mut unique = BTreeMap::new();
                    for row in parent_rows {
                        if let Some(parent) = row.rule.into_option() {
                            if parent_names.contains(&parent.name) {
                                match unique.entry(parent.name.clone()) {
                                    Entry::Vacant(entry) => {
                                        entry.insert(Some(parent));
                                    }
                                    Entry::Occupied(mut entry) => {
                                        entry.insert(None);
                                    }
                                }
                            }
                        }
                    }
                    filegroup_parents.extend(unique.into_iter().filter_map(|(name, rule)| {
                        rule.filter(|r| r.rule_class_key.as_deref() == Some("genrule"))
                            .map(|r| (name, FilegroupWitness::new(r)))
                    }));
                    let inputs: HashSet<_> = filegroup_parents
                        .values()
                        .flat_map(|parent| &parent.declaration.rule_input)
                        .collect();
                    for (label, witness) in direct_sources {
                        if !inputs.contains(&label) {
                            continue;
                        }
                        if !requested.contains(&label)
                            && present.contains(canonical_query_label(&label, &mapping).as_ref())
                        {
                            previously_present.insert(label.clone());
                        }
                        requested.insert(label.clone());
                        AliasSource::record(&mut sources, label, witness);
                    }
                }
                Err(error) => {
                    self.check_cancelled()?;
                    eprintln!("[Warn] retaining repository inputs of unobserved filegroup parents: {error:#}");
                }
            }
        }
        if requested.is_empty() {
            return Ok(Default::default());
        }
        // The strict declaration carries all structural attributes. When the caller requests
        // Bazel's synthetic attribute hash, bind its own projection to the same returned rule.
        // The parent already exists in the primary inventory and is read only as a witness.
        let projected = self.command_options.iter().any(|option| {
            matches!(
                option.as_str(),
                "--proto:include_synthetic_attribute_hash"
                    | "--proto:include_synthetic_attribute_hash=true"
            )
        });
        if projected {
            requested.extend(filegroup_parents.keys().cloned());
            previously_present.extend(filegroup_parents.keys().cloned());
        }
        let query = query_set(&requested)?;
        let query = self.with_exclude_filter(query);
        let mut rows = ordinary.query_with(&query, false, version, false, &mut |_, _| {})?;
        let returned: HashSet<_> = rows.iter().filter_map(target_name).collect();
        filegroups.retain(|name, _| returned.contains(name.as_str()));
        if projected {
            let mut seen = HashSet::new();
            for rule in rows.iter().filter_map(|row| row.rule.as_option()) {
                let Some(witness) = filegroups
                    .get_mut(&rule.name)
                    .or_else(|| filegroup_parents.get_mut(&rule.name))
                else {
                    continue;
                };
                witness.projected_definition = if seen.insert(&rule.name)
                    && rule.rule_class_key == witness.declaration.rule_class_key
                {
                    crate::hash::projected_filegroup_definition(rule)
                } else {
                    None
                };
            }
        }
        for source in rows.iter().filter_map(|row| row.source_file.as_option()) {
            let witness = sources.get(&source.name).and_then(Option::as_ref);
            if let Some(witness) = witness {
                filegroup_sources.insert(source.name.clone(), witness.clone());
            }
        }
        // Re-read primary sources and parents supply only evidence for their original rows.
        rows.retain(|row| target_name(row).is_none_or(|name| !previously_present.contains(name)));
        Ok((rows, filegroups))
    }

    /// A fine repository may expose libraries in other repositories through aliases. Follow
    /// actual targets of aliases used by the ordinary graph, without enrolling more namespaces.
    fn acquire_alias_inputs(
        &self,
        targets: &mut Vec<Target>,
        root_mapping: Option<&[u8]>,
        version: Option<BazelVersion>,
        alias_sources: &mut HashMap<String, AliasSource>,
        transform: &mut impl FnMut(&mut [Target], Option<RepositoryInputs>),
    ) -> Result<()> {
        let mapping = root_mapping
            .map(|raw| crate::hash::parse_apparent_repo_mapping(&String::from_utf8_lossy(raw)))
            .unwrap_or_default();
        let fine: HashSet<_> = self
            .fine_grained_external_repos
            .iter()
            .map(|repo| {
                let name = repo.trim_start_matches('@');
                if repo.starts_with("@@") {
                    name
                } else {
                    mapping.get(name).map_or(name, String::as_str)
                }
            })
            .collect();
        let fine_target = |name: &str| {
            name.strip_prefix('@')
                .and_then(|name| name.split_once("//"))
                .is_some_and(|(repo, _)| {
                    let canonical = repo
                        .strip_prefix('@')
                        .unwrap_or_else(|| mapping.get(repo).map_or(repo, String::as_str));
                    fine.contains(canonical)
                })
        };
        let ordinary_names: HashSet<_> = targets
            .iter()
            .filter_map(|row| row.rule.as_option())
            .filter(|rule| rule.rule_class == "alias" && !fine_target(&rule.name))
            .map(|rule| canonical_query_label(&rule.name, &mapping))
            .collect();
        let included = |name: &str| {
            fine_target(name) || ordinary_names.contains(&canonical_query_label(name, &mapping))
        };
        let mut primary = HashSet::new();
        let mut inside = HashMap::new();
        let mut queue = Vec::new();
        for row in targets.iter() {
            let Some(name) = target_name(row) else {
                continue;
            };
            let id = canonical_query_label(name, &mapping);
            primary.insert(id.clone());
            let is_fine = fine_target(name);
            if is_fine || ordinary_names.contains(&id) {
                inside.insert(id, row);
            }
            if !is_fine && !main_repo_label_key(name).starts_with("//external:") {
                queue.extend(target_inputs(row).filter(|s| included(s)));
            }
        }
        let mut walked = HashSet::new();
        let mut aliases = BTreeSet::new();
        let mut ordinary_aliases = BTreeSet::new();
        while let Some(input) = queue.pop() {
            let id = canonical_query_label(input, &mapping);
            if !walked.insert(id.clone()) {
                continue;
            }
            let Some(&row) = inside.get(&id) else {
                continue;
            };
            if let Some(rule) = row.rule.as_option().filter(|r| r.rule_class == "alias") {
                if fine_target(&rule.name) {
                    aliases.insert(rule.name.clone());
                } else {
                    ordinary_aliases
                        .insert(canonical_query_label(&rule.name, &mapping).into_owned());
                }
            }
            queue.extend(target_inputs(row).filter(|s| included(s)));
        }
        if aliases.is_empty() {
            return Ok(());
        }
        let version = match version {
            Some(version) => version,
            None => self.version()?,
        };
        // Bazel 7 does not expose the native rule class identity needed to distinguish aliases.
        if version < BazelVersion(8, 0, 0) {
            return Ok(());
        }

        // The caller may exclude actual from the attributes it wants to hash. Inspect this
        // finite set separately; keep the original rows and their full select-condition inputs.
        let mut bindings = self.alias_actuals(&aliases, version)?;
        if bindings.is_empty() {
            return Ok(());
        }
        // Follow returned literal aliases until their first missing destination.
        // Configurable primary aliases retain the existing package boundary.
        use crate::proto::blaze_query::attribute::Discriminator as Kind;
        let mut returned_aliases: BTreeSet<_> = bindings
            .values()
            .filter(|actual| {
                actual.r#type == Kind::Label && actual.selector_list.as_option().is_none()
            })
            .filter_map(|actual| actual.string_value.as_ref())
            .filter(|name| ordinary_names.contains(&canonical_query_label(name, &mapping)))
            .cloned()
            .collect();
        let mut inspected = HashSet::new();
        while !returned_aliases.is_empty() {
            let names: BTreeSet<_> = std::mem::take(&mut returned_aliases)
                .into_iter()
                .filter(|name| inspected.insert(canonical_query_label(name, &mapping).into_owned()))
                .collect();
            if names.is_empty() {
                break;
            }
            for (name, actual) in self.alias_actuals(&names, version)? {
                if actual.r#type != Kind::Label || actual.selector_list.as_option().is_some() {
                    continue;
                }
                if let Some(destination) = actual.string_value.as_ref() {
                    if ordinary_names.contains(&canonical_query_label(destination, &mapping)) {
                        returned_aliases.insert(destination.clone());
                    }
                    bindings.insert(name, actual);
                }
            }
        }
        let mut missing = BTreeSet::new();
        for actual in bindings.values() {
            for destination in scalar_label_values(actual) {
                if !primary.contains(&canonical_query_label(destination, &mapping)) {
                    missing.insert(destination.clone());
                }
            }
        }
        let mut strict = self.clone();
        strict.keep_going = false;
        strict
            .command_options
            .extend(["--nokeep_going".into(), "--proto:rule_classes".into()]);
        // The dependency and inventory queries below can reach arbitrary rules. Flattening a
        // dict attribute enumerates every combination of its concatenated selects, which
        // runs Bazel out of memory on a single target; hash those rows unflattened instead.
        let mut supplemental = strict.clone();
        supplemental
            .command_options
            .push("--noproto:flatten_selects".into());
        let mut reached_aliases = BTreeSet::new();
        let mut sources = BTreeMap::new();
        let mut extra = if missing.is_empty() {
            Vec::new()
        } else {
            let expression = self.with_exclude_filter(format!(
                "let needed = deps({}) in $needed + buildfiles($needed)",
                query_set(&missing)?
            ));
            let mut closure = supplemental.clone();
            closure.command_options.push("--proto:locations".into());
            closure.query_with_filtered(
                &expression,
                false,
                Some(version),
                false,
                &mut |batch, _| transform(batch, None),
                |row| {
                    if let Some(source) = row
                        .source_file
                        .as_option()
                        .filter(|s| missing.contains(&s.name))
                    {
                        AliasSource::observe(source, &mut sources);
                    }
                    // A dependency can lead back to an alias in the primary query. Keep its
                    // binding even though the existing target row remains authoritative.
                    if let Some(rule) = row
                        .rule
                        .as_option()
                        .filter(|rule| rule.rule_class_key.as_deref() == Some("alias"))
                    {
                        reached_aliases.insert(rule.name.clone());
                    }
                    target_name(row).is_some_and(|name| {
                        !primary.contains(&canonical_query_label(name, &mapping))
                    })
                },
            )?
        };
        ordinary_aliases.extend(extra.iter().flat_map(target_inputs).filter_map(|input| {
            let name = canonical_query_label(input, &mapping);
            ordinary_names.contains(&name).then(|| name.into_owned())
        }));
        if !ordinary_aliases.is_empty() {
            // Only a Rule row proves its package table was evaluated. buildfiles()
            // can also return synthetic load-file rows from other packages.
            let exposed: BTreeSet<_> = extra
                .iter()
                .filter_map(|row| row.rule.as_option())
                .filter(|rule| main_repo_label_key(&rule.name).starts_with('@'))
                .filter_map(|rule| {
                    canonical_query_label(&rule.name, &mapping)
                        .rsplit_once(':')
                        .map(|(package, _)| package.to_owned())
                })
                .collect();
            let extra_names: HashSet<_> = extra
                .iter()
                .filter_map(target_name)
                .map(|name| canonical_query_label(name, &mapping))
                .collect();
            let missing_exposed = |name: &str| {
                !primary.contains(name)
                    && !extra_names.contains(name)
                    && name
                        .rsplit_once(':')
                        .is_some_and(|(package, _)| exposed.contains(package))
            };
            let inspect = ordinary_aliases
                .into_iter()
                .filter(|name| {
                    inside
                        .get(name.as_str())
                        .and_then(|row| row.rule.as_option())
                        .is_some_and(|rule| {
                            rule.rule_input.iter().any(|input| {
                                missing_exposed(&canonical_query_label(input, &mapping))
                            })
                        })
                })
                .collect();
            let mut requested = BTreeSet::new();
            for (name, actual) in self.alias_actuals(&inspect, version)? {
                requested.extend(scalar_label_values(&actual).filter_map(|child| {
                    let child = canonical_query_label(child, &mapping);
                    missing_exposed(&child).then(|| child.into_owned())
                }));
                bindings.insert(name, actual);
            }
            if !requested.is_empty() {
                let packages: BTreeSet<_> = requested
                    .iter()
                    .filter_map(|name| name.rsplit_once(':').map(|(package, _)| package.to_owned()))
                    .collect();
                // A literal all-targets target can shadow the wildcard. Separate siblings
                // expressions also avoid released Bazel's cross-repository package-path dedup.
                let expression = packages
                    .iter()
                    .map(|package| {
                        let pattern = format!("{package}:all-targets");
                        query_labels(std::iter::once(&pattern))
                            .map(|label| format!("siblings({label})"))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .join(" + ");
                let records = supplemental.query_with(
                    &self.with_exclude_filter(expression),
                    false,
                    Some(version),
                    false,
                    &mut |_, _| {},
                )?;
                let mut inventory: BTreeMap<_, _> = records
                    .into_iter()
                    .filter_map(|row| {
                        let name = canonical_query_label(target_name(&row)?, &mapping).into_owned();
                        name.rsplit_once(':')
                            .is_some_and(|(package, _)| packages.contains(package))
                            .then_some((name, row))
                    })
                    .collect();
                let mut pending: Vec<_> = requested.into_iter().collect();
                let mut rows = Vec::new();
                while let Some(name) = pending.pop() {
                    let Some(row) = inventory.remove(&name) else {
                        continue;
                    };
                    pending.extend(
                        target_inputs(&row)
                            .map(|input| canonical_query_label(input, &mapping).into_owned()),
                    );
                    if !primary.contains(name.as_str()) && !extra_names.contains(name.as_str()) {
                        rows.push(row);
                    }
                }
                transform(&mut rows, None);
                extra.extend(rows);
            }
        }
        reached_aliases.extend(
            extra
                .iter()
                .filter_map(|target| target.rule.as_option())
                .filter(|r| r.rule_class_key.as_deref() == Some("alias"))
                .map(|r| r.name.clone()),
        );
        bindings.extend(self.alias_actuals(&reached_aliases, version)?);
        // Primary and supplemental rows may have been prehashed, which releases their
        // projected attributes. Re-read only these native aliases with the original options,
        // then use their evaluated actual, including the mapping of select keys to labels.
        let names = bindings.keys().cloned().collect();
        let mut aliases_with_attrs = strict.query_with(
            &self.with_exclude_filter(query_set(&names)?),
            false,
            Some(version),
            false,
            &mut |_, _| {},
        )?;
        let mut alias_attrs = HashMap::new();
        for row in &mut aliases_with_attrs {
            if let Some(rule) = row.rule.as_option_mut() {
                if let Some(actual) = bindings.remove(&rule.name) {
                    let source = if actual.r#type == Kind::Label
                        && actual.selector_list.as_option().is_none()
                    {
                        actual
                            .string_value
                            .as_ref()
                            .and_then(|name| sources.get(name))
                            .cloned()
                            .flatten()
                    } else {
                        None
                    };
                    rule.attribute.retain(|attr| attr.name != "actual");
                    rule.attribute.push(actual);
                    alias_attrs.insert(
                        rule.name.clone(),
                        (std::mem::take(&mut rule.attribute), source),
                    );
                }
            }
        }
        if !bindings.is_empty() {
            bail!("Bazel did not return all observed forwarding aliases");
        }
        let first = targets.len();
        targets.extend(extra);
        let definitions = self.package_inputs_for_acquired_rules(targets, first, Some(version))?;
        for (index, row) in targets.iter_mut().enumerate() {
            let Some(rule) = row.rule.as_option_mut() else {
                continue;
            };
            if let Some((attrs, source)) = alias_attrs.remove(&rule.name) {
                rule.attribute = attrs;
                if let Some(source) = source {
                    alias_sources.insert(rule.name.clone(), source);
                }
                // Hash the alias rule itself. A generated BUILD can define unrelated siblings.
                transform(std::slice::from_mut(row), None);
                continue;
            }
            if index < first {
                continue;
            }
            if let Some((package, _)) = rule.name.rsplit_once(':') {
                if let Some(loads) = definitions.get(package) {
                    rule.rule_input.extend(loads.iter().cloned());
                    rule.rule_input.sort_unstable();
                    rule.rule_input.dedup();
                }
            }
        }
        Ok(())
    }

    /// Collect loading inputs for newly acquired rules. `buildfiles` can report a
    /// synthetic load-only BUILD at the same label as the real BUILD. Refresh only
    /// ambiguous rows in packages containing an acquired rule; unrelated load-only
    /// packages must not be evaluated as target dependencies.
    fn package_inputs_for_acquired_rules(
        &self,
        targets: &mut [Target],
        first: usize,
        version: Option<BazelVersion>,
    ) -> Result<HashMap<String, Vec<String>>> {
        let mut definitions: HashMap<String, Vec<String>> = targets[first..]
            .iter()
            .filter_map(|row| row.rule.as_option().map(|rule| &rule.name))
            .filter_map(|name| name.rsplit_once(':'))
            .map(|(package, _)| (package.to_owned(), Vec::new()))
            .collect();
        if definitions.is_empty() {
            return Ok(definitions);
        }
        let builds: Vec<_> = targets
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let source = row.source_file.as_option()?;
                let (package, "BUILD" | "BUILD.bazel") = source.name.rsplit_once(':')? else {
                    return None;
                };
                definitions.contains_key(package).then_some(index)
            })
            .collect();
        let requested: BTreeSet<_> = builds
            .iter()
            .filter_map(|&index| targets[index].source_file.as_option())
            .filter(|source| source.package_contains_errors.is_none())
            .map(|source| source.name.clone())
            .collect();
        if !requested.is_empty() {
            let mut strict = self.clone();
            strict.keep_going = false;
            strict.command_options.push("--nokeep_going".into());
            let expression = self.with_exclude_filter(query_set(&requested)?);
            match strict.query_with(&expression, false, version, false, &mut |_, _| {}) {
                Ok(rows) => {
                    let mut observed = HashMap::new();
                    for row in rows {
                        let Some(name) = target_name(&row).filter(|name| requested.contains(*name))
                        else {
                            continue;
                        };
                        let loads = row.source_file.as_option().and_then(|source| {
                            (source.package_contains_errors == Some(false))
                                .then(|| source.subinclude.clone())
                        });
                        observed
                            .entry(name.to_owned())
                            .and_modify(|value| *value = None)
                            .or_insert(loads);
                    }
                    for &index in &builds {
                        let source = targets[index].source_file.as_option_mut().unwrap();
                        if let Some(Some(loads)) = observed.get(&source.name) {
                            source.subinclude.clone_from(loads);
                            source.package_contains_errors = Some(false);
                        }
                    }
                }
                Err(error) => {
                    self.check_cancelled()?;
                    eprintln!("[Warn] retaining prior BUILD loading inputs after query failure: {error:#}");
                }
            }
        }
        for index in builds {
            let source = targets[index].source_file.as_option().unwrap();
            let package = source.name.rsplit_once(':').unwrap().0;
            let inputs = definitions.get_mut(package).unwrap();
            inputs.push(source.name.clone());
            inputs.extend(source.subinclude.iter().cloned());
            inputs.sort_unstable();
            inputs.dedup();
        }
        Ok(definitions)
    }

    fn alias_actuals(
        &self,
        aliases: &BTreeSet<String>,
        version: BazelVersion,
    ) -> Result<BTreeMap<String, Attribute>> {
        if aliases.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut metadata = self.clone();
        metadata.keep_going = false;
        metadata.command_options.extend([
            "--nokeep_going".into(),
            "--proto:output_rule_attrs=actual".into(),
            "--proto:rule_classes".into(),
            "--noproto:flatten_selects".into(),
        ]);
        let records = metadata.query_with(
            &self.with_exclude_filter(query_set(aliases)?),
            false,
            Some(version),
            false,
            &mut |_, _| {},
        )?;
        Ok(records
            .into_iter()
            .filter_map(|target| {
                let rule = target.rule.into_option()?;
                if rule.rule_class_key.as_deref() != Some("alias") {
                    return None;
                }
                let actual = rule.attribute.into_iter().find(|a| a.name == "actual")?;
                Some((rule.name, actual))
            })
            .collect())
    }

    /// `external_package` is whether `//external` exists, when known; `Some(false)` leaves it out
    /// of the query instead of trying it and retrying without it.
    fn collect_all_targets(
        &self,
        bzlmod_repos: Vec<Target>,
        external_package: Option<bool>,
        mut query: impl FnMut(&str, bool) -> Result<Vec<Target>>,
    ) -> Result<Vec<Target>> {
        let include_external = !self.exclude_external_targets && external_package != Some(false);
        let mut queries = Vec::new();
        if include_external {
            queries.push("'//external:all-targets'".to_owned());
        }
        if self.use_cquery {
            let expression = self
                .cquery_root_expression
                .as_deref()
                .map(|expression| format!("deps({})", Self::configured_root_query(expression)))
                .or_else(|| self.cquery_expression.clone())
                .unwrap_or_else(|| "deps(//...:all-targets)".to_owned());
            let mut targets = query(&self.with_exclude_filter(expression), true)?;
            if include_external {
                match query("'//external:all-targets'", false) {
                    Ok(external) => targets.extend(external),
                    Err(error) if is_external_package_error(&error.to_string()) => {
                        eprintln!(
                            "[Warn] //external is not available; dropping //external:all-targets"
                        );
                    }
                    Err(error) => return Err(error),
                }
            }
            targets.extend(bzlmod_repos);
            deduplicate_targets(targets)
        } else {
            queries.push("'//...:all-targets'".to_owned());
            queries.extend(
                self.fine_grained_external_repos
                    .iter()
                    .map(|repo| format!("'{repo}//...:all-targets'")),
            );
            let full = self.with_exclude_filter(queries.join(" + "));
            match query(&full, false) {
                Ok(mut targets) => {
                    targets.extend(bzlmod_repos);
                    deduplicate_targets(targets)
                }
                Err(error) if include_external && is_external_package_error(&error.to_string()) => {
                    let without_external = self.with_exclude_filter(
                        std::iter::once("'//...:all-targets'".to_owned())
                            .chain(
                                self.fine_grained_external_repos
                                    .iter()
                                    .map(|repo| format!("'{repo}//...:all-targets'")),
                            )
                            .collect::<Vec<_>>()
                            .join(" + "),
                    );
                    let mut targets = query(&without_external, false)?;
                    targets.extend(bzlmod_repos);
                    deduplicate_targets(targets)
                }
                Err(error) => Err(error),
            }
        }
    }
    fn query_bzlmod_repos(
        &self,
        root_mapping: Option<Vec<u8>>,
        survey: ModuleSurvey,
    ) -> Result<BzlmodRepos> {
        let mut failed = FailedExtensions::default();
        // The initial probe returns stdout only on success. The fallback may be partial.
        let retain_root_mapping = root_mapping.is_some();
        let root_mapping = match root_mapping {
            Some(mapping) => mapping,
            None => {
                let output = self.run_capture(&["mod", "dump_repo_mapping", ""])?;
                self.accept_mod_output(output, "dump_repo_mapping", &mut failed)?
            }
        };
        let canonical_to_apparent = parse_repo_mapping(&String::from_utf8_lossy(&root_mapping));
        let (module_graph, complete_graph) =
            match self.module_graph_json_tolerating(survey.graph, &mut failed) {
                Ok((json, complete)) => (Some(json), complete),
                Err(error) if self.keep_going && self.isolated_dependency_root.is_none() => {
                    eprintln!("[Warn] {error:#}; hashing Bzlmod repositories without module edges");
                    (None, false)
                }
                Err(error) => return Err(error),
            };
        let module_edges = module_graph
            .as_deref()
            .map(parse_module_dependency_edges)
            .unwrap_or_default();
        let mut canonical_names = canonical_to_apparent
            .keys()
            .filter(|name| name.contains('+') || name.contains('~'))
            .map(|name| format!("@@{name}"))
            .collect::<BTreeSet<_>>();
        // Modules the root does not declare (e.g. one only a dependency pulls in under its own
        // apparent name) are absent from the root's repo mapping, yet sit on the path between
        // root-visible repos. Ask for them by module key so that path stays connected (#510).
        let hidden_modules = module_graph
            .as_deref()
            .map(|json| {
                if self.isolated_dependency_root.is_some() && complete_graph {
                    isolated_module_repo_names(json, &canonical_to_apparent)
                } else {
                    Ok(hidden_module_repo_names(json, &canonical_to_apparent))
                }
            })
            .transpose()?;
        let hidden_modules = hidden_modules.unwrap_or_default();
        canonical_names.extend(hidden_modules.keys().cloned());
        // Repositories a module extension generates without any module importing them (a Go
        // SDK behind a toolchain, the crate behind a hub alias) are in no module's repo mapping,
        // so nothing above names them. Enumerate them from the extensions' own mappings (#522).
        let extension_repos = match self.extension_repositories(
            &canonical_to_apparent,
            hidden_modules.values().cloned().collect(),
            survey.mappings,
            retain_root_mapping && complete_graph,
            &mut failed,
        ) {
            Ok(repos) => repos,
            Err(error) if self.keep_going && self.isolated_dependency_root.is_none() => {
                eprintln!("[Warn] failed to enumerate module extension repositories: {error:#}");
                ExtensionRepositories::default()
            }
            Err(error) => return Err(error),
        };
        canonical_names.extend(
            extension_repos
                .generated
                .iter()
                .map(|name| format!("@@{name}")),
        );
        let repositories = if canonical_names.is_empty() {
            Vec::new()
        } else {
            self.show_repositories_within(
                &canonical_names.into_iter().collect::<Vec<_>>(),
                &mut failed,
                mod_argument_byte_budget(),
                || {
                    verified_hidden_modules(
                        module_graph.as_deref(),
                        &root_mapping,
                        &extension_repos.module_mappings,
                        &hidden_modules,
                    )
                    .then_some(&hidden_modules)
                },
            )?
        };
        let mut scope = IsolatedRepoScope::default();
        let label_only = LabelOnlyRepositories {
            extensions: self
                .label_only_extension_prefixes(&canonical_to_apparent, &extension_repos.generated),
            owner_exemptions: if let Some(root) = &self.isolated_dependency_root {
                if let Some(extension) = failed.extensions.first() {
                    bail!("module extension {extension} failed");
                }
                if let Some(repository) = failed.unavailable.first() {
                    bail!("repository mapping for {repository} failed");
                }
                if failed.observed {
                    bail!("Bazel reported an unidentified module extension failure");
                }
                if !retain_root_mapping {
                    bail!("root repository mapping is incomplete");
                }
                if !complete_graph {
                    bail!("module graph is incomplete");
                }
                if !extension_repos.complete_mappings {
                    bail!("repository mappings are incomplete");
                }
                scope =
                    resolve_isolated_root(root, &root_mapping, &extension_repos, &repositories)?;
                scope.members.clone()
            } else {
                BTreeSet::new()
            },
        };
        Ok(BzlmodRepos {
            definitions: Some(RepositoryDefinitions {
                repositories,
                module_edges,
                extensions: extension_repos,
                label_only,
            }),
            failed_extensions: failed,
            canonical_to_apparent,
            module_graph_json: module_graph,
            module_graph_complete: complete_graph,
            root_mapping: retain_root_mapping.then_some(root_mapping),
            scope,
        })
    }

    /// The usage prefixes (`rules_rs++crate`) of `label_only_module_extensions`. An entry that
    /// does not parse, names a module the root cannot see, or matches no generated repository is
    /// reported and ignored, which leaves that extension on the conservative edges. So is one
    /// whose prefix Bazel may have given to another extension: two `.bzl` files of one module
    /// that export the same extension name are told apart only by a numeric suffix.
    fn label_only_extension_prefixes(
        &self,
        canonical_to_apparent: &BTreeMap<String, Vec<String>>,
        generated: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        let prefixes_in_use = generated
            .iter()
            .filter_map(|repo| extension_usage_prefix(repo))
            .collect::<BTreeSet<_>>();
        let mut prefixes = BTreeSet::new();
        for id in &self.label_only_module_extensions {
            let parsed = id.rsplit_once('%').and_then(|(file, name)| {
                let (repo, _) = file.split_once("//")?;
                Some((repo, name))
            });
            let Some((repo, name)) = parsed.filter(|(_, name)| !name.is_empty()) else {
                eprintln!(
                    "[Warn] --labelOnlyModuleExtensions: {id} is not of the form @module//path.bzl%extension"
                );
                continue;
            };
            let canonical = match repo.strip_prefix("@@") {
                Some(canonical) => Some(canonical.to_owned()),
                None => {
                    let apparent = repo.trim_start_matches('@');
                    canonical_to_apparent
                        .iter()
                        .find(|(_, names)| names.iter().any(|name| name == apparent))
                        .map(|(canonical, _)| canonical.clone())
                }
            };
            let Some(prefix) = canonical
                .map(|canonical| format!("{canonical}+{name}"))
                .filter(|prefix| prefixes_in_use.contains(prefix.as_str()))
            else {
                eprintln!(
                    "[Warn] --labelOnlyModuleExtensions: {id} generates no repository the root module can see"
                );
                continue;
            };
            let disambiguated = prefixes_in_use.iter().any(|other| {
                other.strip_prefix(prefix.as_str()).is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                })
            });
            if disambiguated {
                eprintln!(
                    "[Warn] --labelOnlyModuleExtensions: {id} shares its name with another extension of its module, so its repositories cannot be told apart"
                );
                continue;
            }
            prefixes.insert(prefix);
        }
        prefixes
    }

    /// Canonical names that fit one invocation are asked for by name. More are asked of
    /// `--all_repos` in a single invocation, filtered to the names wanted: every invocation costs
    /// a Bazel server round trip (seconds on Windows, whose `cmd.exe` limit is the one that splits
    /// the names most finely, #536), and the names are nearly every repository anyway. A Bazel
    /// without `--all_repos`, or one whose answer a module extension it cannot evaluate
    /// spoils, is asked by name after all, one invocation per chunk.
    fn show_repositories_within<'a>(
        &self,
        canonical_names: &[String],
        failed: &mut FailedExtensions,
        budget: usize,
        hidden_bindings: impl FnOnce() -> Option<&'a BTreeMap<String, String>>,
    ) -> Result<Vec<Repository>> {
        // Hidden module keys start with a lowercase module name, after canonical @@ arguments.
        let canonical_count = canonical_names.partition_point(|name| name.starts_with("@@"));
        let (canonical, hidden) = canonical_names.split_at(canonical_count);
        let selected_bytes = canonical.iter().map(|name| name.len() + 1).sum::<usize>();
        if selected_bytes > budget {
            match self.show_all_repositories(failed) {
                Ok(Some(BulkRepositories {
                    mut by_canonical,
                    complete,
                })) => {
                    // A module-key request prints its canonical definition with one extra
                    // field. Reuse the bulk answer only when the parents' own repo mappings
                    // identify every key and the bulk command answered completely.
                    let hidden_records =
                        complete
                            .then(hidden_bindings)
                            .flatten()
                            .and_then(|bindings| {
                                hidden
                                    .iter()
                                    .map(|key| {
                                        let name = bindings.get(key)?;
                                        let record = by_canonical.get(name)?;
                                        if record.module_key.is_some()
                                            || record.apparent_name.is_some()
                                        {
                                            return None;
                                        }
                                        let mut record = record.clone();
                                        record.module_key = Some(key.clone());
                                        Some(record)
                                    })
                                    .collect::<Option<Vec<_>>>()
                            });
                    // RepoOverride has no streamed-proto record, so missing canonical slots
                    // remain omitted, in the same order as an explicit read.
                    let mut repositories: Vec<_> = canonical
                        .iter()
                        .filter_map(|name| by_canonical.remove(name.trim_start_matches('@')))
                        .collect();
                    repositories.extend(match hidden_records {
                        Some(records) => records,
                        None => self.show_repositories_explicit(hidden, failed, budget)?,
                    });
                    return Ok(repositories);
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("[Warn] {error:#}; reading selected repositories explicitly");
                }
            }
            self.check_cancelled()?;
        }
        self.show_repositories_explicit(canonical_names, failed, budget)
    }

    fn show_repositories_explicit(
        &self,
        canonical_names: &[String],
        failed: &mut FailedExtensions,
        budget: usize,
    ) -> Result<Vec<Repository>> {
        let mut repositories = Vec::new();
        for chunk in chunk_arguments(canonical_names, budget) {
            let output_file = self.run_mod_over_repos("show_repo", chunk, failed, |names| {
                let output_file = NamedTempFile::new().context("create mod show_repo output")?;
                let mut command = self.command();
                command.arg("mod").arg("show_repo").args(names);
                command
                    .arg("--output=streamed_proto")
                    .stdout(File::create(output_file.path())?)
                    .stdin(Stdio::null())
                    .stderr(Stdio::piped());
                let output = self
                    .run_configured(&mut command)
                    .context("execute bazel mod show_repo")?;
                if output.status.success() && !output.stderr.is_empty() {
                    eprint!("{}", String::from_utf8_lossy(&output.stderr));
                }
                Ok((output.status, output.stderr, output_file))
            })?;
            if let Some(output_file) = output_file {
                repositories.extend(decode_delimited::<Repository>(output_file.path())?);
            }
        }
        Ok(repositories)
    }

    /// `bazel mod show_repo --all_repos --output=streamed_proto`. `None` when Bazel did not answer: it
    /// predates the option (every Bazel with the `--output=streamed_proto` this needs, 8.6
    /// and 9.0 on, has it), or a module extension it cannot evaluate failed the whole
    /// invocation. Failures are noted for the explicit reads that follow to identify
    /// unavailable canonical repositories.
    fn show_all_repositories(
        &self,
        failed: &mut FailedExtensions,
    ) -> Result<Option<BulkRepositories>> {
        let output_file = NamedTempFile::new().context("create mod show_repo output")?;
        let mut command = self.command();
        command
            .arg("mod")
            .arg("show_repo")
            .arg("--all_repos")
            .arg("--output=streamed_proto")
            .stdout(File::create(output_file.path())?)
            .stdin(Stdio::null())
            .stderr(Stdio::piped());
        if self.verbose {
            eprintln!("[Info] Command: {command:?}");
        }
        let output = self
            .run_configured(&mut command)
            .context("execute bazel mod show_repo --all_repos")?;
        let answered = if output.status.success() {
            true
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            match extension_failure(&stderr) {
                Some(failure) => {
                    self.note_failed_extensions(failed, failure.extensions);
                    failure.answered
                }
                None => {
                    if self.verbose {
                        eprintln!(
                            "[Info] bazel mod show_repo --all_repos failed with {}, asking by name instead: {}",
                            output.status,
                            stderr.trim()
                        );
                    }
                    false
                }
            }
        };
        if !answered {
            return Ok(None);
        }
        if !output.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        let mut complete = output.status.success() && failed.is_empty();
        let mut by_canonical = HashMap::new();
        for repository in decode_delimited::<Repository>(output_file.path())? {
            let Some(canonical) = repository.canonical_name.as_deref() else {
                complete = false;
                continue;
            };
            let canonical = canonical.trim_start_matches('@');
            if !has_unavailable_mapping(canonical, failed)
                && by_canonical
                    .insert(canonical.to_owned(), repository)
                    .is_some()
            {
                complete = false;
            }
        }
        Ok(Some(BulkRepositories {
            by_canonical,
            complete,
        }))
    }

    /// Enumerates every repository generated by a module extension that at least one module
    /// imports from. Module mappings name the extension repos they `use_repo`. A repository
    /// of each such extension sees its siblings, whether or not any module imports them.
    ///
    /// Mapping failures identify the unavailable canonical repositories before excluding them.
    fn extension_repositories(
        &self,
        canonical_to_apparent: &BTreeMap<String, Vec<String>>,
        hidden_module_repos: Vec<String>,
        prefetched: BTreeMap<String, RepoMapping>,
        complete_inputs: bool,
        failed: &mut FailedExtensions,
    ) -> Result<ExtensionRepositories> {
        let visible_module_repos = canonical_to_apparent
            .keys()
            .filter(|canonical| !is_extension_repo(canonical))
            .cloned()
            .collect::<Vec<_>>();
        let mut imported = canonical_to_apparent
            .keys()
            .filter(|canonical| is_extension_repo(canonical))
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut module_repos = visible_module_repos.clone();
        module_repos.extend(hidden_module_repos.iter().cloned());
        let mut module_mappings = BTreeMap::new();
        let mut complete_mappings = complete_inputs && failed.is_empty();
        if !module_repos.is_empty() {
            // The hidden modules' repository names are derived, not reported, so a derivation
            // Bazel disagrees with falls back to the root-visible modules alone.
            let mappings = match self.module_mappings(
                &module_repos,
                prefetched,
                failed,
                &mut complete_mappings,
            ) {
                Ok(mappings) => mappings,
                Err(error) if !hidden_module_repos.is_empty() => {
                    eprintln!("[Warn] {error:#}; enumerating extension repositories from the root-visible modules only");
                    complete_mappings = false;
                    self.dump_repo_mappings(&visible_module_repos, failed, &mut complete_mappings)?
                }
                Err(error) => return Err(error),
            };
            imported.extend(
                mappings
                    .iter()
                    .flat_map(|mapping| mapping.values())
                    .filter(|canonical| is_extension_repo(canonical))
                    .cloned(),
            );
            complete_mappings &= mappings.len() == module_repos.len();
            if complete_mappings {
                module_mappings.extend(module_repos.iter().cloned().zip(mappings));
            }
        }
        imported.retain(|canonical| !has_unavailable_mapping(canonical, failed));
        let mut representatives = BTreeMap::new();
        for canonical in &imported {
            if let Some((prefix, _)) = canonical.rsplit_once('+') {
                representatives
                    .entry(prefix.to_owned())
                    .or_insert_with(|| canonical.clone());
            }
        }
        let mut generated = imported.clone();
        let mut complete_spoke_mappings = true;
        if !representatives.is_empty() {
            let names = representatives.values().cloned().collect::<Vec<_>>();
            let mappings =
                match self.dump_repo_mappings(&names, failed, &mut complete_spoke_mappings) {
                    Ok(mappings) => mappings,
                    Err(_) => {
                        complete_spoke_mappings = false;
                        // One extension that fails to evaluate should not hide the rest; retry
                        // one at a time and report the ones Bazel cannot describe.
                        let mut mappings = Vec::new();
                        for name in &names {
                            match self.dump_repo_mappings(
                                std::slice::from_ref(name),
                                failed,
                                &mut complete_mappings,
                            ) {
                                Ok(mapping) => mappings.extend(mapping),
                                Err(error) if self.keep_going => {
                                    eprintln!(
                                    "[Warn] skipping the module extension behind {name}: {error:#}"
                                );
                                }
                                Err(error) => return Err(error),
                            }
                        }
                        mappings
                    }
                };
            generated.extend(
                mappings
                    .into_iter()
                    .flat_map(|mapping| mapping.into_values())
                    .filter(|canonical| {
                        canonical
                            .rsplit_once('+')
                            .is_some_and(|(prefix, _)| representatives.contains_key(prefix))
                    }),
            );
        }
        imported.retain(|canonical| !has_unavailable_mapping(canonical, failed));
        generated.retain(|canonical| !has_unavailable_mapping(canonical, failed));
        if !complete_mappings || !complete_spoke_mappings {
            module_mappings.clear();
        }
        Ok(ExtensionRepositories {
            imported,
            generated,
            complete_mappings: complete_mappings && complete_spoke_mappings,
            module_mappings,
        })
    }

    fn module_mappings(
        &self,
        repos: &[String],
        mut prefetched: BTreeMap<String, RepoMapping>,
        failed: &mut FailedExtensions,
        complete: &mut bool,
    ) -> Result<Vec<RepoMapping>> {
        if prefetched.is_empty() {
            return self.dump_repo_mappings(repos, failed, complete);
        }
        let mut missing = Vec::new();
        let mut mappings = Vec::with_capacity(repos.len());
        for repo in repos {
            let mapping = prefetched.remove(repo);
            if mapping.is_none() {
                missing.push(repo.clone());
            }
            mappings.push(mapping);
        }
        let mut complete_missing = true;
        let answers = self.dump_repo_mappings(&missing, failed, &mut complete_missing)?;
        if !complete_missing {
            return self.dump_repo_mappings(repos, failed, complete);
        }
        let mut answers = answers.into_iter();
        Ok(mappings
            .into_iter()
            .map(|mapping| {
                mapping.unwrap_or_else(|| answers.next().expect("one answer per missing module"))
            })
            .collect())
    }

    /// `bazel mod dump_repo_mapping` over `repos` (canonical names without `@@`), one
    /// apparent -> canonical map per repository.
    fn dump_repo_mappings(
        &self,
        repos: &[String],
        failed: &mut FailedExtensions,
        complete: &mut bool,
    ) -> Result<Vec<RepoMapping>> {
        let mut mappings = Vec::new();
        for chunk in chunk_arguments(repos, mod_argument_byte_budget()) {
            let stdout = self.run_mod_over_repos("dump_repo_mapping", chunk, failed, |names| {
                let mut args = vec!["mod", "dump_repo_mapping"];
                args.extend(names.iter().map(String::as_str));
                let output = self.run_capture(&args)?;
                // Bazel prints mappings in argument order, without repository names. Keep
                // them bound to the requested names only if every original name was answered.
                *complete &= output.status.success() && names == chunk;
                Ok((output.status, output.stderr, output.stdout))
            })?;
            let Some(stdout) = stdout else {
                *complete = false;
                continue;
            };
            let start = mappings.len();
            for line in String::from_utf8_lossy(&stdout).lines() {
                match serde_json::from_str(line.trim()) {
                    Ok(mapping) => mappings.push(mapping),
                    Err(_) => *complete = false,
                }
            }
            *complete &= mappings.len() - start == chunk.len();
        }
        Ok(mappings)
    }

    /// Runs a `bazel mod` subcommand over `repos`, leaving out the repositories of module
    /// extensions Bazel cannot evaluate. Bazel answers for every other repository when such
    /// an extension merely sits in the dependency graph, but asking for one of its
    /// repositories by name fails the whole invocation with nothing printed, so the
    /// invocation is retried without them. `None` once no repository is left to ask about.
    fn run_mod_over_repos<T>(
        &self,
        subcommand: &str,
        repos: &[String],
        failed: &mut FailedExtensions,
        mut attempt: impl FnMut(&[String]) -> Result<(ExitStatus, Vec<u8>, T)>,
    ) -> Result<Option<T>> {
        let without_failed = |repos: &[String], failed: &FailedExtensions| {
            repos
                .iter()
                .filter(|repo| !has_unavailable_mapping(repo, failed))
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut repos = without_failed(repos, failed);
        loop {
            if repos.is_empty() {
                return Ok(None);
            }
            let (status, stderr, value) = attempt(&repos)?;
            if status.success() {
                return Ok(Some(value));
            }
            let stderr = String::from_utf8_lossy(&stderr);
            let Some(failure) = extension_failure(&stderr) else {
                bail!(
                    "bazel mod {subcommand} failed with {status}: {}",
                    stderr.trim()
                );
            };
            self.note_failed_extensions(failed, failure.extensions);
            if failure.answered {
                return Ok(Some(value));
            }
            self.find_unavailable_repositories(&repos, failed)?;
            let remaining = without_failed(&repos, failed);
            if remaining.len() == repos.len() {
                bail!(
                    "bazel mod {subcommand} failed with {status}: {}",
                    stderr.trim()
                );
            }
            repos = remaining;
        }
    }

    /// Identifies unavailable canonical repositories by splitting failed mapping batches.
    /// Successful batches need no per-repository calls. Unlike extension ids in diagnostics,
    /// these names include Bazel's disambiguation of equal symbols from different files.
    /// A successful mapping does not prove the internal repository itself exists.
    fn find_unavailable_repositories(
        &self,
        repos: &[String],
        failed: &mut FailedExtensions,
    ) -> Result<()> {
        let repos = repos
            .iter()
            .map(|repo| repo.trim_start_matches('@'))
            .filter(|repo| is_extension_repo(repo) && !failed.unavailable.contains(*repo))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut pending = chunk_arguments(&repos, mod_argument_byte_budget());
        pending.reverse();
        while let Some(repos) = pending.pop() {
            let mut args = vec!["mod", "dump_repo_mapping"];
            args.extend(repos.iter().map(String::as_str));
            let output = self.run_capture(&args)?;
            if output.status.success() {
                continue;
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            // dump_repo_mapping returns before the inspector's partial-answer path.
            let failure = extension_failure(&stderr).filter(|failure| !failure.answered);
            let Some(failure) = failure else {
                bail!(
                    "bazel mod dump_repo_mapping failed with {}: {}",
                    output.status,
                    stderr.trim()
                );
            };
            if let [repo] = repos {
                self.note_failed_extensions(failed, failure.extensions);
                failed.unavailable.insert(repo.clone());
            } else {
                let (left, right) = repos.split_at(repos.len() / 2);
                pending.push(right);
                pending.push(left);
            }
        }
        Ok(())
    }

    /// The stdout of a `bazel mod` invocation that takes no repository names, kept when the
    /// invocation only failed because a module extension in the graph cannot be evaluated.
    fn accept_mod_output(
        &self,
        output: Output,
        subcommand: &str,
        failed: &mut FailedExtensions,
    ) -> Result<Vec<u8>> {
        if output.status.success() {
            return Ok(output.stdout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        match extension_failure(&stderr) {
            Some(failure) if failure.answered => {
                self.note_failed_extensions(failed, failure.extensions);
                Ok(output.stdout)
            }
            _ => bail!(
                "bazel mod {subcommand} failed with {}: {}",
                output.status,
                stderr.trim()
            ),
        }
    }

    fn note_failed_extensions(&self, failed: &mut FailedExtensions, extensions: BTreeSet<String>) {
        failed.observed = true;
        for extension in extensions {
            if failed.extensions.insert(extension.clone()) {
                eprintln!(
                    "[Warn] Bazel reported that module extension {extension} cannot be evaluated"
                );
            }
        }
    }

    /// `bazel query //...:all-targets` lists repository inputs without loading their
    /// dependencies, so it does not report their failed mappings (#531). Checks those inputs
    /// after an extension failure. Returns an error, or warns under `--keep_going`.
    fn check_failed_extension_references(
        &self,
        targets: &[Target],
        failed: &mut FailedExtensions,
        canonical_to_apparent: &BTreeMap<String, Vec<String>>,
    ) -> Result<()> {
        if failed.is_empty() {
            return Ok(());
        }
        let apparent_to_canonical = canonical_to_apparent
            .iter()
            .flat_map(|(canonical, apparent)| {
                apparent
                    .iter()
                    .map(move |name| (name.as_str(), canonical.as_str()))
            })
            .collect::<BTreeMap<_, _>>();
        let mut referenced = BTreeSet::new();
        for target in targets {
            let mut labels = vec![target_name(target).unwrap_or_default()];
            if let Some(rule) = target.rule.as_option() {
                labels.extend(rule.rule_input.iter().map(String::as_str));
                labels.extend(
                    rule.configured_rule_input
                        .iter()
                        .filter_map(|input| input.label.as_deref()),
                );
            }
            for label in labels {
                let Some((repo, _)) = label.split_once("//") else {
                    continue;
                };
                let canonical = repo.strip_prefix("@@").or_else(|| {
                    let apparent = repo.strip_prefix('@')?;
                    apparent_to_canonical.get(apparent).copied()
                });
                if let Some(canonical) = canonical {
                    referenced.insert(canonical);
                }
            }
        }
        self.find_unavailable_repositories(
            &referenced
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            failed,
        )?;
        let unavailable_apparent = apparent_to_canonical
            .into_iter()
            .filter_map(|(apparent, canonical)| {
                has_unavailable_mapping(canonical, failed).then_some(apparent)
            })
            .collect::<BTreeSet<_>>();
        let unavailable = |label: &str| -> bool {
            let Some((repo, _)) = label.split_once("//") else {
                return false;
            };
            match repo.strip_prefix("@@") {
                Some(canonical) => has_unavailable_mapping(canonical, failed),
                None => repo
                    .strip_prefix('@')
                    .is_some_and(|apparent| unavailable_apparent.contains(apparent)),
            }
        };
        let mut offenders = Vec::new();
        for target in targets {
            let name = target_name(target).unwrap_or_default();
            let mut labels = vec![name];
            if let Some(rule) = target.rule.as_option() {
                labels.extend(rule.rule_input.iter().map(String::as_str));
                labels.extend(
                    rule.configured_rule_input
                        .iter()
                        .filter_map(|input| input.label.as_deref()),
                );
            }
            if let Some(label) = labels.into_iter().find(|label| unavailable(label)) {
                offenders.push(format!(
                    "{name} depends on {label}, whose repository mapping failed"
                ));
            }
        }
        if offenders.is_empty() {
            return Ok(());
        }
        let shown = offenders
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let more = offenders.len().saturating_sub(5);
        let summary = if more > 0 {
            format!("{shown}; and {more} more")
        } else {
            shown
        };
        if self.keep_going {
            eprintln!(
                "[Warn] hashes do not cover inputs with failed repository mappings: {summary}"
            );
            Ok(())
        } else {
            let reported = failed
                .extensions
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "repository mappings needed by targets failed: {summary}; reported module extensions: {reported}"
            )
        }
    }

    pub(crate) fn observed_toolchains(
        &self,
        targets: &[Target],
        root_mapping: Option<&[u8]>,
        track_readers: bool,
    ) -> Result<crate::toolchain_candidates::ObservedToolchains> {
        use crate::toolchain_candidates::{Declaration, ObservedToolchains};
        let mapping = root_mapping
            .map(|raw| crate::hash::parse_apparent_repo_mapping(&String::from_utf8_lossy(raw)))
            .unwrap_or_default();
        let canon = |s: &str| canonical_query_label(s, &mapping).into_owned();
        let rows = || {
            targets
                .iter()
                .filter_map(|row| row.rule.as_option())
                .filter(|r| !main_repo_label_key(&r.name).starts_with("//external:"))
        };
        let mut result = ObservedToolchains {
            version: 1,
            readers: track_readers.then(BTreeMap::new),
            ..Default::default()
        };
        let names: BTreeSet<_> = rows()
            .filter(|r| r.rule_class == "toolchain")
            .map(|r| canon(&r.name))
            .collect();
        let mut types: BTreeSet<_> = rows()
            .filter(|r| r.rule_class == "toolchain_type")
            .map(|r| canon(&r.name))
            .collect();
        let mut metadata = self.clone();
        metadata.keep_going = false;
        metadata.command_options.extend([
            "--nokeep_going".into(),
            "--consistent_labels".into(),
            "--proto:rule_classes".into(),
            "--noproto:flatten_selects".into(),
            "--proto:output_rule_attrs=toolchain,toolchain_type,actual".into(),
        ]);
        let version = if names.is_empty() {
            None
        } else {
            Some(self.version()?)
        };
        let read = |labels: &BTreeSet<String>| -> Result<Vec<Target>> {
            if labels.is_empty() {
                return Ok(Vec::new());
            }
            let names = query_labels(labels.iter())?;
            let expression = self.with_exclude_filter(names);
            metadata.query_with(&expression, false, version, false, &mut |_, _| {})
        };
        for row in read(&names)? {
            let Some(rule) = row.rule.into_option() else {
                continue;
            };
            let id = canon(&rule.name);
            if rule.rule_class_key.as_deref() != Some("toolchain") {
                result.unresolved.insert(id);
                continue;
            }
            let labels = |attribute: &str| {
                rule.attribute
                    .iter()
                    .filter(|a| a.name == attribute)
                    .flat_map(scalar_label_values)
                    .filter(|v| v.starts_with("//") || v.starts_with('@'))
                    .map(|v| canon(v))
                    .collect::<BTreeSet<_>>()
            };
            let declaration = Declaration {
                types: labels("toolchain_type"),
                implementations: labels("toolchain"),
            };
            if declaration.types.is_empty() || declaration.implementations.is_empty() {
                result.unresolved.insert(id.clone());
            }
            result.declarations.insert(id, declaration);
        }
        result.unresolved.extend(
            names
                .iter()
                .filter(|n| !result.declarations.contains_key(*n))
                .cloned(),
        );
        let returned_aliases: HashSet<_> = rows()
            .filter(|r| r.rule_class == "alias")
            .map(|r| canon(&r.name))
            .collect();
        let returned_types = types.clone();
        let mut pending: BTreeSet<_> = result
            .declarations
            .values()
            .flat_map(|d| d.types.iter().cloned())
            .collect();
        let mut seen = BTreeSet::new();
        let mut aliases = BTreeMap::new();
        while !pending.is_empty() {
            let mut requested = BTreeSet::new();
            for ty in std::mem::take(&mut pending) {
                if !seen.insert(ty.clone()) {
                    continue;
                }
                if returned_aliases.contains(&ty) {
                    requested.insert(ty);
                } else if !returned_types.contains(&ty) {
                    result.unresolved.insert(ty);
                }
            }
            let alias_rows = match read(&requested) {
                Ok(rows) => rows,
                Err(error) => {
                    self.check_cancelled()?;
                    result.unresolved.extend(requested);
                    eprintln!("[Warn] observed type aliases could not be read: {error:#}");
                    break;
                }
            };
            for row in alias_rows {
                let Some(rule) = row.rule.into_option() else {
                    continue;
                };
                if rule.rule_class_key.as_deref() != Some("alias") {
                    continue;
                }
                let actual = rule
                    .attribute
                    .iter()
                    .find(|a| a.name == "actual" && a.selector_list.as_option().is_none())
                    .and_then(|a| a.string_value.as_ref());
                if let Some(actual) = actual.filter(|v| v.starts_with("//") || v.starts_with('@')) {
                    let destination = canon(actual);
                    pending.insert(destination.clone());
                    aliases.insert(canon(&rule.name), destination);
                }
            }
            result
                .unresolved
                .extend(requested.into_iter().filter(|ty| !aliases.contains_key(ty)));
        }
        for declaration in result.declarations.values_mut() {
            let mut work: Vec<_> = declaration.types.iter().cloned().collect();
            while let Some(ty) = work.pop() {
                types.insert(ty.clone());
                if let Some(next) = aliases.get(&ty) {
                    if declaration.types.insert(next.clone()) {
                        work.push(next.clone());
                    }
                }
            }
            if declaration.types.is_disjoint(&returned_types) {
                result.unresolved.extend(declaration.types.iter().cloned());
            }
        }
        let needed: BTreeSet<&str> = result
            .declarations
            .iter()
            .flat_map(|(name, d)| {
                std::iter::once(name)
                    .chain(&d.implementations)
                    .chain(&d.types)
            })
            .map(String::as_str)
            .collect();
        let mut spellings: HashMap<_, _> = types.iter().map(|t| (t.clone(), t)).collect();
        let mut by_repo: HashMap<&str, Vec<&String>> = HashMap::new();
        for (apparent, canonical) in &mapping {
            by_repo.entry(canonical).or_default().push(apparent);
        }
        for ty in &types {
            if ty.starts_with("//") {
                spellings.insert(format!("@{ty}"), ty);
                spellings.insert(format!("@@{ty}"), ty);
            } else if let Some((repo, suffix)) =
                ty.strip_prefix("@@").and_then(|s| s.split_once("//"))
            {
                for apparent in by_repo.get(repo).into_iter().flatten() {
                    spellings.insert(format!("@{apparent}//{suffix}"), ty);
                }
            }
        }
        if let Some(readers) = &mut result.readers {
            readers.extend(types.iter().map(|ty| (ty.clone(), Vec::new())));
        }
        for row in targets {
            let Some(name) = target_name(row) else {
                continue;
            };
            let id = canonical_query_label(name, &mapping);
            if needed.contains(id.as_ref()) || types.contains(id.as_ref()) {
                result.hash_labels.insert(id.into_owned(), name.to_owned());
            }
            if let Some(readers) = &mut result.readers {
                for ty in row
                    .rule
                    .as_option()
                    .into_iter()
                    .flat_map(|r| &r.rule_input)
                    .filter_map(|v| spellings.get(v))
                {
                    if let Some(members) = readers.get_mut(*ty) {
                        members.push(name.to_owned());
                    }
                }
            }
        }
        if let Some(readers) = &mut result.readers {
            for members in readers.values_mut() {
                members.sort_unstable();
                members.dedup();
            }
        }
        Ok(result)
    }

    pub fn query(&self, expression: &str, use_cquery: bool) -> Result<Vec<Target>> {
        self.query_with(expression, use_cquery, None, false, &mut |_, _| {})
    }

    fn query_with(
        &self,
        expression: &str,
        use_cquery: bool,
        version: Option<BazelVersion>,
        lean: bool,
        transform: &mut impl FnMut(&mut [Target], &EncodedTargets<'_>),
    ) -> Result<Vec<Target>> {
        self.query_with_filtered(expression, use_cquery, version, lean, transform, |_| true)
    }

    fn query_with_filtered(
        &self,
        expression: &str,
        use_cquery: bool,
        version: Option<BazelVersion>,
        lean: bool,
        transform: &mut impl FnMut(&mut [Target], &EncodedTargets<'_>),
        retain: impl FnMut(&Target) -> bool,
    ) -> Result<Vec<Target>> {
        let graph = self.query_with_capture(
            expression,
            use_cquery,
            version,
            lean,
            transform,
            CqueryCapture::Standard,
            retain,
        )?;
        Ok(graph.targets)
    }

    fn query_configured_roots(
        &self,
        expression: &str,
        version: Option<BazelVersion>,
    ) -> Result<BTreeMap<String, String>> {
        let version = match version {
            Some(version) => version,
            None => self.version()?,
        };
        let mut resolved = Vec::new();
        let query = self.with_exclude_filter(Self::configured_root_query(expression));
        if let Err(error) = self.query_with_capture(
            &query,
            true,
            Some(version),
            false,
            &mut |_, _| {},
            CqueryCapture::Roots(&mut resolved),
            |_| true,
        ) {
            let selection = self.with_exclude_filter(expression.to_owned());
            let output = self
                .query_formatted(
                    &selection,
                    "def format(target):\n    return str(target.label)\n",
                    version,
                )
                .with_context(|| format!("configured root query failed: {error:#}"))?;
            if !output.status.success() {
                bail!(
                    "configured root query failed ({error:#}); root selection check failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            if output.stdout.iter().all(u8::is_ascii_whitespace) {
                return Ok(BTreeMap::new());
            }
            return Err(error);
        }
        let mut roots = BTreeMap::new();
        for (label, key) in resolved {
            match roots.entry(label) {
                Entry::Vacant(entry) => {
                    entry.insert(key);
                }
                Entry::Occupied(entry) if entry.get() != &key => {
                    bail!(
                        "configured root {} resolved to multiple configurations",
                        entry.key()
                    );
                }
                Entry::Occupied(_) => {}
            }
        }
        Ok(roots)
    }

    #[allow(clippy::too_many_arguments)]
    fn query_with_capture(
        &self,
        expression: &str,
        use_cquery: bool,
        version: Option<BazelVersion>,
        lean: bool,
        transform: &mut impl FnMut(&mut [Target], &EncodedTargets<'_>),
        capture: CqueryCapture<'_>,
        retain: impl FnMut(&Target) -> bool,
    ) -> Result<QueryGraph> {
        let (mut roots, verify) = match capture {
            CqueryCapture::Standard => (None, false),
            CqueryCapture::Roots(roots) => (Some(roots), false),
            CqueryCapture::Verification => (None, true),
        };
        let rooted = use_cquery && self.cquery_root_expression.is_some();
        let version = match version {
            Some(version) => version,
            None => self.version()?,
        };
        let compatible = if use_cquery && !verify {
            Some(self.query_compatible_labels(expression, version, false)?)
        } else {
            None
        };
        let query_file = NamedTempFile::new().context("create query file")?;
        fs::write(query_file.path(), expression).context("write query file")?;
        let output_file = NamedTempFile::new().context("create query output file")?;
        let streamed_cquery = version >= BazelVersion(7, 0, 0);
        let supports_output_file = version >= BazelVersion(8, 2, 0);
        let output_format = if use_cquery && !streamed_cquery {
            "proto"
        } else {
            "streamed_proto"
        };

        let mut command = self.command();
        command.arg(if use_cquery { "cquery" } else { "query" });
        if !use_cquery {
            command.arg("--output").arg(output_format);
        }
        command.arg("--proto:instantiation_stack");
        if !use_cquery {
            command.arg("--order_output=no");
        }
        if self.keep_going {
            command.arg("--keep_going");
        }
        if use_cquery {
            command.args(&self.cquery_options);
            if verify {
                // Verification needs the complete query traversal. These settings affect only
                // dependency projection; the caller's configuration and repository options stay.
                command.args([
                    "--experimental_explicit_aspects",
                    "--implicit_deps",
                    "--tool_deps",
                    "--nodep_deps",
                    "--include_aspects",
                    "--proto:output_rule_attrs=all",
                    "--proto:default_values",
                    "--proto:definition_stack",
                    "--proto:rule_classes",
                    "--proto:locations",
                    "--norelative_locations",
                ]);
            }
            command
                .arg("--transitions=lite")
                .arg("--output")
                .arg(output_format);
            if rooted {
                // These fields carry configured identities and the existing repository inputs.
                command
                    .arg("--proto:include_configurations")
                    .arg("--proto:rule_inputs_and_outputs");
            }
            command.arg("--consistent_labels");
        } else {
            command.args(&self.command_options);
        }
        command.arg("--query_file").arg(query_file.path());
        if supports_output_file {
            command.arg("--output_file").arg(output_file.path());
            command.stdout(Stdio::null());
        } else {
            command.stdout(File::create(output_file.path())?);
        }
        command.stdin(Stdio::null()).stderr(Stdio::piped());
        if self.verbose {
            eprintln!("[Info] Executing Query: {expression}");
            eprintln!("[Info] Command: {command:?}");
        }
        let output = self
            .run_configured(&mut command)
            .context("failed to execute Bazel query")?;
        let code = output.status.code().unwrap_or(-1);
        // A formatter can emit its completion marker before a keep-going query returns 3.
        // Verification requires command success; ordinary partial positives keep their policy.
        let partial = self.keep_going && code == 3 && !verify;
        if code != 0 && !partial {
            return Err(QueryFailure {
                code,
                message: format!(
                    "Bazel query failed, exit code {code}: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            }
            .into());
        }
        if !output.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }

        let mut graph = if use_cquery && verify {
            decode_cquery_graph(
                output_file.path(),
                streamed_cquery,
                &mut |batch| transform(batch, &[]),
                roots.as_deref_mut(),
                compatible.as_ref().filter(|_| rooted),
                verify,
                verify && rooted,
            )?
        } else if use_cquery {
            decode_cquery_stream(
                output_file.path(),
                streamed_cquery,
                &mut |batch| transform(batch, &[]),
                roots.as_deref_mut(),
                compatible.as_ref().filter(|_| rooted),
            )?
            .into()
        } else {
            decode_target_stream_filtered(output_file.path(), lean, transform, retain)?.into()
        };
        if let Some(compatible) = compatible.filter(|_| !rooted) {
            graph.targets.retain(|target| {
                target_name(target).is_some_and(|name| {
                    compatible.contains(main_repo_label_key(target_label(name)))
                })
            });
            if let Some(roots) = roots {
                roots.retain(|(label, _)| compatible.contains(main_repo_label_key(label)));
            }
        }
        Ok(graph)
    }

    fn query_compatible_labels(
        &self,
        expression: &str,
        version: BazelVersion,
        available_readers: bool,
    ) -> Result<HashSet<String>> {
        let output = self
            .query_formatted(
                expression,
                r#"def format(target):
    # Native targets such as package groups can have no provider dictionary.
    if "IncompatiblePlatformProvider" not in (providers(target) or {}):
        target_repr = repr(target)
        if "<alias target" in target_repr:
            # repr() prints a main-repository alias with its apparent label
            # (`//:tool`) even under --consistent_labels, while every other
            # target comes back as `@@//:tool` under Bzlmod and `@//:tool` in
            # WORKSPACE mode. The labels are compared through
            # main_repo_label_key, which puts all three in one form.
            return target_repr.split(" ")[2]
        return str(target.label)
    return ""
"#,
                version,
            )
            .context("execute compatible-target cquery")?;
        if !(output.status.success()
            || (available_readers && matches!(output.status.code(), Some(1 | 3 | 7))))
        {
            return Err(QueryFailure {
                code: output.status.code().unwrap_or(-1),
                message: format!(
                    "compatible-target cquery failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            }
            .into());
        }
        Ok(std::str::from_utf8(&output.stdout)
            .context("decode compatible target labels")?
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| main_repo_label_key(line).to_owned())
            .collect())
    }

    fn query_formatted(
        &self,
        expression: &str,
        formatter: &str,
        version: BazelVersion,
    ) -> Result<Output> {
        let query_file = NamedTempFile::new().context("create query file")?;
        fs::write(query_file.path(), expression)?;
        let starlark_file = NamedTempFile::new().context("create cquery formatter")?;
        fs::write(starlark_file.path(), formatter)?;
        let output_file = (version >= BazelVersion(8, 2, 0))
            .then(NamedTempFile::new)
            .transpose()
            .context("create cquery output file")?;
        let mut command = self.command();
        command
            .arg("cquery")
            .args(&self.cquery_options)
            .arg("--output")
            .arg("starlark")
            .arg("--starlark:file")
            .arg(starlark_file.path())
            .arg("--consistent_labels")
            .arg("--query_file")
            .arg(query_file.path())
            .stdin(Stdio::null());
        if let Some(file) = &output_file {
            command.arg("--output_file").arg(file.path());
        }
        let mut output = self.run(&mut command)?;
        if let Some(file) = output_file {
            output.stdout = fs::read(file.path())?;
        }
        Ok(output)
    }

    fn canonical_bzlmod_repo_names(&self) -> Result<Vec<String>> {
        let mapping_output = self.run_capture(&["mod", "dump_repo_mapping", ""])?;
        let mapping = self.accept_mod_output(
            mapping_output,
            "dump_repo_mapping",
            &mut FailedExtensions::default(),
        )?;
        Ok(parse_repo_mapping(&String::from_utf8_lossy(&mapping))
            .keys()
            .filter(|name| name.contains('+') || name.contains('~'))
            .map(|name| format!("@@{name}"))
            .collect::<Vec<_>>())
    }

    fn show_repo_streamed_proto(&self, canonical_names: &[String]) -> Result<Vec<u8>> {
        if canonical_names.is_empty() {
            return Ok(Vec::new());
        }
        let output = self.run_mod_over_repos(
            "show_repo",
            canonical_names,
            &mut FailedExtensions::default(),
            |names| {
                let mut command = self.command();
                command
                    .arg("mod")
                    .arg("show_repo")
                    .args(names)
                    .arg("--output=streamed_proto")
                    .stdin(Stdio::null())
                    .stderr(Stdio::piped());
                eprintln!(
                    "[BD-DBG][fingerprint-command] path=streamed_proto_selected cmd={command:?}"
                );
                if self.verbose {
                    eprintln!("[Info] Command: {command:?}");
                }
                let output = self
                    .run(&mut command)
                    .context("execute bazel mod show_repo streamed_proto")?;
                Ok((output.status, output.stderr, output.stdout))
            },
        )?;
        Ok(output.unwrap_or_default())
    }

    fn show_repo_fingerprint_text_fallback(&self, canonical_names: &[String]) -> Result<String> {
        let mut all_visible_command = self.command();
        all_visible_command
            .arg("mod")
            .arg("show_repo")
            .arg("--all_visible_repos")
            .arg("--output=text");
        eprintln!(
            "[BD-DBG][fingerprint-command] path=text_all_visible cmd={all_visible_command:?}"
        );
        all_visible_command
            .stdin(Stdio::null())
            .stderr(Stdio::piped());
        let output = self.run(&mut all_visible_command);
        if let Ok(output) = output {
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
        }

        let mut command = self.command();
        command.arg("mod").arg("show_repo");
        if canonical_names.is_empty() {
            command.arg("--output=text");
        } else {
            command.args(canonical_names).arg("--output=text");
        }
        eprintln!("[BD-DBG][fingerprint-command] path=text_selected_repos cmd={command:?}");
        command.stdin(Stdio::null()).stderr(Stdio::piped());
        let output = self
            .run(&mut command)
            .context("execute bazel mod show_repo")?;
        if !output.stderr.is_empty() && self.verbose {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        if !output.status.success() {
            bail!("bazel mod show_repo failed with {}", output.status);
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn parse_repo_mapping(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut canonical_to_apparent = BTreeMap::<String, Vec<String>>::new();
    for line in text.lines() {
        let value: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(mapping) = value.as_object() else {
            continue;
        };
        for (apparent, canonical) in mapping {
            let Some(canonical) = canonical.as_str() else {
                continue;
            };
            if apparent.is_empty()
                || canonical.is_empty()
                || canonical.starts_with("bazel_tools")
                || canonical.starts_with("_builtins")
                || canonical.starts_with("local_config_")
                || canonical.starts_with("rules_java_builtin")
            {
                continue;
            }
            canonical_to_apparent
                .entry(canonical.to_owned())
                .or_default()
                .push(apparent.clone());
        }
    }
    canonical_to_apparent
}

#[derive(Default)]
struct ModuleSurvey {
    graph: Option<Output>,
    mappings: BTreeMap<String, RepoMapping>,
}

/// Canonical names of the repositories module extensions generate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ExtensionRepositories {
    /// The ones some module (the root included) imports with `use_repo`: the hubs, toolchain
    /// registries and rule repos that targets name.
    imported: BTreeSet<String>,
    /// Every repository of every extension in `imported`, the imported ones included.
    generated: BTreeSet<String>,
    /// All modules and extension representatives supplied their requested mapping.
    complete_mappings: bool,
    /// Complete module mappings from the ordinary module read, keyed by their requested canonical names.
    module_mappings: BTreeMap<String, RepoMapping>,
}

#[derive(Default)]
struct IsolatedRepoScope {
    anchor: String,
    members: BTreeSet<String>,
    aliases: RepoMapping,
    // Typed labels addressed by the usage's repository rules, including non-source targets.
    repository_inputs: BTreeSet<String>,
}

fn scalar_label_values(actual: &Attribute) -> impl Iterator<Item = &String> {
    actual
        .string_value
        .iter()
        .chain(
            actual
                .selector_list
                .as_option()
                .into_iter()
                .flat_map(|list| &list.elements)
                .flat_map(|selector| &selector.entries)
                .filter_map(|entry| entry.string_value.as_ref()),
        )
        .filter(|label| !label.is_empty())
}

fn query_labels<'a>(labels: impl Iterator<Item = &'a String>) -> Result<String> {
    labels
        .map(|label| {
            if !label.contains('\'') {
                Ok(format!("'{label}'"))
            } else if !label.contains('"') {
                Ok(format!("\"{label}\""))
            } else {
                bail!("cannot quote Bazel query label {label}")
            }
        })
        .collect::<Result<Vec<_>>>()
        .map(|labels| labels.join(" + "))
}

fn canonical_query_label<'a>(label: &'a str, mapping: &HashMap<String, String>) -> Cow<'a, str> {
    let label = main_repo_label_key(label);
    if let Some((repo, target)) = label.strip_prefix('@').and_then(|s| s.split_once("//")) {
        if let Some(canonical) = mapping.get(repo) {
            return Cow::Owned(format!("@@{canonical}//{target}"));
        }
    }
    Cow::Borrowed(label)
}

fn external_query_package(label: &str) -> Option<&str> {
    let (package, _) = label.rsplit_once(':')?;
    (package.starts_with("@@") && !package.starts_with("@@//")).then_some(package)
}

fn target_inputs(target: &Target) -> impl Iterator<Item = &String> {
    target
        .rule
        .as_option()
        .into_iter()
        .flat_map(|r| r.rule_input.iter())
        .chain(
            target
                .generated_file
                .as_option()
                .map(|f| &f.generating_rule),
        )
}

fn query_set(labels: &BTreeSet<String>) -> Result<String> {
    let operands = labels
        .iter()
        .map(|label| query_labels(std::iter::once(label)))
        .collect::<Result<Vec<_>>>()?;
    Ok(format!("set({})", operands.join(" ")))
}

/// Coverage of the retained primary targets followed by any accepted supplement.
#[derive(Default)]
struct ScopedCoverage {
    names: HashSet<String>,
    files: HashSet<String>,
    missing: BTreeSet<String>,
    direct_readers: Vec<bool>,
}

impl ScopedCoverage {
    fn record(&mut self, scope: &IsolatedRepoScope, targets: &[Target]) {
        for target in targets {
            if let Some(name) = target_name(target).and_then(|name| scope.canonical(name)) {
                self.missing.remove(&name);
                self.names.insert(name);
            }
            if let Some(name) = target
                .source_file
                .as_option()
                .and_then(|file| scope.canonical(&file.name))
            {
                self.files.insert(name);
            }
        }
        for target in targets {
            let mut direct = false;
            if let Some(rule) = target.rule.as_option() {
                for input in rule
                    .rule_input
                    .iter()
                    .filter_map(|label| scope.canonical(label))
                {
                    direct = true;
                    if !self.names.contains(&input) {
                        self.missing.insert(input);
                    }
                }
            }
            self.direct_readers.push(direct);
        }
    }
}

// The primary wins on equal names. Preserve repeated records in the supplement
// when no primary record has their name.
fn retain_supplemental_targets(extra: &mut Vec<Target>, primary: &[Target]) {
    if extra.is_empty() {
        return;
    }
    let primary_names = primary.iter().filter_map(target_name);
    let present: HashSet<_> = if extra.len() <= primary.len() / 2 {
        let names: HashSet<_> = extra.iter().filter_map(target_name).collect();
        primary_names.filter(|name| names.contains(name)).collect()
    } else {
        primary_names.collect()
    };
    extra.retain(|target| target_name(target).is_some_and(|name| !present.contains(name)));
}

impl IsolatedRepoScope {
    fn expression(&self, ordinary: &str) -> String {
        format!("let members = deps('@{}//...:all-targets') in ({ordinary}) + $members + buildfiles($members)", self.anchor)
    }

    fn canonical(&self, label: &str) -> Option<String> {
        let (repo, rest) = label.strip_prefix('@')?.split_once("//")?;
        let canonical = if let Some(name) = repo.strip_prefix('@') {
            self.members.get(name)?
        } else {
            self.members.get(self.aliases.get(repo)?)?
        };
        Some(format!("@@{canonical}//{rest}"))
    }

    fn package<'a>(&self, label: &'a str) -> Cow<'a, str> {
        let package = label.rsplit_once(':').map_or(label, |(package, _)| package);
        if let Some((repo, path)) = package
            .strip_prefix('@')
            .and_then(|label| label.split_once("//"))
        {
            if repo.starts_with('@') && repo.len() > 1 {
                return Cow::Borrowed(package);
            }
            let canonical = repo
                .strip_prefix('@')
                .or_else(|| self.aliases.get(repo).map(String::as_str));
            if let Some(repo) = canonical {
                return if repo.is_empty() {
                    Cow::Owned(format!("//{path}"))
                } else {
                    Cow::Owned(format!("@@{repo}//{path}"))
                };
            }
        }
        Cow::Borrowed(package)
    }

    // Ordinary query can emit two source records for a BUILD, even within one stream.
    // Keep the union of observed load edges before normal first-record deduplication.
    fn record_build_loads(
        &self,
        batch: &[Target],
        loads_by_package: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        for source in batch
            .iter()
            .filter_map(|target| target.source_file.as_option())
        {
            let Some((_, "BUILD" | "BUILD.bazel")) = source.name.rsplit_once(':') else {
                continue;
            };
            if self.canonical(&source.name).is_some() {
                loads_by_package
                    .entry(source.name.clone())
                    .or_default()
                    .extend(source.subinclude.iter().cloned());
            } else {
                for load in source
                    .subinclude
                    .iter()
                    .filter_map(|load| self.canonical(load))
                {
                    loads_by_package
                        .entry(source.name.clone())
                        .or_default()
                        .insert(load);
                }
            }
        }
    }

    fn connect_external_readers(
        &self,
        loads_by_package: &BTreeMap<String, BTreeSet<String>>,
        targets: &mut [Target],
        coverage: &ScopedCoverage,
    ) -> Result<()> {
        debug_assert_eq!(targets.len(), coverage.direct_readers.len());
        let mut readers = BTreeSet::new();
        for (build, loads) in loads_by_package {
            for load in loads.iter().filter_map(|label| self.canonical(label)) {
                if !coverage.files.contains(&load) {
                    bail!("--isolatedDependencyRoot: buildfiles did not return scoped load {load}");
                }
                readers.insert(self.package(build));
            }
        }
        let root = format!("//external:{}", self.anchor);
        for (index, target) in targets.iter_mut().enumerate() {
            if let Some(source) = target.source_file.as_option_mut() {
                if self.canonical(&source.name).is_some() {
                    if let Some(loads) = loads_by_package.get(&source.name) {
                        source.subinclude = loads.iter().cloned().collect();
                    }
                }
            }
            if let Some(rule) = target.rule.as_option_mut() {
                if !rule.name.starts_with("//external:")
                    && self.canonical(&rule.name).is_none()
                    && (coverage.direct_readers[index]
                        || (!readers.is_empty()
                            && readers.contains(self.package(&rule.name).as_ref())))
                    && !rule.rule_input.contains(&root)
                {
                    rule.rule_input.push(root.clone());
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct LabelOnlyRepositories {
    /// Usage prefixes of `label_only_module_extensions`; their owners get no stand-in edges.
    extensions: BTreeSet<String>,
    owner_exemptions: BTreeSet<String>,
}

/// Bind the effective root import to one returned extension usage. Its one imported
/// hub already depends on every hidden member; use that same aggregate at its readers.
fn resolve_isolated_root(
    root: &str,
    root_mapping: &[u8],
    survey: &ExtensionRepositories,
    repositories: &[Repository],
) -> Result<IsolatedRepoScope> {
    let name = root
        .strip_prefix('@')
        .context("expected an apparent repository name beginning with @")?;
    if name.is_empty() || name.contains(['@', '/', '%', '+', ':']) {
        bail!("expected one apparent repository name without a path or canonical prefix");
    }
    let mapping: RepoMapping =
        serde_json::from_slice(root_mapping).context("invalid root repository mapping")?;
    let hub = mapping
        .get(name)
        .context("root import is missing from the repository mapping")?;
    let group = effective_root_group(hub, survey, repositories)?;
    if let Some(other) = survey
        .imported
        .iter()
        .find(|other| *other != hub && extension_usage_prefix(other) == Some(group))
    {
        bail!("extra imported repository {other} belongs to the isolated usage");
    }
    let members: BTreeSet<_> = survey
        .generated
        .iter()
        .filter(|repo| extension_usage_prefix(repo) == Some(group))
        .cloned()
        .collect();
    let mut inputs = BTreeSet::new();
    for repo in repositories {
        let display_name = repo
            .canonical_name
            .as_deref()
            .or(repo.original_name.as_deref())
            .unwrap_or("<unnamed>");
        for attr in &repo.attribute {
            use crate::proto::blaze_query::attribute::Discriminator as Kind;
            if !attr.__buffa_unknown_fields.is_empty() {
                bail!(
                    "repository {display_name} attribute {} has unknown protobuf fields",
                    attr.name
                );
            }
            if matches!(
                attr.r#type,
                Kind::Unknown
                    | Kind::SelectorList
                    | Kind::FilesetEntryList
                    | Kind::License
                    | Kind::Output
                    | Kind::OutputList
            ) {
                bail!(
                    "repository {display_name} attribute {} has unsupported type {:?}",
                    attr.name,
                    attr.r#type
                );
            }
            if !matches!(
                attr.r#type,
                Kind::Label
                    | Kind::LabelList
                    | Kind::LabelListDict
                    | Kind::LabelDictUnary
                    | Kind::LabelKeyedStringDict
            ) {
                continue;
            }
            for value in attribute_label_values(attr) {
                let reader = repo.canonical_name.as_deref().with_context(|| {
                    format!(
                        "repository {display_name} attribute {} has no canonical repository name",
                        attr.name
                    )
                })?;
                let canonical = if value.starts_with("@@") {
                    value.to_owned()
                } else if value.starts_with("//") {
                    format!("@@{value}")
                } else if value.starts_with('@') {
                    bail!(
                        "repository {reader} attribute {} has unresolved label {value}",
                        attr.name
                    );
                } else {
                    continue;
                };
                let (target, _) = canonical[2..].split_once("//").with_context(|| {
                    format!(
                        "repository {reader} attribute {} has invalid label {canonical}",
                        attr.name
                    )
                })?;
                if members.contains(reader) {
                    inputs.insert(canonical);
                } else if members.contains(target) {
                    // A repository generator outside the usage may consume a file
                    // without producing any ordinary target edge.
                    bail!(
                        "outside repository {reader} attribute {} reads {canonical}",
                        attr.name
                    );
                }
            }
        }
    }
    Ok(IsolatedRepoScope {
        anchor: name.to_owned(),
        aliases: mapping,
        members,
        repository_inputs: inputs,
    })
}

/// Bazel assigns each root-isolated usage its own canonical prefix. Its module and
/// version slots are both empty, followed by the exported usage name. Repository names
/// themselves cannot contain '+'. Verify that every returned original name agrees with
/// that prefix, and that no other root import identifies a different group of that name.
fn effective_root_group<'a>(
    hub: &'a str,
    survey: &ExtensionRepositories,
    repositories: &[Repository],
) -> Result<&'a str> {
    fn root_export(prefix: &str) -> Option<&str> {
        let (owner_and_symbol, name) = prefix.rsplit_once("+++")?;
        let (_owner, symbol) = owner_and_symbol.rsplit_once("+_")?;
        (!symbol.is_empty() && !symbol.contains('+') && !name.is_empty() && !name.contains('+'))
            .then_some(name)
    }
    let group = extension_usage_prefix(hub)
        .with_context(|| format!("mapped repository {hub} is not an extension repository"))?;
    let usage = root_export(group).with_context(|| {
        format!("mapped repository {hub} is not a root-isolated extension usage")
    })?;
    if !survey.imported.contains(hub) {
        bail!("mapped repository {hub} is not imported");
    }
    for imported in &survey.imported {
        let Some(candidate) = extension_usage_prefix(imported) else {
            continue;
        };
        if candidate != group && root_export(candidate) == Some(usage) {
            bail!("root-isolated usage name {usage} conflicts with imported repository {imported}");
        }
    }
    let mut seen = BTreeSet::new();
    for row in repositories {
        let Some(name) = row.canonical_name.as_deref() else {
            continue;
        };
        let Some((prefix, original)) = name.rsplit_once('+') else {
            continue;
        };
        if prefix != group {
            continue;
        }
        if original.is_empty() {
            bail!("repository {name} has an empty original name");
        }
        if row.original_name.as_deref() != Some(original) {
            bail!(
                "repository {name} reported original name {:?}, expected {original}",
                row.original_name
            );
        }
        if !seen.insert(name) {
            bail!("repository {name} appears more than once in the catalog");
        }
    }
    if !seen.contains(hub) {
        bail!("mapped root repository {hub} has no catalog record");
    }
    for name in &survey.generated {
        if extension_usage_prefix(name) == Some(group) && !seen.contains(name.as_str()) {
            bail!("generated repository {name} has no catalog record");
        }
    }
    for name in seen {
        if !survey.generated.contains(name) {
            bail!("catalog repository {name} is absent from the generated inventory");
        }
    }
    Ok(group)
}

type RepoMapping = BTreeMap<String, String>;

struct BulkRepositories {
    by_canonical: HashMap<String, Repository>,
    complete: bool,
}

/// Resolve each hidden module through its apparent name in its parent's mapping. The
/// names predicted by `hidden_module_repo_names` alone cannot identify a bulk record.
fn verified_hidden_modules(
    graph: Option<&str>,
    root_mapping: &[u8],
    mappings: &BTreeMap<String, RepoMapping>,
    hidden: &BTreeMap<String, String>,
) -> bool {
    fn walk(
        node: &serde_json::Value,
        parent: &RepoMapping,
        mappings: &BTreeMap<String, RepoMapping>,
        hidden: &BTreeMap<String, String>,
        seen: &mut BTreeSet<String>,
    ) -> Option<()> {
        for child in node.get("dependencies")?.as_array()? {
            let key = child.get("key")?.as_str()?;
            let canonical = parent.get(child.get("apparentName")?.as_str()?)?;
            if let Some(expected) = hidden.get(key) {
                if expected != canonical {
                    return None;
                }
                seen.insert(key.to_owned());
            }
            if child
                .get("dependencies")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|dependencies| !dependencies.is_empty())
            {
                walk(child, mappings.get(canonical)?, mappings, hidden, seen)?;
            }
        }
        Some(())
    }
    if hidden.is_empty() || mappings.is_empty() {
        return false;
    }
    let Some(graph) = graph.and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
    else {
        return false;
    };
    let Ok(root) = serde_json::from_slice::<RepoMapping>(root_mapping) else {
        return false;
    };
    let mut seen = BTreeSet::new();
    walk(&graph, &root, mappings, hidden, &mut seen).is_some() && seen.len() == hidden.len()
}

/// Whether `canonical` names a repository a module extension generated rather than a module's
/// own repository: `rules_go++go_sdk+go_default_sdk`, `platforms+host_platform+host_platform`
/// or the root's `+ext+repo`, against `rules_go+`, `dep+1.0` and `platforms`.
fn is_extension_repo(canonical: &str) -> bool {
    canonical.matches('+').count() >= 2
}

/// The canonical prefix shared by repositories generated by one extension usage.
pub(crate) fn extension_usage_prefix(canonical: &str) -> Option<&str> {
    if !is_extension_repo(canonical) {
        return None;
    }
    canonical.rsplit_once('+').map(|(prefix, _repo)| prefix)
}

/// Finds the emitted defining module and its definition hash from an extension usage prefix.
/// Ordinary module canonicals include the module/version `+` separator, and those components
/// contain no `+`. Special bare names never have versioned forms, so emitted names cannot
/// nest at separator boundaries.
fn extension_repo_owner<'a>(
    usage_prefix: &str,
    module_definitions: &'a BTreeMap<String, String>,
) -> Option<(&'a String, &'a String)> {
    usage_prefix
        .match_indices('+')
        .find_map(|(separator, _matched)| {
            module_definitions.get_key_value(&usage_prefix[..separator])
        })
}

/// Upper bound on the bytes of repository names one `bazel mod` invocation is given, unless
/// [`MOD_ARGUMENT_BYTE_BUDGET_ENV`] says otherwise. Windows caps a command line at 32K
/// characters, and `cmd.exe` at [`CMD_EXE_COMMAND_LINE_LIMIT`], which is the one that binds
/// whenever a batch file is on the path to Bazel: a `--bazelPath` naming `bazel.bat`, or
/// Bazelisk running the workspace's `tools/bazel.bat` wrapper, which re-expands the whole
/// line through `"%BAZEL_REAL%" %*` (#536). 6K leaves the Bazel path, the startup options
/// and the subcommand their share of it. Elsewhere the limit is megabytes, but one
/// invocation per few thousand repositories keeps a workspace with tens of thousands of
/// extension repositories (an LLVM or pip hub per platform) away from it too.
const DEFAULT_MOD_ARGUMENT_BYTE_BUDGET: usize = if cfg!(windows) { 6 * 1024 } else { 256 * 1024 };

/// The longest command line `cmd.exe` runs.
const CMD_EXE_COMMAND_LINE_LIMIT: usize = 8191;

// The Windows default must leave `cmd.exe` room for everything but the names: at least a
// Bazel path, startup options and the `mod` subcommand's own arguments (#536).
const _: () = assert!(
    !cfg!(windows) || DEFAULT_MOD_ARGUMENT_BYTE_BUDGET + 2047 <= CMD_EXE_COMMAND_LINE_LIMIT
);

/// Environment variable overriding [`DEFAULT_MOD_ARGUMENT_BYTE_BUDGET`] with a byte count,
/// for a Bazel wrapper that leaves the names less room (long startup options) or more (no
/// batch file on Windows).
pub const MOD_ARGUMENT_BYTE_BUDGET_ENV: &str = "BAZEL_DIFF_MOD_ARGUMENT_BYTE_BUDGET";

fn mod_argument_byte_budget() -> usize {
    mod_argument_byte_budget_from(std::env::var(MOD_ARGUMENT_BYTE_BUDGET_ENV).ok().as_deref())
}

/// The byte budget `setting` (the environment variable's value) asks for, or the default when
/// it is unset or not a positive byte count.
fn mod_argument_byte_budget_from(setting: Option<&str>) -> usize {
    let Some(setting) = setting else {
        return DEFAULT_MOD_ARGUMENT_BYTE_BUDGET;
    };
    match setting.trim().parse::<usize>() {
        Ok(budget) if budget > 0 => budget,
        _ => {
            eprintln!(
                "[Warn] ignoring {MOD_ARGUMENT_BYTE_BUDGET_ENV}={setting:?}: not a positive byte count"
            );
            DEFAULT_MOD_ARGUMENT_BYTE_BUDGET
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct ScopedSources {
    pub(crate) repositories: BTreeSet<String>,
    pub(crate) repository_inputs: BTreeSet<String>,
    pub(crate) queried_files: HashSet<String>,
    pub(crate) alias_sources: HashMap<String, AliasSource>,
    /// Full native declaration returned separately from the caller's projected hash row.
    pub(crate) filegroups: BTreeMap<String, FilegroupWitness>,
    /// Declared inputs returned by both the strict filegroup inventory and the ordinary projection.
    pub(crate) filegroup_sources: BTreeMap<String, AliasSource>,
    /// Full declarations of native rules with direct inputs in the observed filegroups.
    pub(crate) filegroup_parents: BTreeMap<String, FilegroupWitness>,
}

/// The full structural declaration and, where Bazel returned its synthetic attribute hash,
/// the definition digest of the corresponding caller-projected row.
#[derive(Clone)]
pub(crate) struct FilegroupWitness {
    pub(crate) declaration: Rule,
    pub(crate) projected_definition: Option<[u8; 32]>,
}

impl FilegroupWitness {
    pub(crate) fn new(declaration: Rule) -> Self {
        Self {
            declaration,
            projected_definition: None,
        }
    }
}

/// Evaluated metadata of a literal native alias destination returned in the strict closure.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AliasSource {
    pub(crate) label: String,
    pub(crate) location: String,
    pub(crate) visibility: Vec<String>,
}

impl AliasSource {
    fn observe(
        source: &crate::proto::blaze_query::SourceFile,
        sources: &mut BTreeMap<String, Option<Self>>,
    ) {
        // Visibility labels are evaluated for a real InputFile. A synthetic
        // buildfiles() result carries no such declaration.
        let witness = source.location.as_ref().and_then(|location| {
            if source.visibility_label.is_empty()
                || !source.package_group.is_empty()
                || source.license.as_option().is_some()
            {
                return None;
            }
            let mut visibility = source.visibility_label.clone();
            visibility.sort_unstable();
            visibility.dedup();
            Some(AliasSource {
                label: source.name.clone(),
                location: location.clone(),
                visibility,
            })
        });
        Self::record(sources, source.name.clone(), witness);
    }

    fn record(sources: &mut BTreeMap<String, Option<Self>>, label: String, witness: Option<Self>) {
        match sources.entry(label) {
            Entry::Vacant(entry) => {
                entry.insert(witness);
            }
            Entry::Occupied(mut entry) => {
                if entry.get() != &witness {
                    entry.insert(None);
                }
            }
        }
    }
}

/// Targets with the accepted module graph and successful unfiltered root mapping from discovery.
pub(crate) struct QueriedTargets {
    pub(crate) targets: Vec<Target>,
    pub(crate) verification: Option<VerifiedQuery>,
    pub(crate) configured_roots: Option<BTreeMap<String, String>>,
    pub(crate) module_graph_json: Option<String>,
    pub(crate) module_graph_complete: bool,
    pub(crate) root_mapping: Option<Vec<u8>>,
    pub(crate) scoped_sources: ScopedSources,
    /// The output base `bazel info` reported before the query, when it was asked.
    pub(crate) output_base: Option<PathBuf>,
}

pub(crate) struct VerifiedQuery {
    pub(crate) targets: Vec<Target>,
    pub(crate) aspects: Vec<ObservedAspect>,
    /// Locations from genuine SourceFile rows. A missing or conflicting location is unresolved.
    pub(crate) source_locations: BTreeMap<String, Option<String>>,
}

pub(crate) struct ObservedAspect {
    pub(crate) key: String,
    pub(crate) inputs: Vec<String>,
}

struct QueryGraph {
    targets: Vec<Target>,
    aspects: Option<Vec<ObservedAspect>>,
    source_locations: BTreeMap<String, Option<String>>,
}

impl From<Vec<Target>> for QueryGraph {
    fn from(targets: Vec<Target>) -> Self {
        Self {
            targets,
            aspects: None,
            source_locations: BTreeMap::new(),
        }
    }
}

impl QueryGraph {
    fn into_verified(self) -> Option<VerifiedQuery> {
        self.aspects.map(|aspects| VerifiedQuery {
            targets: self.targets,
            aspects,
            source_locations: self.source_locations,
        })
    }
}

enum CqueryCapture<'a> {
    Standard,
    Roots(&'a mut Vec<(String, String)>),
    Verification,
}

/// The repositories `bazel mod` describes for a Bzlmod workspace, lowered to `//external:`
/// nodes, with what it could not describe.
#[derive(Default)]
struct BzlmodRepos {
    definitions: Option<RepositoryDefinitions>,
    scope: IsolatedRepoScope,
    /// Extension diagnostics and canonical repositories whose mappings failed.
    failed_extensions: FailedExtensions,
    /// The root module's repository mapping, canonical name -> apparent names.
    canonical_to_apparent: BTreeMap<String, Vec<String>>,
    /// The accepted module graph, shared by repository edges and hash-file metadata.
    module_graph_json: Option<String>,
    /// Only a successful command can support verification; tolerated partial answers cannot.
    module_graph_complete: bool,
    /// Unfiltered stdout from a successful initial Bzlmod probe, shared with external hashing.
    root_mapping: Option<Vec<u8>>,
}

/// Keep module definitions separate from ordinary rule inputs until their missing edges are known.
struct RepositoryDefinitions {
    repositories: Vec<Repository>,
    module_edges: BTreeMap<String, BTreeSet<String>>,
    extensions: ExtensionRepositories,
    label_only: LabelOnlyRepositories,
}

impl RepositoryDefinitions {
    fn lower<T>(
        &mut self,
        bazel: &BazelOptions,
        canonical_to_apparent: &BTreeMap<String, Vec<String>>,
        process: impl FnMut(RepositoryRow) -> T,
    ) -> Vec<T> {
        repository_rows(
            std::mem::take(&mut self.repositories),
            canonical_to_apparent,
            ModuleDependencies {
                edges: &self.module_edges,
                mappings: &self.extensions.module_mappings,
            },
            (!bazel.use_cquery).then_some(&self.extensions.imported),
            &self.label_only,
            &bazel.workspace,
            process,
        )
    }
}

/// Extension diagnostics and the canonical repositories whose mappings actually failed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FailedExtensions {
    /// Bazel reported an extension failure, even if its diagnostic had no parseable ID.
    observed: bool,
    /// Extension ids printed by Bazel.
    extensions: BTreeSet<String>,
    /// Canonical repository names whose mapping keys failed.
    unavailable: BTreeSet<String>,
}

impl FailedExtensions {
    fn is_empty(&self) -> bool {
        !self.observed && self.extensions.is_empty() && self.unavailable.is_empty()
    }
}

/// What a failed `bazel mod` invocation said about module extensions.
struct ExtensionFailure {
    /// The extensions Bazel could not evaluate, by the id it prints.
    extensions: BTreeSet<String>,
    /// Whether Bazel still answered for the repositories that do not need them. It does when
    /// the extensions merely sit in the dependency graph, ending with `Results may be
    /// incomplete as N extensions failed.`; not when one of their repositories was asked for.
    answered: bool,
}

/// The line `bazel mod` ends with when it answered despite a module extension failing.
const EXTENSION_FAILURE_MARKER: &str = "Results may be incomplete as";

/// Reads a failed `bazel mod` invocation's stderr for module extensions it could not
/// evaluate. It retains ids printed as `@@<repo>//<package>:<file>.bzl%<name>` and also
/// recognizes the partial-answer marker when no id is present. `None` when neither is found.
fn extension_failure(stderr: &str) -> Option<ExtensionFailure> {
    let answered = stderr.contains(EXTENSION_FAILURE_MARKER);
    let extensions = stderr
        .lines()
        .filter(|line| line.contains("module extension"))
        .flat_map(str::split_whitespace)
        .filter(|word| word.starts_with("@@") && word.contains("//") && word.contains('%'))
        .map(|word| word.trim_end_matches(['.', ',', ';', ':']).to_owned())
        .collect::<BTreeSet<_>>();
    (answered || !extensions.is_empty()).then_some(ExtensionFailure {
        extensions,
        answered,
    })
}

fn has_unavailable_mapping(canonical: &str, failed: &FailedExtensions) -> bool {
    failed
        .unavailable
        .contains(canonical.trim_start_matches('@'))
}

/// Splits `arguments` into consecutive runs of at most `budget` bytes (separators included),
/// every run non-empty.
fn chunk_arguments(arguments: &[String], budget: usize) -> Vec<&[String]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (index, argument) in arguments.iter().enumerate() {
        let cost = argument.len() + 1;
        if index > start && bytes + cost > budget {
            chunks.push(&arguments[start..index]);
            start = index;
            bytes = 0;
        }
        bytes += cost;
    }
    if start < arguments.len() {
        chunks.push(&arguments[start..]);
    }
    chunks
}

/// A generated repository can acquire its first unimported sibling later. Keep its
/// definition available to qualified readers on both sides of that change.
#[derive(Clone)]
pub(crate) enum RepositoryInputs {
    DefinitionOnly,
    WithSiblings(Arc<RepositoryInputGroup>),
}

impl RepositoryInputs {
    pub(crate) fn siblings(&self) -> Option<&RepositoryInputGroup> {
        match self {
            Self::DefinitionOnly => None,
            Self::WithSiblings(group) => Some(group),
        }
    }
}

pub(crate) struct RepositoryInputGroup {
    pub(crate) extension: String,
    pub(crate) labels: Vec<String>,
}

fn materialize_repository_inputs_for_batch(batch: &mut [Target], inputs: Option<RepositoryInputs>) {
    if let Some(inputs) = inputs {
        for target in batch {
            materialize_repository_inputs(target, Some(inputs.clone()), None);
        }
    }
}

fn materialize_repository_inputs(
    target: &mut Target,
    shared: Option<RepositoryInputs>,
    _ordinary_inputs: Option<Vec<String>>,
) {
    if let Some(inputs) = shared.as_ref().and_then(RepositoryInputs::siblings) {
        target.rule.as_option_mut().unwrap().rule_input = inputs.labels.clone();
    }
}

struct ModuleDependencies<'a> {
    edges: &'a BTreeMap<String, BTreeSet<String>>,
    mappings: &'a BTreeMap<String, RepoMapping>,
}

/// The base rule contains only real module inputs. Extension memberships
/// stay grouped until observation determines which stand-ins are still needed.
struct RepositoryRow {
    target: Target,
    memberships: Vec<Arc<RepositoryInputGroup>>,
    generated: bool,
    ordinary_complete: bool,
}

impl RepositoryRow {
    fn inferred<'a>(&'a self, group: &'a RepositoryInputGroup) -> impl Iterator<Item = &'a str> {
        let rule = self.target.rule.as_option().expect("repository");
        group.labels.iter().filter_map(|child| {
            (child != &rule.name && rule.rule_input.binary_search(child).is_err())
                .then_some(child.as_str())
        })
    }

    fn finish(
        mut self,
        observed: &BTreeSet<String>,
        transform: &mut impl FnMut(&mut Target, Option<RepositoryInputs>, Option<Vec<String>>),
    ) -> Target {
        let rule = self.target.rule.as_option_mut().expect("repository");
        let ordinary = self.ordinary_complete.then(|| rule.rule_input.clone());
        let mut inputs = self.generated.then_some(RepositoryInputs::DefinitionOnly);
        for group in self.memberships {
            if observed.contains(&group.extension) {
                continue;
            }
            if self.generated
                && rule.rule_input.is_empty()
                && group.labels.binary_search(&rule.name).is_err()
            {
                inputs = Some(RepositoryInputs::WithSiblings(group));
            } else {
                inputs = None;
                rule.rule_input.extend(
                    group
                        .labels
                        .iter()
                        .filter(|child| **child != rule.name)
                        .cloned(),
                );
            }
        }
        rule.rule_input.sort_unstable();
        rule.rule_input.dedup();
        transform(&mut self.target, inputs, ordinary);
        self.target
    }
}

fn repository_rows<T>(
    mut repositories: Vec<Repository>,
    canonical_to_apparent: &BTreeMap<String, Vec<String>>,
    modules: ModuleDependencies<'_>,
    extension_edges: Option<&BTreeSet<String>>,
    label_only: &LabelOnlyRepositories,
    workspace: &Path,
    mut process: impl FnMut(RepositoryRow) -> T,
) -> Vec<T> {
    let ModuleDependencies {
        edges: module_edges,
        mappings: module_mappings,
    } = modules;
    let workspace = RepositoryWorkspace::from_repositories(workspace, &repositories);
    // A module the root cannot see has no apparent name there; its node is named by canonical
    // name, matching the `apparent_names` fallback below.
    let node_names = |canonical: &str| {
        canonical_to_apparent
            .get(canonical)
            .cloned()
            .unwrap_or_else(|| vec![canonical.to_owned()])
    };
    let mut module_to_canonical = BTreeMap::<String, String>::new();
    let mut ambiguous_modules = BTreeSet::new();
    for repository in &repositories {
        let Some(canonical) = repository.canonical_name.as_deref() else {
            continue;
        };
        if canonical.matches('+').count() != 1 {
            continue;
        }
        let module = canonical.split('+').next().unwrap_or(canonical);
        match module_to_canonical.entry(module.to_owned()) {
            Entry::Vacant(entry) => {
                entry.insert(canonical.to_owned());
            }
            Entry::Occupied(mut entry) => {
                if entry.get() != canonical {
                    ambiguous_modules.insert(entry.key().clone());
                    entry.insert(canonical.to_owned());
                }
            }
        }
    }

    // An extension's repositories are built by the owning module's extension code, from tags
    // the owning module's definition shapes, so that definition -- attributes, override patches,
    // a local override's tree -- is folded into each of them as an attribute: a patched
    // `rules_rs` is a different `@rules_rust` and `@bd_crates`, which is how a target that never
    // names `@rules_rs//` sees the patch. An attribute rather than an edge, so the query-mode
    // edges from a module to its extension repositories below do not close a cycle.
    // The same record supplies the module definition and its synthetic rule. Use the record's
    // position, since separate records with the same canonical name may have different inputs.
    let mut module_files = HashMap::new();
    let mut module_definitions = BTreeMap::new();
    for (index, repository) in repositories.iter_mut().enumerate() {
        let Some(name) = &repository.canonical_name else {
            continue;
        };
        if !is_extension_repo(name) {
            let name = name.clone();
            let files = prepare_repository(repository, &workspace);
            module_definitions.insert(name, repository_definition_hash(repository, &files));
            module_files.insert(index, files);
        }
    }

    // Query mode cannot see how a target reaches an extension's repositories: a toolchain is
    // resolved rather than declared, and a hub's aliases live in a BUILD file the query never
    // loads. Two conservative stand-ins keep the change reachable: the module that owns an
    // extension depends on everything it generates (every rules_go target names `@rules_go//`),
    // and each imported repository of an extension depends on the siblings nobody imports (the
    // `@bd_crates` hub reaches its `bd_crates__*` spokes). Spokes themselves stay leaves, so no
    // cycle forms. Fine-grained hashing uses these groups to find affected callers.
    // A repository row adds only the memberships that still lack observed replacement inputs.
    // An extension the user lists in `label_only_module_extensions` gets no owner edges: the user
    // asserts that its repositories are reached only through labels, which the query reports,
    // never through a resolved toolchain. Otherwise every target of the owning module, of every
    // module with a `bazel_dep` path to it, and every fine-grained spoke would change whenever
    // any repository of the extension does (issue #603). It is opt-in because a toolchain
    // resolved from one of them is invisible to a query, however it is registered.
    // An explicitly label-only imported repository needs no owner fallback. Its own node,
    // the hub-to-spoke inputs and the owner's edges to other generators remain.
    let mut module_extension_repos = BTreeMap::<String, BTreeMap<String, BTreeSet<String>>>::new();
    let mut extension_spokes = BTreeMap::<String, BTreeSet<String>>::new();
    if let Some(imported) = extension_edges {
        for repository in &repositories {
            let canonical = repository.canonical_name.as_deref().unwrap_or_default();
            let Some(extension) = extension_usage_prefix(canonical) else {
                continue;
            };
            if let Some((module_repo, _definition_hash)) =
                extension_repo_owner(extension, &module_definitions).filter(
                    |(module_repo, _definition_hash)| {
                        !module_repo.is_empty()
                            && !label_only.owner_exemptions.contains(canonical)
                            && !label_only.extensions.contains(extension)
                    },
                )
            {
                module_extension_repos
                    .entry(module_repo.clone())
                    .or_default()
                    .entry(extension.to_owned())
                    .or_default()
                    .extend(node_names(canonical));
            }
            if !imported.contains(canonical) {
                extension_spokes
                    .entry(extension.to_owned())
                    .or_default()
                    .extend(node_names(canonical));
            }
        }
    }

    let mut targets = Vec::new();
    let group = |(extension, names): (String, BTreeSet<String>)| {
        Arc::new(RepositoryInputGroup {
            extension,
            labels: names
                .into_iter()
                .map(|name| format!("//external:{name}"))
                .collect(),
        })
    };
    let owned: BTreeMap<_, Vec<_>> = module_extension_repos
        .into_iter()
        .map(|(module, usages)| (module, usages.into_iter().map(group).collect()))
        .collect();
    let spokes: BTreeMap<_, _> = extension_spokes
        .into_iter()
        .map(group)
        .map(|group| (group.extension.clone(), group))
        .collect();
    for (index, mut repository) in repositories.into_iter().enumerate() {
        let canonical = repository.canonical_name.as_deref().unwrap_or_default();
        let extension = extension_usage_prefix(canonical);
        let generated = extension.is_some();
        let owner_definition_hash = extension
            .and_then(|extension| extension_repo_owner(extension, &module_definitions))
            .map(|(_module_repo, definition_hash)| definition_hash.as_str());
        let module = (canonical.matches('+').count() == 1)
            .then(|| canonical.split('+').next().unwrap_or(canonical));
        // A successful `mod graph` may still honor display filters. The owner's
        // completed mapping is independent of them and must account for every
        // regular repository dependency before this node can be projected.
        let ordinary_complete = module.is_some_and(|module| {
            !ambiguous_modules.contains(module)
                && module_edges
                    .get(module)
                    .into_iter()
                    .flatten()
                    .all(|dependency| {
                        !ambiguous_modules.contains(dependency.as_str())
                            && module_to_canonical.contains_key(dependency)
                    })
                && module_mappings.get(canonical).is_some_and(|mapping| {
                    mapping.values().all(|dependency| {
                        if dependency == canonical
                            || WELL_KNOWN_MODULES.contains(&dependency.as_str())
                            || is_extension_repo(dependency)
                        {
                            return true;
                        }
                        let Some(name) = dependency.split_once('+').map(|(name, _)| name) else {
                            return false;
                        };
                        !ambiguous_modules.contains(name)
                            && module_to_canonical.get(name) == Some(dependency)
                            && module_edges
                                .get(module)
                                .is_some_and(|dependencies| dependencies.contains(name))
                    })
                })
        });
        let ordinary_dependencies = module
            .into_iter()
            .flat_map(|module| module_edges.get(module).into_iter().flatten())
            .filter_map(|dependency| module_to_canonical.get(dependency))
            .flat_map(|canonical| node_names(canonical))
            .collect::<BTreeSet<_>>();
        let mut memberships = owned.get(canonical).cloned().unwrap_or_default();
        if let Some(group) = extension
            .filter(|_| extension_edges.is_some_and(|imported| imported.contains(canonical)))
            .and_then(|usage| spokes.get(usage))
        {
            memberships.push(Arc::clone(group));
        }
        let names = node_names(canonical);
        if names.is_empty() {
            continue;
        }
        let files = module_files
            .remove(&index)
            .unwrap_or_else(|| prepare_repository(&mut repository, &workspace));
        let rule = repository_rule(repository, files, owner_definition_hash);
        // All apparent names have the same definition. Move it into the final output.
        let mut rules = std::iter::repeat_n(rule, names.len());
        for apparent in names {
            targets.push(process(RepositoryRow {
                target: repository_target(rules.next().unwrap(), &apparent, &ordinary_dependencies),
                memberships: memberships.clone(),
                generated,
                ordinary_complete,
            }));
        }
    }
    targets
}

fn parse_bazel_version(text: &str) -> Option<BazelVersion> {
    let version = text.lines().find_map(|line| {
        line.strip_prefix("Build label: ")
            .or_else(|| line.strip_prefix("bazel "))
    })?;
    let mut parts = version
        .split('-')
        .next()
        .unwrap_or(version)
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u32>()
                .unwrap_or(0)
        });
    Some(BazelVersion(
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    ))
}

/// Parses `bazel info release output_base starlark-semantics`. `None` unless every key is there.
fn parse_server_info(text: &str) -> Option<ServerInfo> {
    let mut release = None;
    let mut output_base = None;
    let mut semantics = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(": ") else {
            continue;
        };
        match key {
            "release" => release = Some(value.trim()),
            "output_base" => output_base = Some(PathBuf::from(value.trim())),
            "starlark-semantics" => semantics = Some(value.trim()),
            _ => {}
        }
    }
    let (release, output_base, semantics) = (release?, output_base?, semantics?);
    // `release 8.8.2`, or `development version` for a source build, whose version stays unknown.
    let version = release
        .strip_prefix("release ")
        .filter(|version| version.starts_with(|c: char| c.is_ascii_digit()))
        .and_then(|version| parse_bazel_version(&format!("bazel {version}")));
    // The semantics print only flags that differ from their defaults: WORKSPACE is on by default
    // before Bazel 8, off from 8 on, and gone in 9.
    let external_package = if semantics.contains("enable_workspace=true") {
        Some(true)
    } else if semantics.contains("enable_workspace=false") {
        Some(false)
    } else {
        version.map(|version| version < BazelVersion(8, 0, 0))
    };
    Some(ServerInfo {
        version,
        output_base: Some(output_base),
        external_package,
    })
}

fn is_external_package_error(message: &str) -> bool {
    message.contains("no such package 'external'")
        || message.contains("//external package is not available")
}

fn deduplicate_targets(mut targets: Vec<Target>) -> Result<Vec<Target>> {
    // Decide which targets to keep while their names can still be borrowed.
    let mut keep = {
        let mut seen = HashSet::new();
        targets
            .iter()
            .map(|target| target_name(target).is_some_and(|name| seen.insert(name)))
            .collect::<Vec<_>>()
            .into_iter()
    };
    targets.retain(|_| keep.next().expect("one decision per target"));
    Ok(targets)
}

pub fn decode_delimited<M: Message>(path: &Path) -> Result<Vec<M>> {
    let mut reader = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut batch = RawBatch::default();
    let mut messages = Vec::new();
    let mut decoded = Vec::new();
    while read_raw_batch(&mut reader, &mut batch)? {
        batch
            .slices()
            .map(|bytes| buffa::DecodeOptions::new().decode_from_slice::<M>(bytes))
            .collect_into_vec(&mut decoded);
        messages.reserve(decoded.len());
        for message in decoded.drain(..) {
            messages.push(message?);
        }
    }
    Ok(messages)
}

/// Upper bounds on one decode batch. Batching keeps peak memory proportional to
/// the batch rather than to the whole query output, which for a large workspace
/// is gigabytes.
///
/// A fully decoded rule carries every attribute (implicit ones included, several
/// hundred bytes each) until the batch transform prehashes and drops them, so the
/// message bound sets most of the decode peak (generate-hashes decodes lean, see
/// [`decode_lean_target`], and leaves those attributes in the batch's raw buffer). On the large perf-proof workload,
/// 8192 instead of 1024 raised generate-hashes' peak RSS from about 160 to 245 MiB;
/// below 1024 the peak no longer moves, and each batch stays large enough to decode
/// in parallel.
const DECODE_BATCH_MESSAGES: usize = 1024;
const DECODE_BATCH_BYTES: usize = 32 * 1024 * 1024;

/// Raw, still-encoded messages read off the stream, kept in one contiguous
/// buffer so a batch costs one allocation rather than one per message.
#[derive(Default)]
struct RawBatch {
    buffer: Vec<u8>,
    bounds: Vec<(usize, usize)>,
}

impl RawBatch {
    fn clear(&mut self) {
        self.buffer.clear();
        self.bounds.clear();
    }

    fn slices(&self) -> impl IndexedParallelIterator<Item = &[u8]> {
        self.bounds
            .par_iter()
            .map(|(start, end)| &self.buffer[*start..*end])
    }
}

/// Reads a length-delimited varint prefix, returning `None` at end of stream.
fn read_message_length(reader: &mut impl BufRead) -> Result<Option<usize>> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let available = reader.fill_buf()?;
        let Some(byte) = available.first().copied() else {
            if shift == 0 {
                return Ok(None);
            }
            bail!("truncated length prefix in Bazel query output");
        };
        reader.consume(1);
        if shift == 63 && byte > 1 {
            bail!("length prefix in Bazel query output overflows a u64");
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            if value > u64::from(buffa::message::MAX_MESSAGE_BYTES) {
                bail!("message exceeds the protobuf size limit");
            }
            return usize::try_from(value)
                .map(Some)
                .map_err(|_| anyhow!("length prefix {value} does not fit in usize"));
        }
        shift += 7;
    }
}

/// Fills `batch` with up to the batch limits worth of encoded messages,
/// returning false once the stream is exhausted.
fn read_raw_batch(reader: &mut impl BufRead, batch: &mut RawBatch) -> Result<bool> {
    batch.clear();
    while batch.bounds.len() < DECODE_BATCH_MESSAGES && batch.buffer.len() < DECODE_BATCH_BYTES {
        let Some(length) = read_message_length(reader)? else {
            break;
        };
        let start = batch.buffer.len();
        // Grow with bytes actually delivered if the stream declares a large frame.
        batch.buffer.reserve(length.min(64 * 1024));
        let copied = std::io::copy(
            &mut reader.take(length as u64),
            &mut &mut batch.buffer as &mut dyn std::io::Write,
        )?;
        if copied != length as u64 {
            bail!("truncated message in Bazel query output");
        }
        batch.bounds.push((start, batch.buffer.len()));
    }
    Ok(!batch.bounds.is_empty())
}

/// Decodes one `bazel query` target, with the rule's attribute vector allocated at its final
/// size up front.
///
/// Bazel prints every attribute of a rule, implicit ones included, so a rule carries dozens of
/// them, and an `Attribute` is several hundred bytes. Decoded with no capacity hint the vector
/// grows by doubling, copying every attribute decoded so far at each step and leaving pages of
/// freed buffers behind. Counting the attribute fields first costs one pass over the rule's
/// top-level tags, skipping each attribute by its length.
fn decode_query_target(bytes: &[u8]) -> Result<Target, buffa::DecodeError> {
    let mut target = Target::default();
    if let Some(attributes) = rule_attribute_count(bytes).filter(|count| *count > 0) {
        target
            .rule
            .get_or_insert_default()
            .attribute
            .reserve_exact(attributes);
    }
    target.merge_from_slice(bytes)?;
    Ok(target)
}

/// `Target.rule` (field 2) and `Rule.attribute` (field 4), both length-delimited.
const TARGET_RULE_TAG: u64 = 2 << 3 | 2;
const RULE_ATTRIBUTE_TAG: u64 = 4 << 3 | 2;

/// The number of `Rule.attribute` entries in an encoded `Target`, summed over every `rule` field
/// (repeated occurrences of a singular message merge). `None` when the target has no rule or the
/// bytes do not parse as a sequence of fields, in which case decoding reports the error.
fn rule_attribute_count(target: &[u8]) -> Option<usize> {
    let mut found_rule = false;
    let mut attributes = 0;
    for field in ProtoFields(target) {
        let (tag, payload) = field?;
        if tag == TARGET_RULE_TAG {
            found_rule = true;
            for field in ProtoFields(payload?) {
                let (tag, _) = field?;
                attributes += usize::from(tag == RULE_ATTRIBUTE_TAG);
            }
        }
    }
    found_rule.then_some(attributes)
}

/// Iterates the top-level fields of an encoded message as `(tag, payload)`, where the payload is
/// `Some` for length-delimited fields. Yields `None` and stops at the first malformed field.
struct ProtoFields<'a>(&'a [u8]);

impl<'a> ProtoFields<'a> {
    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for (index, byte) in self.0.iter().take(10).enumerate() {
            value |= u64::from(byte & 0x7f) << (7 * index);
            if byte & 0x80 == 0 {
                self.0 = &self.0[index + 1..];
                return Some(value);
            }
        }
        None
    }

    fn skip(&mut self, length: usize) -> Option<&'a [u8]> {
        let (skipped, rest) = (length <= self.0.len()).then(|| self.0.split_at(length))?;
        self.0 = rest;
        Some(skipped)
    }
}

impl<'a> Iterator for ProtoFields<'a> {
    type Item = Option<(u64, Option<&'a [u8]>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        let field = (|| {
            let tag = self.varint()?;
            let payload = match tag & 7 {
                0 => self.varint().map(|_| None)?,
                1 => self.skip(8).map(|_| None)?,
                2 => {
                    let length = usize::try_from(self.varint()?).ok()?;
                    Some(self.skip(length)?)
                }
                5 => self.skip(4).map(|_| None)?,
                _ => return None,
            };
            Some((tag, payload))
        })();
        if field.is_none() {
            self.0 = &[];
        }
        Some(field)
    }
}

/// The encoded messages of a batch's lean-decoded targets, one entry per target of the batch
/// handed to the transform: the bytes [`decode_lean_target`] decoded the target from, or empty
/// when it was decoded in full. The whole slice is empty when the stream is not decoded lean.
/// [`encoded_rule_attributes`] reads the attributes the decoded target lacks back out of them.
pub(crate) type EncodedTargets<'a> = [&'a [u8]];

#[cfg(test)]
fn decode_target_stream(
    path: &Path,
    lean: bool,
    transform: &mut impl FnMut(&mut [Target], &EncodedTargets<'_>),
) -> Result<Vec<Target>> {
    decode_target_stream_filtered(path, lean, transform, |_| true)
}

fn decode_target_stream_filtered(
    path: &Path,
    lean: bool,
    transform: &mut impl FnMut(&mut [Target], &EncodedTargets<'_>),
    mut retain: impl FnMut(&Target) -> bool,
) -> Result<Vec<Target>> {
    let mut reader = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut targets = Vec::new();
    let mut batch = RawBatch::default();
    let mut decoded = Vec::new();
    while read_raw_batch(&mut reader, &mut batch)? {
        let start = targets.len();
        let mut encoded = Vec::new();
        if lean {
            let mut lowered = Vec::new();
            batch
                .slices()
                .map_init(LeanScratch::default, |scratch, bytes| {
                    decode_lean_target(bytes, scratch).map(|(target, lean)| {
                        (lower_query_target(target), if lean { bytes } else { &[] })
                    })
                })
                .collect_into_vec(&mut lowered);
            targets.reserve(lowered.len());
            for result in lowered {
                let (target, bytes) = result?;
                if let Some(target) = target.filter(&mut retain) {
                    targets.push(target);
                    encoded.push(bytes);
                }
            }
        } else {
            batch
                .slices()
                .map(|bytes| {
                    decode_query_target(bytes)
                        .map(|target| {
                            if target.r#type == target::Discriminator::SourceFile {
                                // The finite alias closure observes source location and declared
                                // visibility before compaction. Other rows still compact in parallel.
                                Some(target)
                            } else {
                                lower_query_target(target)
                            }
                        })
                        .map_err(anyhow::Error::from)
                })
                .collect_into_vec(&mut decoded);
            targets.reserve(decoded.len());
            for lowered in decoded.drain(..) {
                if let Some(mut target) = lowered?.filter(&mut retain) {
                    if target.r#type == target::Discriminator::SourceFile {
                        compact_target(&mut target);
                    }
                    targets.push(target);
                }
            }
        }
        transform(&mut targets[start..], &encoded);
    }
    Ok(targets)
}

/// A rule attribute left encoded: [`decode_lean_target`] skips decoding it because hashing is
/// the only thing that reads it, and hashing reads its encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EncodedAttribute<'a> {
    /// Index among all of the rule's attributes, in query order.
    pub(crate) position: usize,
    pub(crate) name: &'a str,
    /// The attribute message's bytes, as Bazel wrote them.
    pub(crate) bytes: &'a [u8],
}

/// Attributes that stay decoded in a lean target: what [`lower_query_target`] and the
/// hashing pass read after prehashing (`tags`, `visibility`).
fn decoded_in_lean_target(name: &str) -> bool {
    matches!(name, "tags" | "visibility" | "toolchain")
}

/// One protobuf field read off the wire: its number and its whole encoding (key included).
struct WireField<'a> {
    number: u32,
    /// Key and value bytes, as written.
    encoded: &'a [u8],
    /// The payload of a length-delimited field.
    payload: Option<&'a [u8]>,
}

fn read_wire_varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Iterates the top-level fields of an encoded message. Yields `None` once on anything this
/// reader does not handle (groups, truncation, field number 0), so the caller can fall back to
/// the ordinary decoder, which reports the error.
struct WireFields<'a> {
    bytes: &'a [u8],
    at: usize,
}

fn wire_fields(bytes: &[u8]) -> WireFields<'_> {
    WireFields { bytes, at: 0 }
}

impl<'a> WireFields<'a> {
    fn read(&mut self) -> Option<WireField<'a>> {
        let bytes = self.bytes;
        let start = self.at;
        let mut at = start;
        let key = read_wire_varint(bytes, &mut at)?;
        let number = u32::try_from(key >> 3).ok().filter(|number| *number != 0)?;
        let mut payload = None;
        match key & 7 {
            0 => {
                read_wire_varint(bytes, &mut at)?;
            }
            1 => at = at.checked_add(8)?,
            2 => {
                let length = usize::try_from(read_wire_varint(bytes, &mut at)?).ok()?;
                let end = at.checked_add(length)?;
                payload = Some(bytes.get(at..end)?);
                at = end;
            }
            5 => at = at.checked_add(4)?,
            _ => return None,
        }
        let encoded = bytes.get(start..at)?;
        self.at = at;
        Some(WireField {
            number,
            encoded,
            payload,
        })
    }
}

impl<'a> Iterator for WireFields<'a> {
    type Item = Option<WireField<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.bytes.len() {
            return None;
        }
        let field = self.read();
        if field.is_none() {
            // Stop after reporting the malformed field once.
            self.at = self.bytes.len();
        }
        Some(field)
    }
}

/// The last occurrence of a length-delimited field (protobuf's last-one-wins for a singular
/// field), or `None` when the message is malformed or the field is absent.
fn last_payload(bytes: &[u8], number: u32) -> Option<&[u8]> {
    let mut last = None;
    for field in wire_fields(bytes) {
        let field = field?;
        if field.number == number {
            last = Some(field.payload?);
        }
    }
    last
}

fn push_wire_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Decodes a queried `Target` without materializing the rule attributes nothing but hashing
/// reads, and returns whether it did. Those attributes are absent from the decoded rule (the ones
/// [`decoded_in_lean_target`] names stay decoded); [`encoded_rule_attributes`] reads them back
/// out of `bytes`, still encoded. A rule attribute's encoding
/// is what the rule hash covers ([`crate::hash`] hashes each attribute's message bytes), so
/// hashing Bazel's bytes directly skips a decode, an allocation per string and a re-encode per
/// attribute.
///
/// Repository rules (`//external:*`) are decoded in full: their hashing and loading inputs read
/// URL, checksum and path attributes. So is anything this wire reader does not handle, a target
/// that repeats its rule field (which protobuf merges), or an attribute that is malformed or whose
/// name is not UTF-8. `scratch` holds the re-framed message and is reused between calls.
fn decode_lean_target(bytes: &[u8], scratch: &mut LeanScratch) -> Result<(Target, bool)> {
    match split_lean_target(bytes, scratch) {
        Some(()) => Ok((
            <Target as Message>::decode_from_slice(&scratch.target)?,
            true,
        )),
        None => Ok((decode_query_target(bytes)?, false)),
    }
}

/// Appends to `out` the attributes [`decode_lean_target`] left out of the target it decoded lean
/// from `bytes`, in query order. Only meaningful for such bytes; empty `bytes` add nothing.
pub(crate) fn encoded_rule_attributes<'a>(bytes: &'a [u8], out: &mut Vec<EncodedAttribute<'a>>) {
    let Some(rule_bytes) = single_rule_field(bytes) else {
        return;
    };
    for (position, (attribute, name)) in rule_attributes(rule_bytes).flatten().enumerate() {
        if !decoded_in_lean_target(name) {
            out.push(EncodedAttribute {
                position,
                name,
                bytes: attribute,
            });
        }
    }
}

/// The payload of the target's rule field; `None` when it is absent, repeated (protobuf merges
/// repeated message fields; that is left to the decoder) or the message is malformed.
fn single_rule_field(bytes: &[u8]) -> Option<&[u8]> {
    let mut rule_bytes = None;
    for field in wire_fields(bytes) {
        let field = field?;
        if field.number == 2 {
            if rule_bytes.is_some() {
                return None;
            }
            rule_bytes = Some(field.payload?);
        }
    }
    rule_bytes
}

/// The rule's attribute messages with their names, in order. Yields `None` for an attribute
/// that is malformed or whose name is not UTF-8.
fn rule_attributes(rule_bytes: &[u8]) -> impl Iterator<Item = Option<(&[u8], &str)>> {
    wire_fields(rule_bytes)
        .filter(|field| field.as_ref().is_none_or(|field| field.number == 4))
        .map(|field| {
            let attribute = field?.payload?;
            let name = std::str::from_utf8(last_payload(attribute, 1)?).ok()?;
            Some((attribute, name))
        })
}

/// Buffers [`decode_lean_target`] re-frames a message in, kept per decoding thread.
#[derive(Default)]
struct LeanScratch {
    target: Vec<u8>,
    rule: Vec<u8>,
}

/// Writes `bytes` without its hashed-only rule attributes into `scratch.target`; `None` when the
/// target must be decoded as is.
fn split_lean_target(bytes: &[u8], scratch: &mut LeanScratch) -> Option<()> {
    let rule_bytes = single_rule_field(bytes)?;
    let mut rule_name = None;
    scratch.rule.clear();
    for field in wire_fields(rule_bytes) {
        let field = field?;
        match field.number {
            1 => rule_name = Some(field.payload?),
            4 => {
                let name = std::str::from_utf8(last_payload(field.payload?, 1)?).ok()?;
                if !decoded_in_lean_target(name) {
                    continue;
                }
            }
            _ => {}
        }
        scratch.rule.extend_from_slice(field.encoded);
    }
    if rule_name?.starts_with(b"//external:") {
        return None;
    }
    // Re-frame the target around the rule's remaining fields, in their order.
    scratch.target.clear();
    for field in wire_fields(bytes).flatten() {
        if field.number == 2 {
            push_wire_varint(&mut scratch.target, (2 << 3) | 2);
            push_wire_varint(&mut scratch.target, scratch.rule.len() as u64);
            scratch.target.extend_from_slice(&scratch.rule);
        } else {
            scratch.target.extend_from_slice(field.encoded);
        }
    }
    Some(())
}

fn decode_cquery_stream(
    path: &Path,
    streamed: bool,
    transform: &mut impl FnMut(&mut [Target]),
    roots: Option<&mut Vec<(String, String)>>,
    compatible: Option<&HashSet<String>>,
) -> Result<Vec<Target>> {
    let graph = decode_cquery_graph(path, streamed, transform, roots, compatible, false, false)?;
    Ok(graph.targets)
}

fn decode_cquery_graph(
    path: &Path,
    streamed: bool,
    transform: &mut impl FnMut(&mut [Target]),
    roots: Option<&mut Vec<(String, String)>>,
    compatible: Option<&HashSet<String>>,
    verify: bool,
    marker_expected: bool,
) -> Result<QueryGraph> {
    let mut decoder = CqueryDecoder {
        root_labels: roots.as_ref().map(|_| Vec::new()),
        known_null_configurations: (compatible.is_some() || verify).then(HashSet::new),
        verify,
        ..CqueryDecoder::default()
    };
    let mut marker = false;
    let mut invalid_marker = false;
    if streamed {
        // Results decode in parallel, a raw batch at a time; naming them stays in stream order
        // because a result's configuration may be defined by an earlier message.
        let mut reader = BufReader::with_capacity(1 << 20, File::open(path)?);
        let mut batch = RawBatch::default();
        let mut decoded = Vec::new();
        while read_raw_batch(&mut reader, &mut batch)? {
            batch
                .slices()
                .map(|bytes| {
                    CqueryResult::decode_from_slice(bytes)
                        // The producer writes field 5 = true in its own terminal frame.
                        .map(|result| (result, bytes == [0x28, 0x01]))
                        .map_err(anyhow::Error::from)
                })
                .collect_into_vec(&mut decoded);
            for result in decoded.drain(..) {
                let (mut result, marker_only) = result?;
                if marker {
                    invalid_marker = true;
                }
                if result.explicit_aspect_inputs {
                    marker = true;
                    invalid_marker |= !marker_expected || !marker_only;
                }
                retain_compatible_configured_targets(&mut result, compatible, roots.is_some());
                decoder.push(result, transform);
            }
        }
    } else {
        let bytes = fs::read(path)?;
        let mut result = CqueryResult::decode_from_slice(&bytes)?;
        marker = result.explicit_aspect_inputs;
        invalid_marker = marker && !marker_expected;
        retain_compatible_configured_targets(&mut result, compatible, roots.is_some());
        decoder.push(result, transform);
    }
    decoder.explicit_aspect_inputs = marker_expected && marker && !invalid_marker;
    decoder.invalid_explicit_aspect_marker = invalid_marker;
    decoder.finish(transform)?;
    if roots.is_some() || compatible.is_some() || verify {
        // Bazel can force the same toolchain onto different execution platforms without
        // distinguishing those instances in the configured labels on incoming edges.
        // Catch the collision before repository and query results are deduplicated.
        let mut seen = HashSet::with_capacity(decoder.targets.len());
        for key in decoder.targets.iter().filter_map(target_name) {
            if !seen.insert(key) {
                bail!("cquery returned multiple targets for configured key {key}");
            }
        }
    }
    if let (Some(roots), Some(resolved)) = (roots, decoder.root_labels) {
        roots.extend(resolved);
    }
    Ok(QueryGraph {
        targets: decoder.targets,
        aspects: decoder.observed_aspects,
        source_locations: decoder.source_locations,
    })
}

fn retain_compatible_configured_targets(
    result: &mut CqueryResult,
    compatible: Option<&HashSet<String>>,
    requested_roots: bool,
) {
    let Some(compatible) = compatible else {
        return;
    };
    result.results.retain(|configured| {
        configured.target.as_option().is_some_and(|target| {
            (requested_roots
                && matches!(
                    target.r#type,
                    target::Discriminator::SourceFile | target::Discriminator::PackageGroup
                ))
                || target_name(target)
                    .is_some_and(|label| compatible.contains(main_repo_label_key(label)))
        })
    });
}

/// Separator between a label and the checksum of the configuration a target was analyzed in.
///
/// A cquery graph is a graph of *configured* targets: one label can be analyzed in several
/// configurations (a transition, an exec dependency, a `select()` that resolves differently),
/// and those instances have different dependencies. Bazel identifies each dependency edge by
/// `(label, configuration checksum)` in `configured_rule_input`, and that is the identity
/// the rule-input encoding folds into `label|checksum`. Configured targets are named the same
/// way, so an edge resolves to the instance it points at rather than to whichever instance of
/// the label happened to come first. Collapsing by label loses edges and, when an instance
/// depends on the same label in another configuration, reports a cycle the configured graph
/// does not have.
pub const CONFIGURED_SEPARATOR: char = '|';
const EXPLICIT_ASPECT_PREFIX: &str = "\0aspect:";

/// Bazel reports the null configuration (source files, package groups) with this checksum.
const NULL_CONFIGURATION_CHECKSUM: &str = "null";

/// Decodes `CqueryResult` messages into targets named by their configured identity.
///
/// Each result carries its configuration inline (the deprecated `configuration` field, which
/// Bazel still populates) and by id into `CqueryResult.configurations`. A streamed result's
/// id table only arrives after the results that use it, so a result whose inline checksum is
/// missing waits until the stream ends to be named.
///
/// Resolving a checksum is the only step that depends on earlier messages. Named targets queue
/// in `ready` and are lowered, configured and handed to `transform` a batch at a time in
/// parallel, in the order they were named.
#[derive(Default)]
struct CqueryDecoder {
    targets: Vec<Target>,
    configurations: HashMap<u32, String>,
    pending: Vec<(Target, u32)>,
    ready: Vec<(Target, Option<String>)>,
    lowered: Vec<Option<Target>>,
    root_labels: Option<Vec<(String, String)>>,
    known_null_configurations: Option<HashSet<u32>>,
    ambiguous_label: Option<String>,
    invalid_configuration: Option<u32>,
    verify: bool,
    aspect_identities: Vec<ConfiguredAspect>,
    aspect_results: Vec<ConfiguredAspectResult>,
    observed_aspects: Option<Vec<ObservedAspect>>,
    explicit_aspect_inputs: bool,
    invalid_explicit_aspect_marker: bool,
    source_locations: BTreeMap<String, Option<String>>,
}

impl CqueryDecoder {
    fn push(&mut self, result: CqueryResult, transform: &mut impl FnMut(&mut [Target])) {
        if self.verify {
            self.aspect_identities.extend(result.aspects);
            self.aspect_results.extend(result.aspect_results);
        }
        for configuration in result.configurations {
            if let Some(checksum) = non_null_checksum(&configuration.checksum) {
                if self.verify
                    && (self
                        .configurations
                        .get(&configuration.id)
                        .is_some_and(|previous| previous != checksum)
                        || self
                            .known_null_configurations
                            .as_ref()
                            .is_some_and(|nulls| nulls.contains(&configuration.id)))
                {
                    self.invalid_configuration = Some(configuration.id);
                }
                self.configurations
                    .insert(configuration.id, checksum.to_owned());
            } else if configuration.checksum == NULL_CONFIGURATION_CHECKSUM {
                if self.verify && self.configurations.contains_key(&configuration.id) {
                    self.invalid_configuration = Some(configuration.id);
                }
                if let Some(nulls) = &mut self.known_null_configurations {
                    nulls.insert(configuration.id);
                }
            }
        }
        for configured in result.results {
            let ConfiguredTarget {
                target,
                configuration,
                configuration_id,
                ..
            } = configured;
            let Some(target) = target.into_option() else {
                continue;
            };
            if self.verify {
                if let Some(source) = target.source_file.as_option() {
                    match self.source_locations.entry(source.name.clone()) {
                        Entry::Vacant(entry) => {
                            entry.insert(source.location.clone());
                        }
                        Entry::Occupied(mut entry) if entry.get() != &source.location => {
                            entry.insert(None);
                        }
                        Entry::Occupied(_) => {}
                    }
                }
            }
            let inline = configuration
                .as_option()
                .and_then(|configuration| non_null_checksum(&configuration.checksum))
                .map(str::to_owned);
            let known_null = self.known_null_configurations.is_some()
                && configuration.as_option().is_some_and(|configuration| {
                    configuration.checksum == NULL_CONFIGURATION_CHECKSUM
                });
            if self.verify && configuration_id != 0 {
                let referenced = self.configurations.get(&configuration_id);
                if inline
                    .as_ref()
                    .zip(referenced)
                    .is_some_and(|(inline, referenced)| inline != referenced)
                    || (known_null && referenced.is_some())
                    || (inline.is_some()
                        && self
                            .known_null_configurations
                            .as_ref()
                            .is_some_and(|nulls| nulls.contains(&configuration_id)))
                {
                    self.invalid_configuration = Some(configuration_id);
                }
                if let Some(inline) = &inline {
                    self.configurations
                        .entry(configuration_id)
                        .or_insert_with(|| inline.clone());
                } else if known_null {
                    self.known_null_configurations
                        .as_mut()
                        .expect("verification tracks null configurations")
                        .insert(configuration_id);
                }
            }
            let checksum = inline.or_else(|| self.configurations.get(&configuration_id).cloned());
            if checksum.is_none() && configuration_id != 0 && !known_null {
                self.pending.push((target, configuration_id));
                continue;
            }
            self.ready.push((target, checksum));
            if self.ready.len() >= DECODE_BATCH_MESSAGES {
                self.flush(transform);
            }
        }
    }

    fn flush(&mut self, transform: &mut impl FnMut(&mut [Target])) {
        if self.ready.is_empty() {
            return;
        }
        if self.known_null_configurations.is_some() && self.ambiguous_label.is_none() {
            self.ambiguous_label = self.ready.iter().find_map(|(target, checksum)| {
                let bare = target.r#type == target::Discriminator::SourceFile
                    || (checksum.is_none()
                        && target.r#type != target::Discriminator::EnvironmentGroup);
                let label = target_name(target)?;
                (bare && target_label(label) != label).then(|| label.to_owned())
            });
        }
        let labels = self.root_labels.as_ref().map(|_| {
            self.ready
                .iter()
                .map(|(target, _)| target_name(target).map(str::to_owned))
                .collect::<Vec<_>>()
        });
        let verify = self.verify;
        self.ready
            .par_drain(..)
            .map(|(target, checksum)| {
                let package_group = verify && target.r#type == target::Discriminator::PackageGroup;
                let mut target = if verify {
                    lower_target_without_compaction(target)?
                } else {
                    lower_target(target)?
                };
                if package_group {
                    let rule = target.rule.as_option_mut()?;
                    rule.configured_rule_input = rule
                        .rule_input
                        .iter()
                        .map(|label| ConfiguredRuleInput {
                            label: Some(label.clone()),
                            ..Default::default()
                        })
                        .collect();
                }
                if let Some(checksum) = checksum {
                    configure_target(&mut target, &checksum);
                }
                Some(target)
            })
            .collect_into_vec(&mut self.lowered);
        if let (Some(labels), Some(roots)) = (labels, self.root_labels.as_mut()) {
            for (label, target) in labels.into_iter().zip(&self.lowered) {
                if let (Some(label), Some(key)) = (label, target.as_ref().and_then(target_name)) {
                    roots.push((label, key.to_owned()));
                }
            }
        }
        let start = self.targets.len();
        self.targets.extend(self.lowered.drain(..).flatten());
        if self.targets.len() > start {
            transform(&mut self.targets[start..]);
        }
    }

    fn finish(&mut self, transform: &mut impl FnMut(&mut [Target])) -> Result<()> {
        if let Some(id) = self.invalid_configuration {
            bail!("explicit cquery returned conflicting configuration {id}");
        }
        self.flush(transform);
        let pending = std::mem::take(&mut self.pending);
        for (target, configuration_id) in pending {
            let checksum = self.configurations.get(&configuration_id).cloned();
            if checksum.is_none()
                && self
                    .known_null_configurations
                    .as_ref()
                    .is_some_and(|nulls| !nulls.contains(&configuration_id))
            {
                bail!(
                    "configured target {} referenced missing configuration {configuration_id}",
                    target_name(&target).unwrap_or("<unknown>")
                );
            }
            self.ready.push((target, checksum));
            if self.ready.len() >= DECODE_BATCH_MESSAGES {
                self.flush(transform);
            }
        }
        self.flush(transform);
        if let Some(label) = &self.ambiguous_label {
            bail!("configured target {label} has a bare label ambiguous with the configured-key encoding");
        }
        if self.verify
            && !self.invalid_explicit_aspect_marker
            && (self.explicit_aspect_inputs
                || !self.aspect_identities.is_empty()
                || !self.aspect_results.is_empty()
                || self.targets.iter().any(|target| {
                    target.rule.as_option().is_some_and(|rule| {
                        rule.configured_rule_input
                            .iter()
                            .any(|input| input.aspect_id.is_some())
                    })
                }))
        {
            self.observed_aspects = Some(self.resolve_explicit_aspects()?);
        }
        Ok(())
    }

    fn resolve_explicit_aspects(&mut self) -> Result<Vec<ObservedAspect>> {
        let mut identities = BTreeMap::new();
        for identity in std::mem::take(&mut self.aspect_identities) {
            if identity.id == 0 || identities.insert(identity.id, identity).is_some() {
                bail!("explicit cquery returned a duplicate or zero aspect identity");
            }
        }
        let mut resolved = BTreeMap::<u32, (String, String)>::new();
        let mut identity_digests = HashSet::new();
        let mut visiting = HashSet::new();
        for &id in identities.keys() {
            let mut pending = vec![(id, false)];
            while let Some((id, complete)) = pending.pop() {
                if resolved.contains_key(&id) {
                    continue;
                }
                let identity = identities
                    .get(&id)
                    .with_context(|| format!("explicit cquery referenced missing aspect {id}"))?;
                if !complete {
                    if !visiting.insert(id) {
                        bail!("explicit cquery aspect identities contain a cycle at {id}");
                    }
                    pending.push((id, true));
                    pending.extend(
                        identity
                            .base_aspect_ids
                            .iter()
                            .rev()
                            .map(|&base| (base, false)),
                    );
                    continue;
                }
                visiting.remove(&id);
                let descriptor = identity
                    .aspect_descriptor
                    .as_option()
                    .context("explicit cquery aspect has no descriptor")?;
                if descriptor.name.is_empty() {
                    bail!("explicit cquery aspect has an empty descriptor name");
                }
                let mut parameters = BTreeMap::<&str, Vec<&str>>::new();
                for parameter in &descriptor.parameters {
                    parameters
                        .entry(&parameter.key)
                        .or_default()
                        .push(&parameter.value);
                }
                let base = identity
                    .base_target
                    .as_option()
                    .context("explicit cquery aspect has no base target")?;
                if base.aspect_id.is_some() {
                    bail!("explicit cquery aspect base is not a configured target");
                }
                let base = explicit_configured_input(
                    base,
                    &self.configurations,
                    self.known_null_configurations.as_ref(),
                )?;
                let predecessors = identity
                    .base_aspect_ids
                    .iter()
                    .map(|id| {
                        resolved
                            .get(id)
                            .map(|(key, _)| key.as_str())
                            .context("explicit cquery aspect predecessor was not resolved")
                    })
                    .collect::<Result<Vec<_>>>()?;
                let encoded = serde_json::to_vec(&(
                    &descriptor.name,
                    parameters,
                    &base,
                    &identity.execution_platform,
                    identity.skips_rule_transition,
                    predecessors,
                ))?;
                let mut digest = Sha256::new();
                digest.update(b"configuredAspectIdentity=1\0");
                digest.update(encoded);
                let digest: [u8; 32] = digest.finalize().into();
                if !identity_digests.insert(digest) {
                    bail!("explicit cquery returned an ambiguous aspect identity");
                }
                let key = format!("{EXPLICIT_ASPECT_PREFIX}{}", hex::encode(digest));
                resolved.insert(id, (key, base));
            }
        }
        let input_key = |input: &ConfiguredRuleInput| {
            let base = explicit_configured_input(
                input,
                &self.configurations,
                self.known_null_configurations.as_ref(),
            )?;
            match input.aspect_id {
                None => Ok(base),
                Some(0) => bail!("explicit cquery returned a zero aspect reference"),
                Some(id) => {
                    let (key, expected_base) = resolved.get(&id).with_context(|| {
                        format!("explicit cquery referenced missing aspect {id}")
                    })?;
                    if expected_base != &base {
                        bail!("explicit cquery aspect {id} referenced mismatched base {base}");
                    }
                    Ok(key.clone())
                }
            }
        };
        // A streamed identity table can follow the target rows that reference it.
        // Resolve those query-local IDs only after the complete table has arrived.
        for target in &mut self.targets {
            if target_name(target).is_some_and(|name| name.contains('\0')) {
                bail!("explicit cquery returned a target in the reserved aspect namespace");
            }
            if let Some(rule) = target.rule.as_option_mut() {
                for input in &mut rule.configured_rule_input {
                    *input = ConfiguredRuleInput {
                        label: Some(input_key(input)?),
                        ..Default::default()
                    };
                }
            }
        }
        let mut selected = HashSet::new();
        let mut observed = Vec::with_capacity(self.aspect_results.len());
        for aspect in std::mem::take(&mut self.aspect_results) {
            if aspect.aspect_id == 0 || !selected.insert(aspect.aspect_id) {
                bail!("explicit cquery returned a duplicate or zero selected aspect");
            }
            let (key, _) = resolved.get(&aspect.aspect_id).with_context(|| {
                format!(
                    "explicit cquery selected missing aspect {}",
                    aspect.aspect_id
                )
            })?;
            observed.push(ObservedAspect {
                key: key.clone(),
                inputs: aspect
                    .configured_rule_input
                    .iter()
                    .map(input_key)
                    .collect::<Result<_>>()?,
            });
        }
        Ok(observed)
    }
}

fn explicit_configured_input(
    input: &ConfiguredRuleInput,
    configurations: &HashMap<u32, String>,
    known_null: Option<&HashSet<u32>>,
) -> Result<String> {
    let label = input
        .label
        .as_deref()
        .context("explicit cquery input has no label")?;
    if label.contains('\0') {
        bail!("explicit cquery input uses the reserved aspect namespace");
    }
    let declared_value = input
        .configuration_checksum
        .as_deref()
        .filter(|checksum| !checksum.is_empty());
    let declared = declared_value.and_then(non_null_checksum);
    let referenced = match input.configuration_id.filter(|id| *id != 0) {
        Some(id) => match configurations.get(&id) {
            Some(checksum) => Some(Some(checksum.as_str())),
            None if known_null.is_some_and(|known| known.contains(&id)) => Some(None),
            None => bail!("explicit cquery input {label} referenced missing configuration {id}"),
        },
        None => None,
    };
    if declared_value.is_some() && referenced.is_some_and(|referenced| declared != referenced) {
        bail!("explicit cquery input {label} has mismatched configuration identities");
    }
    Ok(declared.or(referenced.flatten()).map_or_else(
        || label.to_owned(),
        |checksum| configured_key(label, checksum),
    ))
}

fn non_null_checksum(checksum: &str) -> Option<&str> {
    (!checksum.is_empty() && checksum != NULL_CONFIGURATION_CHECKSUM).then_some(checksum)
}

/// Names a rule or generated file by its configured identity, `label|checksum`.
///
/// Source files are never configured (Bazel reports them in the null configuration), so their
/// labels stay bare and keep matching the bare labels in `configured_rule_input`. A generated
/// file shares its generating rule's configuration, so that reference is named the same way.
fn configure_target(target: &mut Target, checksum: &str) {
    match target.r#type {
        target::Discriminator::Rule => {
            if let Some(rule) = target.rule.as_option_mut() {
                rule.name = configured_key(&rule.name, checksum);
            }
        }
        target::Discriminator::GeneratedFile => {
            if let Some(file) = target.generated_file.as_option_mut() {
                file.name = configured_key(&file.name, checksum);
                file.generating_rule = configured_key(&file.generating_rule, checksum);
            }
        }
        _ => {}
    }
}

pub fn configured_key(label: &str, checksum: &str) -> String {
    format!("{label}{CONFIGURED_SEPARATOR}{checksum}")
}

/// Splits a configured identity into its label and configuration checksum.
///
/// Bazel allows `|` in target names, so the suffix only counts as a checksum when it looks
/// like one (non-empty, hexadecimal); a bare label is returned unchanged.
pub fn split_configured(key: &str) -> (&str, Option<&str>) {
    match key.rsplit_once(CONFIGURED_SEPARATOR) {
        Some((label, checksum))
            if !checksum.is_empty() && checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            (label, Some(checksum))
        }
        _ => (key, None),
    }
}

/// The label of a configured identity, or the input unchanged when it is a bare label.
pub fn target_label(key: &str) -> &str {
    split_configured(key).0
}

/// One spelling of a main-repository label, for comparing labels Bazel printed in
/// different forms: `@@//pkg:name` (Bzlmod), `@//pkg:name` (WORKSPACE mode) and the
/// apparent `//pkg:name` that `repr()` gives a main-repository alias all become
/// `//pkg:name`. Other labels are returned unchanged.
pub(crate) fn main_repo_label_key(label: &str) -> &str {
    label
        .strip_prefix("@@")
        .or_else(|| label.strip_prefix('@'))
        .filter(|rest| rest.starts_with("//"))
        .unwrap_or(label)
}

pub fn target_name(target: &Target) -> Option<&str> {
    match target.r#type {
        target::Discriminator::Rule => target.rule.as_option().map(|rule| rule.name.as_str()),
        target::Discriminator::SourceFile => target
            .source_file
            .as_option()
            .map(|source| source.name.as_str()),
        target::Discriminator::GeneratedFile => target
            .generated_file
            .as_option()
            .map(|generated| generated.name.as_str()),
        target::Discriminator::PackageGroup => target
            .package_group
            .as_option()
            .map(|group| group.name.as_str()),
        target::Discriminator::EnvironmentGroup => target
            .environment_group
            .as_option()
            .map(|group| group.name.as_str()),
    }
}

fn lower_query_target(mut target: Target) -> Option<Target> {
    if let Some(rule) = target
        .rule
        .as_option_mut()
        .filter(|r| r.rule_class == "toolchain")
    {
        // Bazel's native toolchain rule points at its implementation with a NODEP_LABEL.
        // Query prints the label as a nodep string and omits it from rule_input.
        if let Some(implementation) = rule
            .attribute
            .iter()
            .find(|a| a.name == "toolchain" && a.nodep == Some(true))
        {
            rule.rule_input
                .extend(implementation.string_value.iter().cloned());
            if let Some(select) = implementation.selector_list.as_option() {
                rule.rule_input
                    .extend(select.elements.iter().flat_map(|element| {
                        element
                            .entries
                            .iter()
                            .filter_map(|entry| entry.string_value.clone())
                    }));
            }
        }
    }
    lower_target(target)
}

fn lower_target(target: Target) -> Option<Target> {
    let mut target = lower_target_without_compaction(target)?;
    compact_target(&mut target);
    Some(target)
}

fn lower_target_without_compaction(mut target: Target) -> Option<Target> {
    match target.r#type {
        target::Discriminator::Rule
        | target::Discriminator::SourceFile
        | target::Discriminator::GeneratedFile => Some(target),
        target::Discriminator::PackageGroup => {
            let group = target.package_group.take()?;
            target.r#type = target::Discriminator::Rule;
            target.rule = buffa::MessageField::some(package_group_rule(&group));
            Some(target)
        }
        target::Discriminator::EnvironmentGroup => {
            eprintln!("[Warn] Unsupported target type in the build graph: ENVIRONMENT_GROUP");
            None
        }
    }
}

fn compact_target(target: &mut Target) {
    target.__buffa_unknown_fields.clear();
    match target.r#type {
        target::Discriminator::Rule => {
            let Some(rule) = target.rule.as_option_mut() else {
                return;
            };
            rule.location = None;
            rule.rule_output.clear();
            rule.default_setting.clear();
            rule.DEPRECATED_public_by_default = None;
            rule.DEPRECATED_is_skylark = None;
            rule.definition_stack.clear();
            if !matches!(
                rule.rule_class_key.as_deref(),
                Some("toolchain" | "alias" | "filegroup" | "genrule")
            ) {
                rule.rule_class_key = None;
            }
            rule.__buffa_unknown_fields.clear();
        }
        target::Discriminator::SourceFile => {
            let Some(source) = target.source_file.as_option_mut() else {
                return;
            };
            source.location = None;
            source.package_group.clear();
            source.visibility_label.clear();
            source.feature.clear();
            source.license = buffa::MessageField::none();
            // A present BUILD error bit distinguishes an evaluated empty load list
            // from older formatters that omit all BUILD.bazel package information.
            if !matches!(
                source.name.rsplit_once(':'),
                Some((_, "BUILD" | "BUILD.bazel"))
            ) {
                source.package_contains_errors = None;
            }
            source.__buffa_unknown_fields.clear();
        }
        target::Discriminator::GeneratedFile => {
            let Some(generated) = target.generated_file.as_option_mut() else {
                return;
            };
            generated.location = None;
            generated.__buffa_unknown_fields.clear();
        }
        _ => {}
    }
}

fn package_group_rule(group: &PackageGroup) -> Rule {
    let packages = Attribute {
        name: "packages".to_owned(),
        r#type: crate::proto::blaze_query::attribute::Discriminator::StringList,
        string_list_value: group.contained_package.clone(),
        ..Default::default()
    };
    let mut rule = Rule {
        name: group.name.clone(),
        rule_class: "package_group".to_owned(),
        ..Default::default()
    };
    rule.attribute.push(packages);
    rule.rule_input = group.included_package_group.clone();
    // cquery follows configured inputs. Package groups always use the null configuration.
    rule.configured_rule_input = group
        .included_package_group
        .iter()
        .map(|label| ConfiguredRuleInput {
            label: Some(label.clone()),
            ..Default::default()
        })
        .collect();
    rule
}

struct RepositoryFiles {
    tree_hash: Option<String>,
    inputs_hash: Option<String>,
}

fn prepare_repository(
    repository: &mut Repository,
    workspace: &RepositoryWorkspace<'_>,
) -> RepositoryFiles {
    // Resolve physical inputs using the original attribute values.
    let files = RepositoryFiles {
        tree_hash: local_repository_content_hash(repository, workspace.path),
        inputs_hash: repository_local_inputs_hash(repository, workspace),
    };
    normalize_workspace_attributes(&mut repository.attribute, &workspace.roots);
    files
}

fn repository_rule(
    repository: Repository,
    files: RepositoryFiles,
    owner_definition_hash: Option<&str>,
) -> Rule {
    let mut rule = Rule {
        rule_class: repository
            .repo_rule_name
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "bzlmod_repo".to_owned()),
        attribute: repository.attribute,
        ..Default::default()
    };
    let mut push_attribute = |name: &str, value: String| {
        let mut attribute = Attribute::default();
        attribute.name = name.to_owned();
        attribute.r#type = crate::proto::blaze_query::attribute::Discriminator::String;
        attribute.string_value = Some(value);
        rule.attribute.push(attribute);
    };
    if let Some(content_hash) = files.tree_hash {
        push_attribute("_bazel_diff_content_hash", content_hash);
    }
    if let Some(inputs_hash) = files.inputs_hash {
        push_attribute("_bazel_diff_local_inputs_hash", inputs_hash);
    }
    if let Some(owner_hash) = owner_definition_hash {
        push_attribute("_bazel_diff_module_hash", owner_hash.to_owned());
    }
    rule
}

fn repository_target(mut rule: Rule, apparent: &str, dependencies: &BTreeSet<String>) -> Target {
    rule.name = format!("//external:{apparent}");
    rule.rule_input = dependencies
        .iter()
        .filter(|dependency| dependency.as_str() != apparent)
        .map(|dependency| format!("//external:{dependency}"))
        .collect();
    let mut target = Target::default();
    target.r#type = target::Discriminator::Rule;
    target.rule = buffa::MessageField::some(rule);
    target
}

/// Every main-repository label a repository rule's attributes point at: `patches`, `build_file`,
/// a `requirements_lock`, and so on. Attributes only name these files; a repository fetched with
/// a changed patch is a different repository, so their contents must reach its hash.
fn repository_local_input_labels(repository: &Repository) -> BTreeSet<&str> {
    attribute_main_repo_labels(&repository.attribute).collect()
}

/// Every main-repository label among the attributes' values: the files and directories a
/// repository rule reads from the workspace (`build_file`, `patches`, a requirements file).
pub(crate) fn attribute_main_repo_labels(attributes: &[Attribute]) -> impl Iterator<Item = &str> {
    attributes.iter().flat_map(|attr| {
        attribute_label_values(attr)
            .chain(
                attr.string_dict_value
                    .iter()
                    .map(|entry| entry.value.as_str()),
            )
            .chain(
                attr.label_dict_unary_value
                    .iter()
                    .map(|entry| entry.key.as_str()),
            )
            .filter(|value| crate::hash::main_repo_path(value).is_some())
    })
}

/// The slots that can name targets after the caller checks the attribute discriminator.
/// LABEL_DICT_UNARY is string -> label and LABEL_KEYED_STRING_DICT is label -> string.
fn attribute_label_values(attribute: &Attribute) -> impl Iterator<Item = &str> {
    attribute
        .string_value
        .iter()
        .chain(&attribute.string_list_value)
        .chain(
            attribute
                .label_dict_unary_value
                .iter()
                .map(|entry| &entry.value),
        )
        .chain(
            attribute
                .label_keyed_string_dict_value
                .iter()
                .map(|entry| &entry.key),
        )
        .chain(
            attribute
                .label_list_dict_value
                .iter()
                .flat_map(|entry| &entry.value),
        )
        // Bazel's AttributeFormatter emits LABEL_LIST_DICT in the same field as
        // STRING_LIST_DICT. Only the former is a typed repository input.
        .chain(
            attribute
                .string_list_dict_value
                .iter()
                .flat_map(|entry| &entry.value),
        )
        .map(String::as_str)
}

/// Digest of a repository's own definition: its rule, its attributes, and the local tree and
/// main-repository files behind them. Like the target hasher, it leaves out URLs whose content an
/// integrity pins: `url`/`urls` under `sha256`/`integrity`, and the URLs
/// `without_integrity_pinned_urls` drops.
fn repository_definition_hash(repository: &Repository, files: &RepositoryFiles) -> String {
    let pinned = repository.attribute.iter().any(|attribute| {
        matches!(attribute.name.as_str(), "sha256" | "integrity")
            && attribute
                .string_value
                .as_deref()
                .is_some_and(|value| !value.is_empty())
    });
    let mut hasher = Sha256::new();
    hasher.update(repository.repo_rule_name.as_deref().unwrap_or_default());
    hasher.update([0]);
    let mut encoded = Vec::new();
    for attribute in &repository.attribute {
        if pinned && matches!(attribute.name.as_str(), "url" | "urls") {
            continue;
        }
        encoded.clear();
        match crate::hash::without_integrity_pinned_urls(attribute, &repository.attribute) {
            Some(stripped) => stripped.encode_length_delimited(&mut encoded),
            None => attribute.encode_length_delimited(&mut encoded),
        }
        hasher.update(&encoded);
    }
    hasher.update([0]);
    if let Some(content_hash) = &files.tree_hash {
        hasher.update(content_hash);
    }
    hasher.update([0]);
    if let Some(inputs_hash) = &files.inputs_hash {
        hasher.update(inputs_hash);
    }
    hex::encode(hasher.finalize())
}

/// Hash workspace inputs and the exact file named by a standard http_archive
/// when Bazel identifies its source repository as a local checkout.
fn repository_local_inputs_hash(
    repository: &Repository,
    workspace: &RepositoryWorkspace<'_>,
) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut hashed_any = false;
    let mut record = |label: &str, digest: &[u8]| {
        hasher.update(label.as_bytes());
        hasher.update([0]);
        hasher.update(digest);
        hasher.update([0]);
        hashed_any = true;
    };
    for label in repository_local_input_labels(repository) {
        let Some(path) = crate::hash::main_repo_path(label) else {
            continue;
        };
        let absolute = workspace.path.join(path);
        let mut content = Sha256::new();
        if absolute.is_file() {
            content.update(fs::read(&absolute).ok()?);
        } else if absolute.is_dir() {
            crate::hash::hash_directory_into(&mut content, &absolute).ok()?;
        } else {
            continue;
        }
        record(label, &content.finalize());
    }
    if let Some((label, digest)) = workspace.archive_build_file(repository) {
        record(label, &digest);
    }
    hashed_any.then(|| hex::encode(hasher.finalize()))
}

fn local_repository_root(repository: &Repository, workspace: &Path) -> Option<PathBuf> {
    if repository.repo_rule_name.as_deref() != Some("local_repository") {
        return None;
    }
    let path = repository
        .attribute
        .iter()
        .find(|attribute| attribute.name == "path")
        .and_then(|attribute| attribute.string_value.as_deref())?;
    Some(workspace.join(path))
}

fn local_repository_content_hash(repository: &Repository, workspace: &Path) -> Option<String> {
    let root = local_repository_root(repository, workspace)?;
    if !root.is_dir() {
        return None;
    }
    let mut files = walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.file_name() != "MODULE.bazel.lock")
        .collect::<Vec<_>>();
    files.sort_by_key(|entry| entry.path().to_owned());
    let mut hasher = Sha256::new();
    for entry in files {
        let relative = entry.path().strip_prefix(&root).ok()?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(fs::read(entry.path()).ok()?);
        hasher.update([0]);
    }
    Some(hex::encode(hasher.finalize()))
}

struct RepositoryWorkspace<'a> {
    path: &'a Path,
    roots: Vec<String>,
    local_repositories: BTreeMap<String, PathBuf>,
}

impl<'a> RepositoryWorkspace<'a> {
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            roots: workspace_roots(path),
            local_repositories: BTreeMap::new(),
        }
    }

    fn from_repositories(path: &'a Path, repositories: &[Repository]) -> Self {
        let local_root = |repository: &Repository| {
            use crate::proto::blaze_query::attribute::Discriminator;
            // A user rule named local_repository need not map targets into path.
            // Bazel 8.6 through 8.8 omits the bzl label only for native RuleClass keys.
            if repository
                .repo_rule_bzl_label
                .as_deref()
                .is_some_and(|label| label != "@@bazel_tools//tools/build_defs/repo:local.bzl")
                || !repository
                    .attribute
                    .iter()
                    .find(|a| a.name == "path")
                    .is_some_and(|a| a.r#type == Discriminator::String)
            {
                return None;
            }
            local_repository_root(repository, path)
        };
        let mut workspace = Self::new(path);
        for repository in repositories {
            if let (Some(name), Some(root)) = (&repository.canonical_name, local_root(repository)) {
                workspace.local_repositories.insert(name.clone(), root);
            }
        }
        // The survey can return duplicate canonical names. A named input is only
        // local when every record for its source agrees on the same checkout.
        for repository in repositories {
            if let Some(name) = &repository.canonical_name {
                if workspace
                    .local_repositories
                    .get(name)
                    .is_some_and(|root| local_root(repository).as_ref() != Some(root))
                {
                    workspace.local_repositories.remove(name);
                }
            }
        }
        workspace
    }

    fn archive_build_file<'r>(&self, repository: &'r Repository) -> Option<(&'r str, [u8; 32])> {
        use crate::proto::blaze_query::attribute::Discriminator;
        if repository.repo_rule_name.as_deref() != Some("http_archive")
            || repository.repo_rule_bzl_label.as_deref()
                != Some("@@bazel_tools//tools/build_defs/repo:http.bzl")
        {
            return None;
        }
        let label = repository
            .attribute
            .iter()
            .find(|attr| attr.name == "build_file" && attr.r#type == Discriminator::Label)?
            .string_value
            .as_deref()?;
        let (name, relative) = crate::hash::repository_input_path(label).ok()?;
        if name.is_empty() {
            return None;
        }
        let file = self.local_repositories.get(name)?.join(relative);
        if !file.is_file() {
            return None;
        }
        Some((label, Sha256::digest(fs::read(file).ok()?).into()))
    }
}

/// The workspace root as given and as resolved, the two spellings an absolute path in a
/// repository attribute can start with.
fn workspace_roots(workspace: &Path) -> Vec<String> {
    let mut roots = vec![workspace.to_string_lossy().trim_end_matches('/').to_owned()];
    if let Ok(resolved) = fs::canonicalize(workspace) {
        let resolved = resolved.to_string_lossy().trim_end_matches('/').to_owned();
        if !roots.contains(&resolved) {
            roots.push(resolved);
        }
    }
    roots.retain(|root| !root.is_empty());
    roots
}

/// Rewrites a path inside the workspace, bare or as a `file://` URL, to start with
/// `%workspace%`. Anything else keeps its spelling: a path that merely contains the workspace's
/// (`/elsewhere/<workspace>/data`), and one that starts with it but leaves it, through `..` or a
/// symlink. Rewriting those would let two different paths outside two checkouts hash the same.
fn workspace_relative<'a>(value: &'a str, roots: &[String]) -> Cow<'a, str> {
    let (scheme, path) = value
        .strip_prefix("file://")
        .map_or(("", value), |path| ("file://", path));
    for root in roots {
        let rest = path
            .strip_prefix(root.as_str())
            .filter(|rest| rest.is_empty() || rest.starts_with('/'));
        if let Some(rest) = rest {
            if !stays_inside(root, rest) {
                break;
            }
            return Cow::Owned(format!("{scheme}%workspace%{rest}"));
        }
    }
    Cow::Borrowed(value)
}

/// Whether `root` followed by `rest` (empty or starting with `/`) names something inside `root`:
/// its `..` components never climb above `root`, and its deepest existing ancestor, with symlinks
/// resolved, is still under `root`. A path that does not exist yet is judged by its spelling.
fn stays_inside(root: &str, rest: &str) -> bool {
    let mut depth = 0usize;
    let mut normalized = PathBuf::from(root);
    for component in rest
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
    {
        if component == ".." {
            let Some(up) = depth.checked_sub(1) else {
                return false;
            };
            depth = up;
            normalized.pop();
        } else {
            depth += 1;
            normalized.push(component);
        }
    }
    let Ok(root) = fs::canonicalize(root) else {
        return true;
    };
    let existing = normalized.ancestors().find(|ancestor| ancestor.exists());
    existing.is_none_or(|ancestor| {
        fs::canonicalize(ancestor).is_ok_and(|resolved| resolved.starts_with(&root))
    })
}

/// Repository attributes with the workspace root replaced by `%workspace%`. Repository rules
/// that build from a directory in the workspace, such as Gazelle's `go_repository` with
/// `local_path` or a `rules_rs` local crate, get that directory as an absolute path, so the same
/// repository would otherwise hash differently in every checkout.
fn normalize_workspace_attributes(attributes: &mut [Attribute], roots: &[String]) {
    let rewrite = |value: &mut String| {
        if let Cow::Owned(relative) = workspace_relative(value, roots) {
            *value = relative;
        }
    };
    for attribute in attributes {
        attribute.string_value.iter_mut().for_each(rewrite);
        attribute.string_list_value.iter_mut().for_each(rewrite);
        for entry in &mut attribute.string_dict_value {
            rewrite(&mut entry.key);
            rewrite(&mut entry.value);
        }
        for entry in &mut attribute.string_list_dict_value {
            rewrite(&mut entry.key);
            entry.value.iter_mut().for_each(rewrite);
        }
    }
}

fn parse_module_dependency_edges(json: &str) -> BTreeMap<String, BTreeSet<String>> {
    fn walk(value: &serde_json::Value, edges: &mut BTreeMap<String, BTreeSet<String>>) {
        let Some(object) = value.as_object() else {
            return;
        };
        let Some(name) = object.get("name").and_then(serde_json::Value::as_str) else {
            return;
        };
        let Some(dependencies) = object
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
        else {
            return;
        };
        for dependency in dependencies {
            if let Some(dependency_name) =
                dependency.get("name").and_then(serde_json::Value::as_str)
            {
                edges
                    .entry(name.to_owned())
                    .or_default()
                    .insert(dependency_name.to_owned());
            }
            walk(dependency, edges);
        }
    }
    let start = json.find('{').unwrap_or(json.len());
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json[start..]) else {
        return BTreeMap::new();
    };
    let mut edges = BTreeMap::new();
    walk(&value, &mut edges);
    break_module_cycles(edges)
}

/// Every module in the resolved graph whose repository is not in the root's repo mapping, as
/// module key (`name@version`, which `bazel mod show_repo` accepts) -> canonical repository
/// name (which `bazel mod dump_repo_mapping` wants). Bazel names a module's repository
/// `name+`, or `name+version` when `multiple_version_override` keeps several versions of it.
fn hidden_module_repo_names(
    json: &str,
    canonical_to_apparent: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, String> {
    fn walk(value: &serde_json::Value, keys: &mut BTreeMap<String, String>) {
        let Some(object) = value.as_object() else {
            return;
        };
        if let (Some(key), Some(name)) = (
            object.get("key").and_then(serde_json::Value::as_str),
            object.get("name").and_then(serde_json::Value::as_str),
        ) {
            if key != "<root>" {
                keys.insert(key.to_owned(), name.to_owned());
            }
        }
        for dependency in object
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            walk(dependency, keys);
        }
    }
    let start = json.find('{').unwrap_or(json.len());
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json[start..]) else {
        return BTreeMap::new();
    };
    let mut keys = BTreeMap::new();
    walk(&value, &mut keys);
    module_repo_names(&keys, canonical_to_apparent, false)
}

/// Decode the fields JsonOutputFormatter uses for repository enumeration. Repeated and cycle
/// references may be unexpanded, but every module must expand somewhere to reveal its children.
fn isolated_module_repo_names(
    json: &str,
    canonical_to_apparent: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, String>> {
    #[derive(serde::Deserialize)]
    struct Module {
        key: String,
        name: String,
        #[serde(default)]
        dependencies: Option<Vec<Module>>,
        #[serde(default, rename = "indirectDependencies")]
        indirect_dependencies: Vec<Module>,
        #[serde(default)]
        cycles: Vec<Module>,
        #[serde(default)]
        unexpanded: bool,
    }

    fn walk(
        module: &Module,
        keys: &mut BTreeMap<String, String>,
        expanded: &mut BTreeSet<String>,
        root: bool,
    ) -> Result<()> {
        let Module {
            key,
            name,
            dependencies,
            indirect_dependencies,
            cycles,
            unexpanded,
        } = module;
        if root {
            if key != "<root>" {
                bail!("module graph root key is {key}, expected <root>");
            }
            if *unexpanded {
                bail!("module graph root is unexpanded");
            }
        }
        if dependencies.is_none() && !unexpanded {
            bail!("module graph entry {key} has no dependencies array");
        }
        if key != "<root>" {
            if name.is_empty()
                || key
                    .rsplit_once('@')
                    .is_none_or(|(module, version)| module != name || version.is_empty())
            {
                bail!("module graph key {key} does not identify {name}@version");
            }
            keys.insert(key.clone(), name.clone());
            if !unexpanded {
                expanded.insert(key.clone());
            }
        }
        for dependency in dependencies
            .iter()
            .flatten()
            .chain(indirect_dependencies)
            .chain(cycles)
        {
            walk(dependency, keys, expanded, false)?;
        }
        Ok(())
    }

    let start = json.find('{').unwrap_or(json.len());
    let graph: Module =
        serde_json::from_str(&json[start..]).context("invalid module graph JSON")?;
    let mut keys = BTreeMap::new();
    let mut expanded = BTreeSet::new();
    walk(&graph, &mut keys, &mut expanded, true)?;
    for key in keys.keys() {
        if !expanded.contains(key) {
            bail!("module graph entry {key} is only an unexpanded reference");
        }
    }
    Ok(module_repo_names(&keys, canonical_to_apparent, true))
}

/// Required inventories match full canonical names: importing one version does not expose another
/// version's mapping. The tolerant path retains its historical module-name filtering.
fn module_repo_names(
    keys: &BTreeMap<String, String>,
    canonical_to_apparent: &BTreeMap<String, Vec<String>>,
    exact_visibility: bool,
) -> BTreeMap<String, String> {
    let visible_modules = canonical_to_apparent
        .keys()
        .filter(|canonical| canonical.matches('+').count() <= 1)
        .filter_map(|canonical| canonical.split('+').next())
        .collect::<BTreeSet<_>>();
    let mut versions_by_name = BTreeMap::<&str, usize>::new();
    for name in keys.values() {
        *versions_by_name.entry(name.as_str()).or_default() += 1;
    }
    keys.iter()
        .filter_map(|(key, name)| {
            if !exact_visibility && visible_modules.contains(name.as_str()) {
                return None;
            }
            let version = key.rsplit_once('@').map_or("", |(_, version)| version);
            let canonical = if WELL_KNOWN_MODULES.contains(&name.as_str()) {
                name.clone()
            } else if versions_by_name.get(name.as_str()).copied().unwrap_or(0) > 1 {
                format!("{name}+{version}")
            } else {
                format!("{name}+")
            };
            (!exact_visibility || !canonical_to_apparent.contains_key(&canonical))
                .then(|| (key.clone(), canonical))
        })
        .collect()
}

/// Modules Bazel ships with, whose repositories keep their bare name rather than `name+`.
const WELL_KNOWN_MODULES: [&str; 3] = ["bazel_tools", "local_config_platform", "platforms"];

fn break_module_cycles(
    edges: BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<String, BTreeSet<String>> {
    fn visit(
        node: &str,
        edges: &BTreeMap<String, BTreeSet<String>>,
        visited: &mut BTreeSet<String>,
        path: &mut BTreeSet<String>,
        result: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        if visited.contains(node) {
            return;
        }
        path.insert(node.to_owned());
        let mut kept = BTreeSet::new();
        for dependency in edges.get(node).into_iter().flatten() {
            if path.contains(dependency) {
                continue;
            }
            kept.insert(dependency.clone());
            visit(dependency, edges, visited, path, result);
        }
        result.insert(node.to_owned(), kept);
        path.remove(node);
        visited.insert(node.to_owned());
    }
    let mut result = BTreeMap::new();
    let mut visited = BTreeSet::new();
    for node in edges.keys() {
        visit(
            node,
            &edges,
            &mut visited,
            &mut BTreeSet::new(),
            &mut result,
        );
    }
    result
}

pub fn encode_configured_input(input: &ConfiguredRuleInput) -> String {
    match input.configuration_checksum.as_deref() {
        Some(checksum) if !checksum.is_empty() => {
            configured_key(input.label.as_deref().unwrap_or_default(), checksum)
        }
        _ => input.label.clone().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::analysis::{
        AspectDescriptor, Configuration, ConfiguredTarget, CqueryResult, KeyValuePair,
    };
    use crate::proto::blaze_query::{
        attribute, EnvironmentGroup, GeneratedFile, LabelDictUnaryEntry, SourceFile,
    };
    fn lower_repositories(
        repositories: Vec<Repository>,
        canonical_to_apparent: &BTreeMap<String, Vec<String>>,
        modules: ModuleDependencies<'_>,
        extension_edges: Option<&BTreeSet<String>>,
        label_only: &LabelOnlyRepositories,
        workspace: &Path,
        transform: &mut impl FnMut(&mut Target, Option<RepositoryInputs>, Option<Vec<String>>),
    ) -> Vec<Target> {
        repository_rows(
            repositories,
            canonical_to_apparent,
            modules,
            extension_edges,
            label_only,
            workspace,
            |row| row.finish(&BTreeSet::new(), transform),
        )
    }
    #[test]
    fn observing_a_membership_preserves_an_overlapping_module_dependency() {
        let row = RepositoryRow {
            target: repository_target(Rule::default(), "owner", &BTreeSet::from(["both".into()])),
            memberships: vec![Arc::new(RepositoryInputGroup {
                extension: "usage".into(),
                labels: vec!["//external:both".into(), "//external:sibling".into()],
            })],
            generated: false,
            ordinary_complete: false,
        };
        assert_eq!(
            row.inferred(&row.memberships[0]).collect::<Vec<_>>(),
            ["//external:sibling"]
        );
        let target = row.finish(&BTreeSet::from(["usage".into()]), &mut |_, _, _| {});
        assert_eq!(
            target.rule.as_option().unwrap().rule_input,
            ["//external:both"]
        );
    }

    fn rule_target(name: &str) -> Target {
        let mut target = Target::default();
        target.r#type = target::Discriminator::Rule;
        target.rule = buffa::MessageField::some(Rule {
            name: name.to_owned(),
            rule_class: "sh_library".to_owned(),
            ..Default::default()
        });
        target
    }

    fn write_delimited(path: &Path, messages: &[impl Message]) {
        let mut bytes = Vec::new();
        for message in messages {
            message.encode_length_delimited(&mut bytes);
        }
        fs::write(path, bytes).unwrap();
    }

    fn prepared_definition_hash(
        repository: &Repository,
        workspace: &RepositoryWorkspace<'_>,
    ) -> String {
        let mut repository = repository.clone();
        let files = prepare_repository(&mut repository, workspace);
        repository_definition_hash(&repository, &files)
    }

    fn prepared_target(
        repository: &Repository,
        apparent: &str,
        dependencies: &BTreeSet<String>,
        owner: Option<&str>,
        workspace: &RepositoryWorkspace<'_>,
    ) -> Target {
        let mut repository = repository.clone();
        let files = prepare_repository(&mut repository, workspace);
        repository_target(
            repository_rule(repository, files, owner),
            apparent,
            dependencies,
        )
    }

    #[test]
    fn buffa_decodes_delimited_bazel_target() {
        let rule = Rule {
            name: "//foo:bar".into(),
            rule_class: "sh_library".into(),
            ..Default::default()
        };
        let mut target = Target::default();
        target.r#type = target::Discriminator::Rule;
        target.rule = buffa::MessageField::some(rule);
        let path = NamedTempFile::new().unwrap();
        let mut bytes = Vec::new();
        target.encode_length_delimited(&mut bytes);
        fs::write(path.path(), bytes).unwrap();
        let decoded = decode_delimited::<Target>(path.path()).unwrap();
        assert_eq!(target_name(&decoded[0]), Some("//foo:bar"));
    }

    #[test]
    fn target_compaction_retains_hash_inputs() {
        let mut target = Target::default();
        target.r#type = target::Discriminator::Rule;
        target.rule = buffa::MessageField::some(Rule {
            name: "//foo:bar".into(),
            rule_class: "sh_library".into(),
            location: Some("foo/BUILD:1:1".into()),
            attribute: vec![Attribute {
                name: "tags".into(),
                r#type: attribute::Discriminator::StringList,
                string_list_value: vec!["manual".into()],
                ..Default::default()
            }],
            rule_input: vec!["//foo:dep".into()],
            rule_output: vec!["//foo:out".into()],
            default_setting: vec!["unused".into()],
            skylark_environment_hash_code: Some("environment".into()),
            instantiation_stack: vec!["foo/defs.bzl:1:1: macro".into()],
            definition_stack: vec!["unused".into()],
            configured_rule_input: vec![ConfiguredRuleInput {
                label: Some("//foo:dep".into()),
                configuration_checksum: Some("cfg".into()),
                ..Default::default()
            }],
            rule_class_key: Some("unused".into()),
            ..Default::default()
        });

        compact_target(&mut target);

        let rule = target.rule.as_option().unwrap();
        assert_eq!(rule.name, "//foo:bar");
        assert_eq!(rule.rule_class, "sh_library");
        assert_eq!(rule.attribute.len(), 1);
        assert_eq!(rule.rule_input, ["//foo:dep"]);
        assert_eq!(
            rule.skylark_environment_hash_code.as_deref(),
            Some("environment")
        );
        assert_eq!(rule.instantiation_stack, ["foo/defs.bzl:1:1: macro"]);
        assert_eq!(rule.configured_rule_input.len(), 1);
        assert!(rule.location.is_none());
        assert!(rule.rule_output.is_empty());
        assert!(rule.default_setting.is_empty());
        assert!(rule.definition_stack.is_empty());
        assert!(rule.rule_class_key.is_none());
    }

    #[test]
    fn source_compaction_retains_name_and_subincludes() {
        let mut target = Target::default();
        target.r#type = target::Discriminator::SourceFile;
        target.source_file = buffa::MessageField::some(SourceFile {
            name: "//foo:bar.txt".into(),
            location: Some("foo/BUILD:1:1".into()),
            subinclude: vec!["//foo:defs.bzl".into()],
            feature: vec!["unused".into()],
            package_contains_errors: Some(true),
            ..Default::default()
        });

        compact_target(&mut target);

        let source = target.source_file.as_option().unwrap();
        assert_eq!(source.name, "//foo:bar.txt");
        assert_eq!(source.subinclude, ["//foo:defs.bzl"]);
        assert!(source.location.is_none());
        assert!(source.feature.is_empty());
        assert!(source.package_contains_errors.is_none());
    }

    #[test]
    fn configured_input_includes_checksum() {
        let input = ConfiguredRuleInput {
            label: Some("//foo:bar".into()),
            configuration_checksum: Some("abc".into()),
            ..Default::default()
        };
        assert_eq!(encode_configured_input(&input), "//foo:bar|abc");
        assert_eq!(
            encode_configured_input(&ConfiguredRuleInput {
                label: Some("//foo:bar".into()),
                ..Default::default()
            }),
            "//foo:bar"
        );
    }

    #[test]
    fn main_repo_label_key_puts_main_repository_labels_in_one_form() {
        // Bzlmod, WORKSPACE mode and repr() of a main-repository alias.
        for label in ["@@//pkg:name", "@//pkg:name", "//pkg:name"] {
            assert_eq!(main_repo_label_key(label), "//pkg:name", "{label}");
        }
        assert_eq!(main_repo_label_key("@@//:tool"), "//:tool");
        // External repositories keep their name, in either spelling.
        for label in ["@@rules_cc+//cc:defs", "@rules_cc//cc:defs", "@@foo//:bar"] {
            assert_eq!(main_repo_label_key(label), label, "{label}");
        }
    }

    #[test]
    fn target_names_and_lowering_cover_all_target_types() {
        let source = Target {
            r#type: target::Discriminator::SourceFile,
            source_file: buffa::MessageField::some(SourceFile {
                name: "//pkg:file".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(target_name(&source), Some("//pkg:file"));

        let generated = Target {
            r#type: target::Discriminator::GeneratedFile,
            generated_file: buffa::MessageField::some(GeneratedFile {
                name: "//pkg:generated".into(),
                generating_rule: "//pkg:rule".into(),
                location: Some("pkg/BUILD:1:1".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let generated = lower_target(generated).unwrap();
        assert_eq!(target_name(&generated), Some("//pkg:generated"));
        assert!(generated
            .generated_file
            .as_option()
            .unwrap()
            .location
            .is_none());

        let group = Target {
            r#type: target::Discriminator::PackageGroup,
            package_group: buffa::MessageField::some(PackageGroup {
                name: "//pkg:friends".into(),
                contained_package: vec!["//client/...".into()],
                included_package_group: vec!["//pkg:base".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let group = lower_target(group).unwrap();
        let rule = group.rule.as_option().unwrap();
        assert_eq!(rule.rule_class, "package_group");
        assert_eq!(rule.rule_input, ["//pkg:base"]);
        assert_eq!(rule.attribute[0].string_list_value, ["//client/..."]);

        let environment = Target {
            r#type: target::Discriminator::EnvironmentGroup,
            environment_group: buffa::MessageField::some(EnvironmentGroup {
                name: "//pkg:env".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(target_name(&environment), Some("//pkg:env"));
        assert!(lower_target(environment).is_none());

        let missing = Target {
            r#type: target::Discriminator::PackageGroup,
            ..Default::default()
        };
        assert!(lower_target(missing).is_none());
    }

    #[test]
    fn deduplicates_named_targets_and_preserves_first() {
        let first = rule_target("//a:a");
        let mut duplicate = first.clone();
        duplicate.rule.as_option_mut().unwrap().rule_class = "other".into();
        let last = rule_target("//b:b");
        let targets = vec![first.clone(), Target::default(), last.clone(), duplicate];
        assert_eq!(deduplicate_targets(targets).unwrap(), [first, last]);
        assert!(is_external_package_error("no such package 'external'"));
        assert!(is_external_package_error(
            "//external package is not available"
        ));
        assert!(!is_external_package_error("another error"));
    }

    fn test_options(workspace: PathBuf, bazel: PathBuf) -> BazelOptions {
        BazelOptions {
            workspace,
            bazel,
            startup_options: Vec::new(),
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: false,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        }
    }

    #[test]
    fn server_info_reports_version_output_base_and_external_package() {
        let info = |release: &str, semantics: &str| {
            parse_server_info(&format!(
                "release: {release}\noutput_base: /cache/ob\nstarlark-semantics: {semantics}\n"
            ))
            .unwrap()
        };
        let default = "StarlarkSemantics{internal_bazel_only_utf_8_byte_strings=true}";
        let bazel8 = info("release 8.8.2", default);
        assert_eq!(bazel8.version, Some(BazelVersion(8, 8, 2)));
        assert_eq!(bazel8.output_base, Some(PathBuf::from("/cache/ob")));
        assert_eq!(bazel8.external_package, Some(false));
        assert_eq!(
            info("release 8.8.2", "StarlarkSemantics{enable_workspace=true}").external_package,
            Some(true)
        );
        assert_eq!(info("release 7.4.1", default).external_package, Some(true));
        assert_eq!(
            info("release 7.4.1", "StarlarkSemantics{enable_workspace=false}").external_package,
            Some(false)
        );
        assert_eq!(
            info("release 9.0.0rc1", default).version,
            Some(BazelVersion(9, 0, 0))
        );
        let development = info("development version", default);
        assert_eq!(development.version, None);
        assert_eq!(development.external_package, None);
        assert_eq!(parse_server_info("release: release 8.8.2\n"), None);
    }

    #[test]
    fn server_info_leaves_external_package_open_when_the_query_may_set_it() {
        let workspace = tempfile::tempdir().unwrap();
        let script = workspace.path().join("bazel");
        fs::write(
            &script,
            "#!/bin/sh\nprintf 'release: release 8.8.2\\noutput_base: /ob\\nstarlark-semantics: StarlarkSemantics{}\\n'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut options = test_options(workspace.path().to_path_buf(), script);
        options.no_bazelrc = true;
        assert_eq!(options.server_info().unwrap().external_package, Some(false));
        options.command_options = vec!["--enable_workspace".into()];
        assert_eq!(options.server_info().unwrap().external_package, None);
        options.command_options.clear();
        options.no_bazelrc = false;
        fs::write(
            workspace.path().join(".bazelrc"),
            "try-import %workspace%/user.bazelrc\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("user.bazelrc"),
            "query --enable_workspace\n",
        )
        .unwrap();
        assert_eq!(options.server_info().unwrap().external_package, None);
    }

    #[test]
    fn known_missing_external_package_is_not_queried() {
        let options = test_options(PathBuf::new(), PathBuf::new());
        let mut queries = Vec::new();
        options
            .collect_all_targets(Vec::new(), Some(false), |expression, _| {
                queries.push(expression.to_owned());
                Ok(vec![rule_target("//app:lib")])
            })
            .unwrap();
        assert_eq!(queries, ["'//...:all-targets'"]);
    }

    #[test]
    fn decodes_query_and_cquery_streams() {
        let directory = tempfile::tempdir().unwrap();
        let query_path = directory.path().join("query.pb");
        write_delimited(&query_path, &[rule_target("//a:a"), rule_target("//b:b")]);
        let mut seen = Vec::new();
        let targets = decode_target_stream(&query_path, false, &mut |batch, _| {
            seen.extend(
                batch
                    .iter()
                    .map(|target| target_name(target).unwrap().to_owned()),
            );
        })
        .unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(seen, ["//a:a", "//b:b"]);

        let result = CqueryResult {
            results: vec![ConfiguredTarget {
                target: buffa::MessageField::some(rule_target("//c:c")),
                ..Default::default()
            }],
            ..Default::default()
        };
        let streamed = directory.path().join("cquery-stream.pb");
        write_delimited(&streamed, std::slice::from_ref(&result));
        assert_eq!(
            target_name(
                &decode_cquery_stream(&streamed, true, &mut |_| {}, None, None).unwrap()[0]
            ),
            Some("//c:c")
        );

        let non_streamed = directory.path().join("cquery.pb");
        fs::write(&non_streamed, result.encode_to_vec()).unwrap();
        assert_eq!(
            target_name(
                &decode_cquery_stream(&non_streamed, false, &mut |_| {}, None, None).unwrap()[0]
            ),
            Some("//c:c")
        );
    }

    #[test]
    fn read_message_length_parses_varints_and_rejects_bad_prefixes() {
        assert_eq!(read_message_length(&mut [].as_slice()).unwrap(), None);
        assert_eq!(
            read_message_length(&mut [0x05].as_slice()).unwrap(),
            Some(5)
        );
        // 300 encodes as two continuation-flagged bytes.
        assert_eq!(
            read_message_length(&mut [0xAC, 0x02].as_slice()).unwrap(),
            Some(300)
        );
        // Continuation bit set with nothing following.
        assert!(read_message_length(&mut [0x80].as_slice()).is_err());
        // Never-terminating prefix overflows the shift.
        assert!(read_message_length(&mut [0x80; 10].as_slice()).is_err());
        let mut overflow = [0x80; 10];
        overflow[9] = 0x02;
        assert!(read_message_length(&mut overflow.as_slice()).is_err());
        // 2 GiB is one byte over the protobuf limit.
        assert!(read_message_length(&mut [0x80, 0x80, 0x80, 0x80, 0x08].as_slice()).is_err());
    }

    fn lean_attribute(name: &str, value: &str) -> Attribute {
        Attribute {
            name: name.to_owned(),
            r#type: crate::proto::blaze_query::attribute::Discriminator::StringList,
            string_list_value: vec![value.to_owned(), format!("{value}-2")],
            ..Default::default()
        }
    }

    #[test]
    fn lean_decode_leaves_hashed_only_attributes_encoded() {
        let mut full = rule_target("//pkg:gen");
        {
            let rule = full.rule.as_option_mut().unwrap();
            rule.location = Some("/ws/pkg/BUILD:3:8".into());
            rule.rule_input = vec!["//pkg:in".into()];
            rule.skylark_environment_hash_code = Some("env".into());
            rule.attribute = vec![
                lean_attribute("srcs", "a"),
                lean_attribute("tags", "manual"),
                lean_attribute("generator_location", "pkg/BUILD:3"),
                lean_attribute("visibility", "//visibility:public"),
                lean_attribute("cmd", "echo"),
            ];
        }
        let bytes = full.encode_to_vec();

        let (lean, decoded_lean) = decode_lean_target(&bytes, &mut LeanScratch::default()).unwrap();
        let mut encoded = Vec::new();
        encoded_rule_attributes(&bytes, &mut encoded);

        assert!(decoded_lean);
        let attributes = &full.rule.as_option().unwrap().attribute;
        assert_eq!(
            encoded,
            [(0, "srcs"), (2, "generator_location"), (4, "cmd")]
                .map(|(position, name)| {
                    let attribute = attributes.iter().find(|a| a.name == name).unwrap();
                    (position, name, attribute.encode_to_vec())
                })
                .iter()
                .map(|(position, name, bytes)| EncodedAttribute {
                    position: *position,
                    name,
                    bytes,
                })
                .collect::<Vec<_>>()
        );
        let mut expected = full.clone();
        expected
            .rule
            .as_option_mut()
            .unwrap()
            .attribute
            .retain(|a| a.name == "tags" || a.name == "visibility");
        assert_eq!(lean, expected);
    }

    #[test]
    fn lean_decode_keeps_repository_rules_and_other_targets_whole() {
        let mut external = rule_target("//external:repo");
        external.rule.as_option_mut().unwrap().attribute = vec![lean_attribute("urls", "u")];
        let source = Target {
            r#type: target::Discriminator::SourceFile,
            source_file: buffa::MessageField::some(SourceFile {
                name: "//pkg:file.txt".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut toolchain = rule_target("//pkg:toolchain");
        {
            let rule = toolchain.rule.as_option_mut().unwrap();
            rule.rule_class = "toolchain".into();
            rule.attribute = vec![lean_attribute("toolchain", "//pkg:impl")];
        }
        for target in [external, source, toolchain] {
            let bytes = target.encode_to_vec();
            let (lean, decoded_lean) =
                decode_lean_target(&bytes, &mut LeanScratch::default()).unwrap();
            assert_eq!(lean, target);
            // Only the toolchain rule decodes lean, keeping its only attribute decoded.
            assert_eq!(
                decoded_lean,
                target_name(&target) == Some("//pkg:toolchain")
            );
            if decoded_lean {
                let mut encoded = Vec::new();
                encoded_rule_attributes(&bytes, &mut encoded);
                assert!(encoded.is_empty(), "{target:?}");
            }
        }
    }

    #[test]
    fn lean_decode_reports_malformed_targets() {
        let mut target = rule_target("//pkg:gen");
        target.rule.as_option_mut().unwrap().attribute = vec![lean_attribute("srcs", "a")];
        let bytes = target.encode_to_vec();
        for end in [bytes.len() - 1, bytes.len() - 5] {
            assert!(decode_lean_target(&bytes[..end], &mut LeanScratch::default()).is_err());
        }
    }

    #[test]
    fn lean_target_stream_hands_encoded_attributes_per_target() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("query.pb");
        let mut written = (0..DECODE_BATCH_MESSAGES + 3)
            .map(|index| {
                let mut target = rule_target(&format!("//pkg:target{index}"));
                target.rule.as_option_mut().unwrap().attribute =
                    vec![lean_attribute("srcs", &index.to_string())];
                target
            })
            .collect::<Vec<_>>();
        written.push(rule_target("//external:repo"));
        write_delimited(&path, &written);
        let mut seen = Vec::new();
        let decoded = decode_target_stream(&path, true, &mut |batch, encoded| {
            assert_eq!(batch.len(), encoded.len());
            for (target, bytes) in batch.iter().zip(encoded) {
                let mut attributes = Vec::new();
                encoded_rule_attributes(bytes, &mut attributes);
                let names = attributes
                    .iter()
                    .map(|a| a.name.to_owned())
                    .collect::<Vec<_>>();
                seen.push((target_name(target).unwrap().to_owned(), names));
            }
        })
        .unwrap();
        assert_eq!(decoded.len(), written.len());
        assert_eq!(seen.len(), written.len());
        assert_eq!(
            seen[0],
            ("//pkg:target0".to_owned(), vec!["srcs".to_owned()])
        );
        assert_eq!(
            seen.last().unwrap(),
            &("//external:repo".to_owned(), Vec::<String>::new())
        );
        let mut external = Vec::new();
        decode_target_stream(&path, true, &mut |batch, encoded| {
            if let Some(index) = batch
                .iter()
                .position(|target| target_name(target) == Some("//external:repo"))
            {
                external.push(encoded[index].to_vec());
            }
        })
        .unwrap();
        assert_eq!(external, [Vec::<u8>::new()]);
        assert!(decoded[..DECODE_BATCH_MESSAGES + 3]
            .iter()
            .all(|target| target.rule.as_option().unwrap().attribute.is_empty()));
    }

    #[test]
    fn decode_target_stream_preserves_order_across_batches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("query.pb");
        let written = (0..DECODE_BATCH_MESSAGES + 5)
            .map(|index| rule_target(&format!("//pkg:target{index}")))
            .collect::<Vec<_>>();
        write_delimited(&path, &written);
        assert_eq!(decode_delimited::<Target>(&path).unwrap(), written);
        let mut seen = Vec::new();
        let decoded = decode_target_stream(&path, false, &mut |batch, _| {
            seen.extend(
                batch
                    .iter()
                    .map(|target| target_name(target).unwrap().to_owned()),
            );
        })
        .unwrap();
        assert_eq!(decoded.len(), written.len());
        // Order must survive the batch boundary in both the returned targets and
        // the sequence `transform` observes.
        let expected = (0..written.len())
            .map(|index| format!("//pkg:target{index}"))
            .collect::<Vec<_>>();
        assert_eq!(seen, expected);
        assert_eq!(
            decoded
                .iter()
                .map(|target| target_name(target).unwrap().to_owned())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn delimited_repositories_preserve_duplicates_and_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repositories.pb");
        // Alternate duplicates, a different value for an unknown field, and an empty record.
        let bytes = [
            6, 0x0a, 1, b'r', 0xa0, 6, 7, 6, 0x0a, 1, b'r', 0xa0, 6, 8, 0, 6, 0x0a, 1, b'r', 0xa0,
            6, 7,
        ];
        fs::write(&path, bytes).unwrap();

        let mut input = bytes.as_slice();
        let mut expected = Vec::new();
        while !input.is_empty() {
            expected.push(
                buffa::DecodeOptions::new()
                    .decode_length_delimited_reader::<Repository>(&mut input)
                    .unwrap(),
            );
        }
        assert_eq!(expected.len(), 4);
        assert_eq!(decode_delimited::<Repository>(&path).unwrap(), expected);
    }

    #[test]
    fn query_targets_decode_with_their_attribute_count_reserved() {
        let attribute = |name: &str| Attribute {
            name: name.into(),
            r#type: attribute::Discriminator::String,
            string_value: Some("v".into()),
            ..Default::default()
        };
        let mut rule = rule_target("//pkg:rule");
        {
            let inner = rule.rule.as_option_mut().unwrap();
            inner.location = Some("pkg/BUILD:1:1".into());
            inner.attribute = (0..37)
                .map(|index| attribute(&format!("a{index}")))
                .collect();
            inner.rule_input = vec!["//pkg:in".into()];
        }
        let source = Target {
            r#type: target::Discriminator::SourceFile,
            source_file: buffa::MessageField::some(SourceFile {
                name: "//pkg:file.txt".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let empty_rule = rule_target("//pkg:empty");
        for (target, count) in [(&rule, Some(37)), (&source, None), (&empty_rule, Some(0))] {
            let bytes = target.encode_to_vec();
            assert_eq!(rule_attribute_count(&bytes), count);
            let decoded = decode_query_target(&bytes).unwrap();
            assert_eq!(&decoded, target);
            assert_eq!(decoded, Target::decode_from_slice(&bytes).unwrap());
            if let Some(rule) = decoded.rule.as_option() {
                assert_eq!(rule.attribute.capacity(), rule.attribute.len());
            }
        }
        // Malformed input is left for the decoder to reject.
        let bytes = rule.encode_to_vec();
        let truncated = &bytes[..bytes.len() - 3];
        assert_eq!(rule_attribute_count(truncated), None);
        assert!(decode_query_target(truncated).is_err());
        assert_eq!(rule_attribute_count(&[0x0b]), None);
    }

    #[test]
    fn decode_target_stream_rejects_truncated_message() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("truncated.pb");
        // A length prefix of 16 followed by only two bytes of payload.
        fs::write(&path, [0x10u8, 0x00, 0x00]).unwrap();
        assert!(decode_target_stream(&path, false, &mut |_, _| {}).is_err());
        assert!(decode_delimited::<Repository>(&path).is_err());
        // One empty repository followed by a complete frame containing an invalid wire tag.
        fs::write(&path, [0x00, 0x01, 0x00]).unwrap();
        assert!(decode_delimited::<Repository>(&path).is_err());
    }

    #[test]
    fn module_edges_are_parsed_and_cycles_broken() {
        let graph = r#"noise
        {
          "name":"root",
          "dependencies":[
            {"name":"a","dependencies":[{"name":"b","dependencies":[{"name":"a","dependencies":[]}]}]},
            {"name":"c","dependencies":[]}
          ]
        }"#;
        let edges = parse_module_dependency_edges(graph);
        assert_eq!(edges["root"], BTreeSet::from(["a".into(), "c".into()]));
        assert_eq!(edges["a"], BTreeSet::from(["b".into()]));
        assert!(edges["b"].is_empty());
        assert!(edges["c"].is_empty());
        assert!(parse_module_dependency_edges("not json").is_empty());
        assert!(parse_module_dependency_edges(r#"{"dependencies":[]}"#).is_empty());
    }

    #[test]
    fn repository_paths_under_the_workspace_hash_relative_to_it() {
        fn string(name: &str, value: &str) -> Attribute {
            Attribute {
                name: name.into(),
                r#type: attribute::Discriminator::String,
                string_value: Some(value.into()),
                ..Default::default()
            }
        }
        // The same repository in two checkouts: a crate built from `vendored/`, which the rule
        // names by absolute path, plus an absolute path outside the workspace.
        let checkout = || {
            let workspace = tempfile::tempdir().unwrap();
            fs::create_dir_all(workspace.path().join("vendored/src")).unwrap();
            fs::write(workspace.path().join("vendored/src/lib.rs"), "v1").unwrap();
            workspace
        };
        let crate_repository = |workspace: &Path| Repository {
            repo_rule_name: Some("local_crate_repository".into()),
            attribute: vec![
                string("path", &format!("{}/vendored", workspace.display())),
                string("toolchain", "/opt/toolchain"),
            ],
            ..Default::default()
        };
        let first = checkout();
        let second = checkout();
        let hashed_attributes = |workspace: &Path| {
            let target = prepared_target(
                &crate_repository(workspace),
                "dep",
                &BTreeSet::new(),
                None,
                &RepositoryWorkspace::new(workspace),
            );
            target.rule.as_option().unwrap().attribute.clone()
        };
        assert_eq!(
            hashed_attributes(first.path()),
            hashed_attributes(second.path())
        );
        assert_eq!(
            prepared_definition_hash(
                &crate_repository(first.path()),
                &RepositoryWorkspace::new(first.path())
            ),
            prepared_definition_hash(
                &crate_repository(second.path()),
                &RepositoryWorkspace::new(second.path())
            )
        );
        // A path outside the workspace keeps its spelling.
        let mut relative = crate_repository(first.path()).attribute;
        normalize_workspace_attributes(&mut relative, &workspace_roots(first.path()));
        assert_eq!(
            relative[0].string_value.as_deref(),
            Some("%workspace%/vendored")
        );
        assert_eq!(relative[1].string_value.as_deref(), Some("/opt/toolchain"));
    }

    #[test]
    fn only_paths_rooted_at_the_workspace_are_rewritten() {
        let roots = vec!["/checkout/repo".to_owned()];
        for (value, expected) in [
            ("/checkout/repo", "%workspace%"),
            ("/checkout/repo/vendored", "%workspace%/vendored"),
            (
                "file:///checkout/repo/registry/x.patch",
                "file://%workspace%/registry/x.patch",
            ),
            // Paths that only contain the workspace's spelling, or share its prefix.
            (
                "/external/checkout/repo/data",
                "/external/checkout/repo/data",
            ),
            (
                "file:///external/checkout/repo/data",
                "file:///external/checkout/repo/data",
            ),
            ("/checkout/repo-other/data", "/checkout/repo-other/data"),
            ("-L/checkout/repo/lib", "-L/checkout/repo/lib"),
        ] {
            assert_eq!(workspace_relative(value, &roots), expected, "{value}");
        }
        // Two different paths outside two checkouts stay different.
        assert_ne!(
            workspace_relative("/data/a/repo/x", &["/a/repo".to_owned()]),
            workspace_relative("/data/b/repo/x", &["/b/repo".to_owned()])
        );
        // `..` that stays inside is fine; `..` that leaves is not.
        assert_eq!(
            workspace_relative("/checkout/repo/a/../b", &roots),
            "%workspace%/a/../b"
        );
        assert_eq!(
            workspace_relative("/checkout/repo/../data", &roots),
            "/checkout/repo/../data"
        );
        assert_eq!(
            workspace_relative("/checkout/repo/a/../../data", &roots),
            "/checkout/repo/a/../../data"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_workspace_is_not_rewritten() {
        let outside = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("link")).unwrap();
        let roots = workspace_roots(workspace.path());
        let root = &roots[0];
        assert_eq!(
            workspace_relative(&format!("{root}/inside/data"), &roots),
            "%workspace%/inside/data"
        );
        let escaping = format!("{root}/link/data");
        assert_eq!(workspace_relative(&escaping, &roots), escaping);
    }

    #[test]
    fn local_repository_hash_is_stable_and_ignores_lockfile() {
        let workspace = tempfile::tempdir().unwrap();
        let repository_root = workspace.path().join("local");
        fs::create_dir(&repository_root).unwrap();
        fs::write(repository_root.join("a.txt"), "a").unwrap();
        fs::write(repository_root.join("MODULE.bazel.lock"), "one").unwrap();
        let repository = Repository {
            repo_rule_name: Some("local_repository".into()),
            attribute: vec![Attribute {
                name: "path".into(),
                r#type: attribute::Discriminator::String,
                string_value: Some("local".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let first = local_repository_content_hash(&repository, workspace.path()).unwrap();
        fs::write(repository_root.join("MODULE.bazel.lock"), "two").unwrap();
        assert_eq!(
            local_repository_content_hash(&repository, workspace.path()).unwrap(),
            first
        );
        fs::write(repository_root.join("a.txt"), "changed").unwrap();
        assert_ne!(
            local_repository_content_hash(&repository, workspace.path()).unwrap(),
            first
        );

        let target = prepared_target(
            &repository,
            "local",
            &BTreeSet::from(["dep".to_owned(), "local".to_owned()]),
            None,
            &RepositoryWorkspace::new(workspace.path()),
        );
        let rule = target.rule.as_option().unwrap();
        assert_eq!(rule.name, "//external:local");
        assert_eq!(rule.rule_class, "local_repository");
        assert_eq!(rule.rule_input, ["//external:dep"]);
        assert!(rule
            .attribute
            .iter()
            .any(|attribute| attribute.name == "_bazel_diff_content_hash"));

        assert!(local_repository_content_hash(&Repository::default(), workspace.path()).is_none());
    }

    #[test]
    fn archive_build_file_hashes_only_its_named_local_repository_file() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("overlays");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("active"), "first").unwrap();
        fs::write(source.join("neighbor"), "unrelated").unwrap();
        let local = Repository {
            canonical_name: Some("overlays+".into()),
            repo_rule_name: Some("local_repository".into()),
            attribute: vec![Attribute {
                name: "path".into(),
                r#type: attribute::Discriminator::String,
                string_value: Some("overlays".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let workspace =
            RepositoryWorkspace::from_repositories(directory.path(), std::slice::from_ref(&local));
        let mut archive = Repository {
            repo_rule_bzl_label: Some("@@bazel_tools//tools/build_defs/repo:http.bzl".into()),
            repo_rule_name: Some("http_archive".into()),
            attribute: vec![Attribute {
                name: "build_file".into(),
                r#type: attribute::Discriminator::Label,
                string_value: Some("@@overlays+//:active".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let first = repository_local_inputs_hash(&archive, &workspace).unwrap();
        fs::write(source.join("neighbor"), "changed").unwrap();
        assert_eq!(
            repository_local_inputs_hash(&archive, &workspace).unwrap(),
            first
        );
        fs::write(source.join("active"), "second").unwrap();
        assert_ne!(
            repository_local_inputs_hash(&archive, &workspace).unwrap(),
            first
        );
        for conflict in [
            Repository {
                repo_rule_bzl_label: Some("@@other+//:local.bzl".into()),
                ..local.clone()
            },
            Repository {
                attribute: vec![Attribute {
                    name: "path".into(),
                    r#type: attribute::Discriminator::String,
                    string_value: Some("elsewhere".into()),
                    ..Default::default()
                }],
                ..local.clone()
            },
            Repository {
                attribute: vec![Attribute {
                    name: "path".into(),
                    r#type: attribute::Discriminator::Label,
                    string_value: Some("overlays".into()),
                    ..Default::default()
                }],
                ..local.clone()
            },
        ] {
            for records in [
                [local.clone(), conflict.clone()],
                [conflict.clone(), local.clone()],
            ] {
                let ambiguous = RepositoryWorkspace::from_repositories(directory.path(), &records);
                assert!(repository_local_inputs_hash(&archive, &ambiguous).is_none());
            }
        }
        archive.attribute[0].r#type = attribute::Discriminator::String;
        assert!(repository_local_inputs_hash(&archive, &workspace).is_none());
        archive.attribute[0].r#type = attribute::Discriminator::Label;
        archive.repo_rule_bzl_label = Some("@@other+//:http.bzl".into());
        assert!(repository_local_inputs_hash(&archive, &workspace).is_none());
    }

    #[test]
    fn repository_hash_covers_main_repo_files_named_by_attributes() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir_all(workspace.path().join("third_party/overlay")).unwrap();
        fs::write(workspace.path().join("dep.patch"), "patch v1").unwrap();
        fs::write(workspace.path().join("third_party/dep.BUILD"), "build").unwrap();
        fs::write(
            workspace.path().join("third_party/overlay/BUILD.bazel"),
            "overlay",
        )
        .unwrap();
        let repository = Repository {
            repo_rule_name: Some("http_archive".into()),
            attribute: vec![
                Attribute {
                    name: "urls".into(),
                    r#type: attribute::Discriminator::StringList,
                    string_list_value: vec!["https://example.com/dep.tar.gz".into()],
                    ..Default::default()
                },
                Attribute {
                    name: "patches".into(),
                    r#type: attribute::Discriminator::LabelList,
                    string_list_value: vec!["@@//:dep.patch".into(), "//:missing.patch".into()],
                    ..Default::default()
                },
                Attribute {
                    name: "build_file".into(),
                    r#type: attribute::Discriminator::Label,
                    string_value: Some("//third_party:dep.BUILD".into()),
                    ..Default::default()
                },
                Attribute {
                    name: "overlay".into(),
                    r#type: attribute::Discriminator::LabelDictUnary,
                    label_dict_unary_value: vec![LabelDictUnaryEntry {
                        key: "//third_party:overlay".into(),
                        value: "BUILD.bazel".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            repository_local_input_labels(&repository),
            BTreeSet::from([
                "@@//:dep.patch",
                "//:missing.patch",
                "//third_party:dep.BUILD",
                "//third_party:overlay",
            ])
        );
        let baseline =
            repository_local_inputs_hash(&repository, &RepositoryWorkspace::new(workspace.path()))
                .unwrap();
        assert_eq!(
            repository_local_inputs_hash(&repository, &RepositoryWorkspace::new(workspace.path()))
                .unwrap(),
            baseline
        );

        // Patch content, the build file, and the overlay directory all reach the hash.
        fs::write(workspace.path().join("dep.patch"), "patch v2").unwrap();
        let patched =
            repository_local_inputs_hash(&repository, &RepositoryWorkspace::new(workspace.path()))
                .unwrap();
        assert_ne!(patched, baseline);
        fs::write(workspace.path().join("third_party/dep.BUILD"), "build v2").unwrap();
        let rebuilt =
            repository_local_inputs_hash(&repository, &RepositoryWorkspace::new(workspace.path()))
                .unwrap();
        assert_ne!(rebuilt, patched);
        fs::write(
            workspace.path().join("third_party/overlay/BUILD.bazel"),
            "overlay v2",
        )
        .unwrap();
        assert_ne!(
            repository_local_inputs_hash(&repository, &RepositoryWorkspace::new(workspace.path()))
                .unwrap(),
            rebuilt
        );

        let target = prepared_target(
            &repository,
            "dep",
            &BTreeSet::new(),
            None,
            &RepositoryWorkspace::new(workspace.path()),
        );
        let rule = target.rule.as_option().unwrap();
        assert!(rule
            .attribute
            .iter()
            .any(|attribute| attribute.name == "_bazel_diff_local_inputs_hash"));

        // A repository whose attributes name no workspace file carries no such attribute.
        let remote_only = Repository {
            repo_rule_name: Some("http_archive".into()),
            attribute: vec![Attribute {
                name: "patches".into(),
                r#type: attribute::Discriminator::LabelList,
                string_list_value: vec!["@other//:dep.patch".into(), "//:missing.patch".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(repository_local_inputs_hash(
            &remote_only,
            &RepositoryWorkspace::new(workspace.path())
        )
        .is_none());
        let target = prepared_target(
            &remote_only,
            "dep",
            &BTreeSet::new(),
            None,
            &RepositoryWorkspace::new(workspace.path()),
        );
        assert!(!target
            .rule
            .as_option()
            .unwrap()
            .attribute
            .iter()
            .any(|attribute| attribute.name == "_bazel_diff_local_inputs_hash"));
    }

    #[test]
    fn parses_versions_and_exclude_filters() {
        assert_eq!(
            parse_bazel_version("Build label: 8.6.1-pre.2026\n"),
            Some(BazelVersion(8, 6, 1))
        );
        assert_eq!(
            parse_bazel_version("bazel 7.0.0\n"),
            Some(BazelVersion(7, 0, 0))
        );
        assert_eq!(parse_bazel_version("unexpected"), None);

        let mut options = BazelOptions {
            workspace: PathBuf::new(),
            bazel: PathBuf::new(),
            startup_options: Vec::new(),
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: false,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        };
        options.exclude_targets_query = Some("attr(tags, manual, //...)".into());
        assert_eq!(
            options.with_exclude_filter("//...".into()),
            "(//...) except (attr(tags, manual, //...))"
        );
        options.exclude_targets_query = Some(" ".into());
        assert_eq!(options.with_exclude_filter("//...".into()), "//...");
    }

    #[test]
    fn query_collection_retries_without_external_and_deduplicates() {
        let options = BazelOptions {
            workspace: PathBuf::new(),
            bazel: PathBuf::new(),
            startup_options: Vec::new(),
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::from(["@repo".to_owned()]),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: false,
            exclude_targets_query: Some("attr(tags, manual, //...)".into()),
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        };
        let mut queries = Vec::new();
        let targets = options
            .collect_all_targets(
                vec![rule_target("//external:bzlmod")],
                None,
                |expression, cquery| {
                    queries.push((expression.to_owned(), cquery));
                    if queries.len() == 1 {
                        Err(anyhow!("no such package 'external'"))
                    } else {
                        Ok(vec![rule_target("//app:lib"), rule_target("//app:lib")])
                    }
                },
            )
            .unwrap();
        assert_eq!(queries.len(), 2);
        assert!(queries[0].0.contains("//external:all-targets"));
        assert!(!queries[1].0.contains("//external:all-targets"));
        assert!(queries[1].0.contains("@repo//...:all-targets"));
        assert!(queries[1].0.contains("except (attr(tags, manual, //...))"));
        assert_eq!(
            targets.iter().filter_map(target_name).collect::<Vec<_>>(),
            ["//app:lib", "//external:bzlmod"]
        );
    }

    #[test]
    fn cquery_collection_drops_missing_external_package() {
        let options = BazelOptions {
            workspace: PathBuf::new(),
            bazel: PathBuf::new(),
            startup_options: Vec::new(),
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: true,
            cquery_expression: Some("deps(//app:lib)".into()),
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: false,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        };
        let mut queries = Vec::new();
        let targets = options
            .collect_all_targets(Vec::new(), None, |expression, cquery| {
                queries.push((expression.to_owned(), cquery));
                if cquery {
                    Ok(vec![rule_target("//app:lib")])
                } else {
                    Err(anyhow!("//external package is not available"))
                }
            })
            .unwrap();
        assert_eq!(
            queries,
            vec![
                ("deps(//app:lib)".to_owned(), true),
                ("'//external:all-targets'".to_owned(), false),
            ]
        );
        assert_eq!(target_name(&targets[0]), Some("//app:lib"));
    }

    #[test]
    fn repository_mapping_and_lowering_are_deterministic() {
        let mapping = parse_repo_mapping(
            r#"not json
{"dep":"dep+1.0","alias":"dep+1.0","child":"child+2.0","tools":"bazel_tools","local":"local_config_platform","":""}
"#,
        );
        assert_eq!(mapping["dep+1.0"], ["alias".to_owned(), "dep".to_owned()]);
        assert_eq!(mapping["child+2.0"], ["child"]);
        assert!(!mapping.contains_key("bazel_tools"));

        let repositories = vec![
            Repository {
                canonical_name: Some("dep+1.0".into()),
                repo_rule_name: Some("http_archive".into()),
                ..Default::default()
            },
            Repository {
                canonical_name: Some("child+2.0".into()),
                repo_rule_name: Some("git_repository".into()),
                ..Default::default()
            },
        ];
        let edges = BTreeMap::from([("dep".to_owned(), BTreeSet::from(["child".to_owned()]))]);
        let mut transformed = Vec::new();
        let targets = lower_repositories(
            repositories,
            &mapping,
            ModuleDependencies {
                edges: &edges,
                mappings: &BTreeMap::new(),
            },
            None,
            &LabelOnlyRepositories::default(),
            Path::new("."),
            &mut |target, _inputs, _ordinary_inputs| {
                transformed.push(target_name(target).unwrap().to_owned());
            },
        );
        assert_eq!(targets.len(), 3);
        assert_eq!(
            transformed,
            ["//external:alias", "//external:dep", "//external:child"]
        );
        let dep = targets
            .iter()
            .find(|target| target_name(target) == Some("//external:dep"))
            .unwrap()
            .rule
            .as_option()
            .unwrap();
        assert_eq!(dep.rule_class, "http_archive");
        assert_eq!(dep.rule_input, ["//external:child"]);
    }

    #[test]
    fn apparent_names_share_one_read_of_the_repository_inputs() {
        let workspace = tempfile::tempdir().unwrap();
        let patch = workspace.path().join("dep.patch");
        fs::write(&patch, "first").unwrap();
        let patched = |canonical: &str| Repository {
            canonical_name: Some(canonical.into()),
            attribute: vec![Attribute {
                name: "patches".into(),
                r#type: attribute::Discriminator::LabelList,
                string_list_value: vec!["//:dep.patch".into()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let first = repository_local_inputs_hash(
            &patched("dep+"),
            &RepositoryWorkspace::new(workspace.path()),
        )
        .unwrap();
        let targets = lower_repositories(
            vec![
                Repository {
                    canonical_name: Some("before+".into()),
                    ..Default::default()
                },
                patched("dep+"),
                patched("dep++ext+spoke"),
            ],
            &parse_repo_mapping(r#"{"before":"before+","a":"dep+","b":"dep+"}"#),
            ModuleDependencies {
                edges: &BTreeMap::new(),
                mappings: &BTreeMap::new(),
            },
            Some(&BTreeSet::new()),
            &LabelOnlyRepositories::default(),
            workspace.path(),
            &mut |target, _inputs, _ordinary_inputs| {
                // The module definitions have been hashed, but dep's targets have not been emitted.
                if target_name(target) == Some("//external:before") {
                    fs::write(&patch, "second").unwrap();
                }
            },
        );
        let second = repository_local_inputs_hash(
            &patched("dep+"),
            &RepositoryWorkspace::new(workspace.path()),
        )
        .unwrap();
        assert_ne!(first, second);
        let observed: Vec<_> = targets
            .iter()
            .map(|target| {
                let digest = target
                    .rule
                    .as_option()
                    .unwrap()
                    .attribute
                    .iter()
                    .find(|attr| attr.name == "_bazel_diff_local_inputs_hash")
                    .and_then(|attr| attr.string_value.as_deref());
                (target_name(target).unwrap(), digest)
            })
            .collect();
        let first = Some(first.as_str());
        assert_eq!(
            observed,
            [
                ("//external:before", None),
                ("//external:a", first),
                ("//external:b", first),
                ("//external:dep++ext+spoke", Some(second.as_str())),
            ]
        );
    }

    /// Issue #522: a module pinned with `single_version_override(patches = ["//:dep.patch"])`
    /// is shown by `bazel mod show_repo` as an `http_archive` whose `patches` name a main-repo
    /// file. Its node is `//external:dep`, named by the root's apparent name and not by the
    /// canonical `dep+` cquery prints, and the patch content is part of the node's hash even
    /// though the patch touches no `.bzl` file.
    #[test]
    fn single_version_override_patch_is_hashed_into_the_apparent_repository_node() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("dep.patch"),
            "--- a/data.txt\n+++ b/data.txt\n@@ -1 +1 @@\n-original\n+patched v1\n",
        )
        .unwrap();
        let mapping = parse_repo_mapping(r#"{"":"","dep":"dep+","bazel_tools":"bazel_tools"}"#);
        let repository = || Repository {
            canonical_name: Some("dep+".into()),
            repo_rule_name: Some("http_archive".into()),
            attribute: vec![
                Attribute {
                    name: "urls".into(),
                    r#type: attribute::Discriminator::StringList,
                    string_list_value: vec!["file:/registry/dep.zip".into()],
                    ..Default::default()
                },
                Attribute {
                    name: "patches".into(),
                    r#type: attribute::Discriminator::LabelList,
                    string_list_value: vec!["//:dep.patch".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let lower = || {
            let targets = lower_repositories(
                vec![repository()],
                &mapping,
                ModuleDependencies {
                    edges: &BTreeMap::new(),
                    mappings: &BTreeMap::new(),
                },
                None,
                &LabelOnlyRepositories::default(),
                workspace.path(),
                &mut materialize_repository_inputs,
            );
            assert_eq!(targets.len(), 1);
            let rule = targets[0].rule.as_option().unwrap().clone();
            assert_eq!(rule.name, "//external:dep");
            rule.attribute
                .iter()
                .find(|attribute| attribute.name == "_bazel_diff_local_inputs_hash")
                .and_then(|attribute| attribute.string_value.clone())
                .expect("patch hashed into the node")
        };
        let before = lower();
        fs::write(
            workspace.path().join("dep.patch"),
            "--- a/data.txt\n+++ b/data.txt\n@@ -1 +1 @@\n-original\n+patched v2\n",
        )
        .unwrap();
        assert_ne!(lower(), before);
    }

    #[test]
    fn modules_hidden_from_the_root_stay_on_the_repository_path() {
        // Issue #510: root -> facade -> middle (apparent name only inside facade) -> leaf.
        let mapping = parse_repo_mapping(
            r#"{"":"","facade":"facade+","leaf":"leaf+","ext_repo":"+ext+repo","bazel_tools":"bazel_tools"}"#,
        );
        let graph = r#"{
          "key":"<root>","name":"root","dependencies":[
            {"key":"facade@_","name":"facade","dependencies":[
              {"key":"middle@_","name":"middle","dependencies":[
                {"key":"leaf@_","name":"leaf","dependencies":[]}]}]},
            {"key":"leaf@_","name":"leaf","dependencies":[]}
          ]
        }"#;
        assert_eq!(
            hidden_module_repo_names(graph, &mapping),
            BTreeMap::from([("middle@_".to_owned(), "middle+".to_owned())])
        );
        assert!(hidden_module_repo_names("not json", &mapping).is_empty());

        let repositories = ["facade+", "middle+", "leaf+"]
            .into_iter()
            .map(|canonical| Repository {
                canonical_name: Some(canonical.into()),
                repo_rule_name: Some("http_archive".into()),
                ..Default::default()
            })
            .collect();
        let targets = lower_repositories(
            repositories,
            &mapping,
            ModuleDependencies {
                edges: &parse_module_dependency_edges(graph),
                mappings: &BTreeMap::new(),
            },
            None,
            &LabelOnlyRepositories::default(),
            Path::new("."),
            &mut materialize_repository_inputs,
        );
        let inputs = |name: &str| {
            targets
                .iter()
                .find(|target| target_name(target) == Some(name))
                .unwrap()
                .rule
                .as_option()
                .unwrap()
                .rule_input
                .clone()
        };
        assert_eq!(inputs("//external:facade"), ["//external:middle+"]);
        assert_eq!(inputs("//external:middle+"), ["//external:leaf"]);
        assert!(inputs("//external:leaf").is_empty());
    }

    #[test]
    fn extension_repo_names_identify_usage_and_owner() {
        assert!(is_extension_repo("rules_go++go_sdk+go_default_sdk"));
        assert!(is_extension_repo("platforms+host_platform+host_platform"));
        assert!(is_extension_repo("+ext+repo"));
        assert!(!is_extension_repo("rules_go+"));
        assert!(!is_extension_repo("dep+1.0"));
        assert!(!is_extension_repo("platforms"));
        let cases = [
            (
                "rules_go++go_sdk+go_default_sdk",
                "rules_go+",
                "rules_go++go_sdk",
            ),
            ("dep+1.0+ext+repo", "dep+1.0", "dep+1.0+ext"),
            (
                "platforms+host_platform+host_platform",
                "platforms",
                "platforms+host_platform",
            ),
            ("+ext+repo", "", "+ext"),
        ];
        let module_definitions = cases
            .iter()
            .map(|(_canonical, owner, _prefix)| ((*owner).to_owned(), String::new()))
            .collect();
        for (canonical, owner, prefix) in cases {
            assert_eq!(extension_usage_prefix(canonical), Some(prefix));
            assert_eq!(
                extension_repo_owner(prefix, &module_definitions)
                    .map(|(owner, _definition_hash)| owner.as_str()),
                Some(owner)
            );
        }
        assert_eq!(extension_usage_prefix("rules_go+"), None);
    }

    #[test]
    fn arguments_are_chunked_by_byte_budget() {
        let names = ["aaaa", "bbbb", "cccc", "dddd", "e"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(
            chunk_arguments(&names, 10),
            [&names[0..2], &names[2..4], &names[4..]]
        );
        // One oversized argument still travels, on its own.
        assert_eq!(chunk_arguments(&names, 3).len(), 5);
        assert_eq!(chunk_arguments(&names, 1000), [&names[..]]);
        assert!(chunk_arguments(&[], 10).is_empty());
    }

    #[test]
    fn argument_budget_defaults_unless_the_environment_sets_a_positive_count() {
        assert_eq!(
            mod_argument_byte_budget_from(None),
            DEFAULT_MOD_ARGUMENT_BYTE_BUDGET
        );
        assert_eq!(mod_argument_byte_budget_from(Some(" 4096 ")), 4096);
        for junk in ["", "0", "-1", "lots", "6K"] {
            assert_eq!(
                mod_argument_byte_budget_from(Some(junk)),
                DEFAULT_MOD_ARGUMENT_BYTE_BUDGET,
                "{junk:?}"
            );
        }
    }

    /// A fake Bazel for `show_repo`: by name it prints `<dir>/<name>.pb` for each name
    /// given; `--all_repos` prints `<dir>/all.pb`, or fails with `<dir>/all-repos.stderr`
    /// when that file exists. Every invocation is appended to `<dir>/calls.log`.
    #[cfg(unix)]
    fn show_repo_script(dir: &Path, by_name: &[Repository], all: &[Repository]) -> PathBuf {
        for repository in by_name {
            let name = repository
                .module_key
                .clone()
                .unwrap_or_else(|| format!("@@{}", repository.canonical_name.as_deref().unwrap()));
            write_delimited(
                &dir.join(format!("{name}.pb")),
                std::slice::from_ref(repository),
            );
        }
        write_delimited(&dir.join("all.pb"), all);
        let script = dir.join("fake-bazel.sh");
        let body = format!(
            r#"#!/bin/sh
echo "$*" >> "{dir}/calls.log"
if [ "$1" = "mod" ] && [ "$2" = "show_repo" ]; then
  shift 2
  if [ "$1" = "--all_repos" ]; then
    if [ -f "{dir}/all-repos.stderr" ]; then
      cat "{dir}/all-repos.stderr" >&2
      exit 2
    fi
    cat "{dir}/all.pb"
    exit 0
  fi
  for name in "$@"; do
    case "$name" in
      --*) ;;
      *) cat "{dir}/$name.pb" ;;
    esac
  done
  exit 0
fi
exit 1
"#,
            dir = dir.display(),
        );
        fs::write(&script, &body).unwrap();
        script
    }

    #[cfg(unix)]
    fn canonical_names(repositories: &[Repository]) -> Vec<&str> {
        repositories
            .iter()
            .filter_map(|repository| repository.canonical_name.as_deref())
            .collect()
    }

    #[cfg(unix)]
    fn show_repo_calls(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[cfg(unix)]
    fn repository(canonical: &str) -> Repository {
        Repository {
            canonical_name: Some(canonical.into()),
            repo_rule_name: Some("http_archive".into()),
            ..Default::default()
        }
    }

    #[test]
    #[cfg(unix)]
    fn large_repository_reads_preserve_selected_records() {
        let workspace = tempfile::tempdir().unwrap();
        let wanted = ["a+", "b+", "c++ext+x"].map(repository);
        let mut bulk_hidden = repository("hidden+");
        bulk_hidden.original_name = Some("upstream_name".into());
        let mut hidden = bulk_hidden.clone();
        hidden.module_key = Some("hidden@1.0".to_owned());
        let mut by_name = wanted.to_vec();
        by_name.push(hidden.clone());
        // `--all_repos` answers with more than was asked: the main-visible `bazel_tools` and
        // a repository nothing imports. Its hidden module record has no module key.
        let mut all = vec![
            repository("bazel_tools"),
            repository("unasked++ext+y"),
            repository("failed++ext+x"),
            bulk_hidden,
        ];
        all.extend(wanted.iter().rev().cloned());
        let script = show_repo_script(workspace.path(), &by_name, &all);
        // RepoOverride produces no streamed proto record in either mode.
        write_delimited(
            &workspace.path().join("@@omitted+.pb"),
            &[] as &[Repository],
        );
        let options = BazelOptions {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("/bin/sh"),
            startup_options: vec![script.to_str().unwrap().to_owned()],
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: true,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        };
        let names = [
            "@@a+",
            "@@b+",
            "@@c++ext+x",
            "@@failed++ext+x",
            "@@omitted+",
            "hidden@1.0",
        ]
        .map(str::to_owned)
        .to_vec();
        let expected = wanted
            .into_iter()
            .chain(std::iter::once(hidden))
            .collect::<Vec<_>>();
        let original_failed = FailedExtensions {
            observed: true,
            extensions: BTreeSet::from(["@@failed+//:ext.bzl%ext".to_owned()]),
            unavailable: BTreeSet::from(["failed++ext+x".to_owned()]),
        };
        let mut failed = original_failed.clone();
        let bindings = BTreeMap::from([("hidden@1.0".into(), "hidden+".into())]);

        // Canonical names use one bulk invocation; the hidden module retains its explicit record.
        let repositories = options
            .show_repositories_within(&names, &mut failed, 8, || {
                panic!("partial bulk output must not inspect hidden bindings")
            })
            .unwrap();
        assert_eq!(repositories, expected);
        let calls = show_repo_calls(workspace.path());
        assert_eq!(
            calls,
            [
                "mod show_repo --all_repos --output=streamed_proto",
                "mod show_repo hidden@1.0 --output=streamed_proto",
            ],
            "{calls:?}"
        );
        assert_eq!(failed, original_failed);

        // Names that fit one invocation are still asked for by name.
        let repositories = options
            .show_repositories_within(&names, &mut failed, 1000, || {
                panic!("small selections must not inspect hidden bindings")
            })
            .unwrap();
        assert_eq!(repositories, expected);
        let calls = show_repo_calls(workspace.path());
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(
            calls[2],
            "mod show_repo @@a+ @@b+ @@c++ext+x @@omitted+ hidden@1.0 --output=streamed_proto"
        );

        // A healthy bulk read can supply the same module-key record, byte for byte.
        let clean: Vec<_> = names
            .into_iter()
            .filter(|name| !name.starts_with("@@failed+"))
            .collect();
        let actual = options
            .show_repositories_within(&clean, &mut FailedExtensions::default(), 8, || {
                Some(&bindings)
            })
            .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(
            &show_repo_calls(workspace.path())[3..],
            ["mod show_repo --all_repos --output=streamed_proto"]
        );
    }

    #[test]
    fn hidden_module_keys_follow_each_parents_actual_alias() {
        let graph = r#"{"dependencies":[{"key":"visible@1","apparentName":"alias","dependencies":[{"key":"hidden@2","apparentName":"other_alias"}]},{"key":"hidden@2","apparentName":"sibling"}]}"#;
        let hidden = BTreeMap::from([("hidden@2".into(), "hidden+".into())]);
        let modules = BTreeMap::from([(
            "visible+".into(),
            BTreeMap::from([("other_alias".into(), "hidden+".into())]),
        )]);
        assert!(verified_hidden_modules(
            Some(graph),
            br#"{"alias":"visible+","sibling":"hidden+"}"#,
            &modules,
            &hidden
        ));
        assert!(!verified_hidden_modules(
            Some(graph),
            br#"{"alias":"visible+","sibling":"different+"}"#,
            &modules,
            &hidden
        ));
        assert!(!verified_hidden_modules(
            Some(graph),
            br#"{"alias":"visible+"}"#,
            &modules,
            &hidden
        ));
    }

    #[test]
    #[cfg(unix)]
    fn incomplete_mapping_rows_remain_unbound() {
        let workspace = tempfile::tempdir().unwrap();
        let script = workspace.path().join("bazel");
        fs::write(&script, "#!/bin/sh\ncat reply\n").unwrap();
        let options = probe_options(workspace.path(), script);
        let names = ["first+", "second+"].map(str::to_owned);
        for (reply, rows, expected_complete) in [
            ("{}\n{}\n", 2, true),
            ("{}\n", 1, false),
            ("{}\ninvalid\n{}\n", 2, false),
            ("{}\n{}\n{}\n", 3, false),
        ] {
            fs::write(workspace.path().join("reply"), reply).unwrap();
            let mut complete = true;
            let mappings = options
                .dump_repo_mappings(&names, &mut FailedExtensions::default(), &mut complete)
                .unwrap();
            assert_eq!(mappings.len(), rows);
            assert_eq!(complete, expected_complete);
        }
    }

    #[test]
    #[cfg(unix)]
    fn mappings_preserve_effective_names_in_each_isolated_extension() {
        let workspace = tempfile::tempdir().unwrap();
        let first = "mod++_ext+owner+1+first+hub";
        let second = "mod++_ext+owner+1+second+hub";
        let library = "mod++_ext+owner+1+first+library";
        let injected = "mod++_ext+owner+1+second+injected";
        for (name, map) in [
            ("dep+", serde_json::json!({"one": first, "two": second})),
            (
                first,
                serde_json::json!({"library": library, "replaced": "other+", "injected": injected}),
            ),
            (
                second,
                serde_json::json!({"other": "unimported++ext+unused"}),
            ),
        ] {
            fs::write(workspace.path().join(name), format!("{map}\n")).unwrap();
        }
        let script = workspace.path().join("bazel");
        fs::write(
            &script,
            "#!/bin/sh\nshift 2\necho \"$*\" >> calls\nfor name do cat \"$name\" || exit 1; done\n",
        )
        .unwrap();
        let options = probe_options(workspace.path(), script);
        let root = BTreeMap::from([
            ("dep+".into(), vec!["dep".into()]),
            (first.into(), vec!["hub".into()]),
        ]);
        let result = options
            .extension_repositories(
                &root,
                Vec::new(),
                BTreeMap::new(),
                true,
                &mut FailedExtensions::default(),
            )
            .unwrap();
        assert_eq!(result.imported, [first, second].map(str::to_owned).into());
        assert_eq!(
            result.generated,
            [first, second, library, injected].map(str::to_owned).into()
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("calls")).unwrap(),
            format!("dep+\n{first} {second}\n")
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_bazel_without_all_repos_is_asked_by_name_per_chunk() {
        let workspace = tempfile::tempdir().unwrap();
        let mut wanted = ["a+", "b+", "c++ext+x"].map(repository).to_vec();
        let mut hidden = repository("hidden+");
        hidden.module_key = Some("hidden@1.0".to_owned());
        wanted.push(hidden);
        let script = show_repo_script(workspace.path(), &wanted, &[]);
        fs::write(
            workspace.path().join("all-repos.stderr"),
            "ERROR: --all_repos :: Unrecognized option: --all_repos\n",
        )
        .unwrap();
        let options = BazelOptions {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("/bin/sh"),
            startup_options: vec![script.to_str().unwrap().to_owned()],
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: true,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        };
        let names = ["@@a+", "@@b+", "@@c++ext+x", "hidden@1.0"]
            .map(str::to_owned)
            .to_vec();
        let mut failed = FailedExtensions::default();
        let repositories = options
            .show_repositories_within(&names, &mut failed, 8, || None)
            .unwrap();
        assert_eq!(repositories, wanted);
        assert_eq!(
            show_repo_calls(workspace.path()),
            [
                "mod show_repo --all_repos --output=streamed_proto",
                "mod show_repo @@a+ --output=streamed_proto",
                "mod show_repo @@b+ --output=streamed_proto",
                "mod show_repo @@c++ext+x --output=streamed_proto",
                "mod show_repo hidden@1.0 --output=streamed_proto",
            ]
        );
        assert_eq!(failed, FailedExtensions::default());
    }

    #[test]
    #[cfg(unix)]
    fn an_extension_that_spoils_all_repos_is_checked_by_name() {
        let workspace = tempfile::tempdir().unwrap();
        let wanted = ["a+", "b+", "c++ext+x"].map(repository);
        let script = show_repo_script(workspace.path(), &wanted, &[]);
        fs::write(
            workspace.path().join("all-repos.stderr"),
            "ERROR: error evaluating module extension @@c+//:ext.bzl%ext\n",
        )
        .unwrap();
        let options = BazelOptions {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("/bin/sh"),
            startup_options: vec![script.to_str().unwrap().to_owned()],
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: true,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        };
        let names = ["@@a+", "@@b+", "@@c++ext+x"].map(str::to_owned).to_vec();
        let mut failed = FailedExtensions::default();
        let repositories = options
            .show_repositories_within(&names, &mut failed, 8, || None)
            .unwrap();
        assert_eq!(canonical_names(&repositories), ["a+", "b+", "c++ext+x"]);
        assert_eq!(
            failed.extensions,
            BTreeSet::from(["@@c+//:ext.bzl%ext".to_owned()])
        );
        assert!(failed.unavailable.is_empty());
        assert_eq!(
            show_repo_calls(workspace.path()),
            [
                "mod show_repo --all_repos --output=streamed_proto",
                "mod show_repo @@a+ --output=streamed_proto",
                "mod show_repo @@b+ --output=streamed_proto",
                "mod show_repo @@c++ext+x --output=streamed_proto",
            ]
        );
    }

    #[test]
    fn hidden_module_repo_names_follow_bazels_naming() {
        let mapping = parse_repo_mapping(r#"{"":"","facade":"facade+","platforms":"platforms"}"#);
        let graph = r#"{
          "key":"<root>","name":"root","dependencies":[
            {"key":"facade@_","name":"facade","dependencies":[
              {"key":"multi@1.0","name":"multi","dependencies":[]},
              {"key":"multi@2.0","name":"multi","dependencies":[]},
              {"key":"platforms@1.1.0","name":"platforms","dependencies":[]},
              {"key":"bazel_tools@_","name":"bazel_tools","dependencies":[]},
              {"key":"leaf@1.0","name":"leaf","dependencies":[],"cycles":[
                {"key":"facade@_","name":"facade","unexpanded":true}]}]},
            {"key":"leaf@1.0","name":"leaf","unexpanded":true}
          ],
          "indirectDependencies":[{"key":"leaf@1.0","name":"leaf","unexpanded":true}]
        }"#;
        let expected = BTreeMap::from([
            ("bazel_tools@_".to_owned(), "bazel_tools".to_owned()),
            ("leaf@1.0".to_owned(), "leaf+".to_owned()),
            ("multi@1.0".to_owned(), "multi+1.0".to_owned()),
            ("multi@2.0".to_owned(), "multi+2.0".to_owned()),
        ]);
        assert_eq!(hidden_module_repo_names(graph, &mapping), expected);
        assert_eq!(
            isolated_module_repo_names(graph, &mapping).unwrap(),
            expected
        );

        let mut one_version = mapping;
        one_version.insert("multi+1.0".into(), vec!["multi".into()]);
        let legacy = hidden_module_repo_names(graph, &one_version);
        assert_eq!(legacy.get("multi@1.0"), None);
        assert_eq!(legacy.get("multi@2.0"), None);
        let isolated = isolated_module_repo_names(graph, &one_version).unwrap();
        assert_eq!(isolated.get("multi@1.0"), None);
        assert_eq!(
            isolated.get("multi@2.0").map(String::as_str),
            Some("multi+2.0")
        );
    }

    #[test]
    fn required_module_inventory_rejects_unreadable_and_incomplete_graphs() {
        let mapping = BTreeMap::new();
        for (graph, reason) in [
            ("not json", "invalid module graph JSON"),
            (
                r#"{"name":"root","dependencies":[]}"#,
                "missing field `key`",
            ),
            (
                r#"{"key":"other@1","name":"other","dependencies":[]}"#,
                "module graph root key is other@1, expected <root>",
            ),
            (
                r#"{"key":"<root>","name":"root","unexpanded":true}"#,
                "module graph root is unexpanded",
            ),
            (
                r#"{"key":"<root>","name":"root","dependencies":[{"key":"dep@1","dependencies":[]}]}"#,
                "missing field `name`",
            ),
            (
                r#"{"key":"<root>","name":"root","dependencies":{}}"#,
                "expected a sequence",
            ),
            (
                r#"{"key":"<root>","name":"root","dependencies":[{"key":"dep@1","name":"dep","unexpanded":true}]}"#,
                "dep@1 is only an unexpanded reference",
            ),
            (
                r#"{"key":"<root>","name":"root","dependencies":[{"key":"wrong@1","name":"dep","dependencies":[]}]}"#,
                "key wrong@1 does not identify dep@version",
            ),
        ] {
            let error = isolated_module_repo_names(graph, &mapping).unwrap_err();
            assert!(format!("{error:#}").contains(reason), "{error:#}");
        }
        assert!(isolated_module_repo_names(
            r#"{"key":"<root>","name":"","dependencies":[]}"#,
            &mapping
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn isolated_module_graph_requires_utf8() {
        use std::os::unix::process::ExitStatusExt;

        let workspace = tempfile::tempdir().unwrap();
        let mut options = probe_options(workspace.path(), PathBuf::from("unused"));
        let output = || Output {
            status: ExitStatus::from_raw(0),
            stdout: b"{\"key\":\"<root>\",\"name\":\"\xff\",\"dependencies\":[]}".to_vec(),
            stderr: Vec::new(),
        };
        options.isolated_dependency_root = Some("@hub".into());
        let error = options
            .module_graph_json_tolerating(Some(output()), &mut FailedExtensions::default())
            .unwrap_err();
        assert!(error.to_string().contains("not valid UTF-8"), "{error:#}");
        options.isolated_dependency_root = None;
        let (json, complete) = options
            .module_graph_json_tolerating(Some(output()), &mut FailedExtensions::default())
            .unwrap();
        assert!(complete);
        assert!(json.contains('\u{fffd}'));
    }

    #[test]
    fn extension_repos_hang_off_their_module_and_hub_in_query_mode_only() {
        // Root imports `rules_go` and the `bd_crates` hub of rules_rs' crate extension; rules_go's
        // own module imports `go_toolchains`; nobody imports `go_default_sdk` or the crate spoke.
        let mapping = parse_repo_mapping(
            r#"{"":"","rules_go":"rules_go+","rules_rs":"rules_rs+","bd_crates":"rules_rs++crate+bd_crates"}"#,
        );
        let imported = BTreeSet::from([
            "rules_rs++crate+bd_crates".to_owned(),
            "rules_go++go_sdk+go_toolchains".to_owned(),
        ]);
        let repositories = || {
            [
                "rules_go+",
                "rules_rs+",
                "rules_rs++crate+bd_crates",
                "rules_rs++crate+bd_crates__anyhow-1.0.104",
                // An isolated root usage adds empty module/version components and its exported name.
                "rules_rs++_crate+++isolated_crates+crate_repo",
                "rules_go++go_sdk+go_toolchains",
                "rules_go++go_sdk+go_default_sdk",
                "+local+root_ext_repo",
            ]
            .into_iter()
            .map(|canonical| Repository {
                canonical_name: Some(canonical.into()),
                repo_rule_name: Some("http_archive".into()),
                ..Default::default()
            })
            .collect::<Vec<_>>()
        };
        let edges = BTreeMap::from([(
            "rules_go".to_owned(),
            BTreeSet::from(["rules_rs".to_owned()]),
        )]);
        let inputs_of = |targets: &[Target], name: &str| {
            targets
                .iter()
                .find(|target| target_name(target) == Some(name))
                .unwrap_or_else(|| panic!("{name} missing"))
                .rule
                .as_option()
                .unwrap()
                .rule_input
                .clone()
        };

        let query = lower_repositories(
            repositories(),
            &mapping,
            ModuleDependencies {
                edges: &edges,
                mappings: &BTreeMap::new(),
            },
            Some(&imported),
            &LabelOnlyRepositories::default(),
            Path::new("."),
            &mut materialize_repository_inputs,
        );
        assert_eq!(query.len(), 8);
        // The owning module reaches everything its extensions generate, after its module deps.
        assert_eq!(
            inputs_of(&query, "//external:rules_go"),
            [
                "//external:rules_go++go_sdk+go_default_sdk",
                "//external:rules_go++go_sdk+go_toolchains",
                "//external:rules_rs",
            ]
        );
        assert_eq!(
            inputs_of(&query, "//external:rules_rs"),
            [
                "//external:bd_crates",
                "//external:rules_rs++_crate+++isolated_crates+crate_repo",
                "//external:rules_rs++crate+bd_crates__anyhow-1.0.104",
            ]
        );
        // An imported repository reaches the siblings of its extension nobody imports.
        assert_eq!(
            inputs_of(&query, "//external:bd_crates"),
            ["//external:rules_rs++crate+bd_crates__anyhow-1.0.104"]
        );
        assert_eq!(
            inputs_of(&query, "//external:rules_go++go_sdk+go_toolchains"),
            ["//external:rules_go++go_sdk+go_default_sdk"]
        );
        // Spokes stay leaves, and the root's own extension has no module node to hang off.
        assert!(inputs_of(&query, "//external:rules_go++go_sdk+go_default_sdk").is_empty());
        assert!(inputs_of(
            &query,
            "//external:rules_rs++crate+bd_crates__anyhow-1.0.104"
        )
        .is_empty());
        assert!(inputs_of(&query, "//external:+local+root_ext_repo").is_empty());

        // A label-only extension's repositories leave its owner's node, while those of its other
        // usages stay, an isolated usage of the same extension included (issue #603). The
        // hub-to-spoke edges remain.
        let label_only = lower_repositories(
            repositories(),
            &mapping,
            ModuleDependencies {
                edges: &edges,
                mappings: &BTreeMap::new(),
            },
            Some(&imported),
            &LabelOnlyRepositories {
                extensions: BTreeSet::from([
                    "rules_go++go_sdk".to_owned(),
                    "rules_rs++crate".to_owned(),
                ]),
                ..Default::default()
            },
            Path::new("."),
            &mut materialize_repository_inputs,
        );
        assert_eq!(label_only.len(), 8);
        assert_eq!(
            inputs_of(&label_only, "//external:rules_go"),
            ["//external:rules_rs"]
        );
        assert_eq!(
            inputs_of(&label_only, "//external:rules_rs"),
            ["//external:rules_rs++_crate+++isolated_crates+crate_repo"]
        );
        assert_eq!(
            inputs_of(&label_only, "//external:bd_crates"),
            ["//external:rules_rs++crate+bd_crates__anyhow-1.0.104"]
        );
        assert_eq!(
            inputs_of(&label_only, "//external:rules_go++go_sdk+go_toolchains"),
            ["//external:rules_go++go_sdk+go_default_sdk"]
        );

        let cquery = lower_repositories(
            repositories(),
            &mapping,
            ModuleDependencies {
                edges: &edges,
                mappings: &BTreeMap::new(),
            },
            None,
            &LabelOnlyRepositories::default(),
            Path::new("."),
            &mut materialize_repository_inputs,
        );
        assert_eq!(cquery.len(), 8);
        assert_eq!(
            inputs_of(&cquery, "//external:rules_go"),
            ["//external:rules_rs"]
        );
        assert!(inputs_of(&cquery, "//external:rules_rs").is_empty());
        assert!(inputs_of(&cquery, "//external:bd_crates").is_empty());
        assert!(inputs_of(&cquery, "//external:rules_go++go_sdk+go_toolchains").is_empty());
    }

    #[test]
    fn label_only_extensions_resolve_to_unambiguous_usage_prefixes() {
        let mapping = parse_repo_mapping(
            r#"{"":"","rules_rs":"rules_rs+","rs":"rules_rs+","rules_go":"rules_go+"}"#,
        );
        let generated = [
            "rules_rs++crate+crates",
            "rules_go++go_sdk+go_toolchains",
            // Two `.bzl` files of rules_go export `pip`; Bazel names the second usage `pip2`.
            "rules_go++pip+pypi",
            "rules_go++pip2+pypi",
        ]
        .map(str::to_owned)
        .into();
        let mut options = probe_options(Path::new("."), PathBuf::from("unused"));
        options.label_only_module_extensions = [
            "@rules_rs//rs:extensions.bzl%crate",
            "@rs//rs:extensions.bzl%crate",
            "@@rules_go+//go:extensions.bzl%go_sdk",
            "@rules_go//python:extensions.bzl%pip",
            "@rules_go//go:extensions.bzl%nogo",
            "@unknown//:extensions.bzl%crate",
            "rules_rs.bzl%crate",
            "@rules_rs//rs:extensions.bzl%",
        ]
        .map(str::to_owned)
        .into();
        assert_eq!(
            options.label_only_extension_prefixes(&mapping, &generated),
            BTreeSet::from(["rules_go++go_sdk".to_owned(), "rules_rs++crate".to_owned()])
        );
    }

    #[test]
    fn filtering_preserves_new_variants_for_small_and_large_supplements() {
        let primary = ["//p:a", "//p:b", "//p:c", "//p:d", "//p:e", "//p:f"].map(rule_target);
        for primary in [&primary[..1], &primary[..]] {
            let mut extra = vec![
                rule_target("//p:new"),
                rule_target("//p:a"),
                rule_target("//p:new"),
            ];
            retain_supplemental_targets(&mut extra, primary);
            assert_eq!(
                extra.iter().filter_map(target_name).collect::<Vec<_>>(),
                ["//p:new", "//p:new"]
            );
        }
    }

    #[test]
    fn supplement_extends_coverage_without_replacing_primary_or_deduplicating_new_names() {
        let member = "owner++_ext+++usage+hub";
        let scope = IsolatedRepoScope {
            anchor: "hub".into(),
            members: BTreeSet::from([member.to_owned()]),
            aliases: RepoMapping::from([("hub".to_owned(), member.to_owned())]),
            ..Default::default()
        };
        let rule = |name: &str, input: &str| {
            let mut target = rule_target(name);
            target
                .rule
                .as_option_mut()
                .unwrap()
                .rule_input
                .push(input.to_owned());
            target
        };
        let primary = [rule("//pkg:reader", "@hub//:new")];
        let mut coverage = ScopedCoverage::default();
        coverage.record(&scope, &primary);
        assert_eq!(
            coverage.missing,
            BTreeSet::from([format!("@@{member}//:new")])
        );
        let mut extra = vec![
            rule("//pkg:reader", "@hub//:discarded"),
            rule("@hub//:new", "@hub//:new"),
            rule("@hub//:new", "@hub//:still_missing"),
        ];
        retain_supplemental_targets(&mut extra, &primary);
        assert_eq!(extra.len(), 2);
        coverage.record(&scope, &extra);
        assert_eq!(
            coverage.missing,
            BTreeSet::from([format!("@@{member}//:still_missing")])
        );
        assert_eq!(coverage.direct_readers, [true, true, true]);
    }

    #[test]
    fn scoped_build_loads_union_duplicate_observations_before_deduplication() {
        let repo = "owner++_ext+++usage+hub";
        let scope = IsolatedRepoScope {
            anchor: "hub".into(),
            members: BTreeSet::from([repo.to_owned()]),
            aliases: RepoMapping::from([("hub".to_owned(), repo.to_owned())]),
            ..Default::default()
        };
        let canonical = format!("@@{repo}//:defs.bzl");
        let source = |name: &str, loads: &[&str]| Target {
            r#type: target::Discriminator::SourceFile,
            source_file: buffa::MessageField::some(SourceFile {
                name: name.to_owned(),
                subinclude: loads.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut records = [
            source("@hub//:BUILD.bazel", &[&canonical]),
            source("@hub//:BUILD.bazel", &[]),
        ];
        for _ in 0..2 {
            let mut loads = BTreeMap::new();
            scope.record_build_loads(&records[..1], &mut loads);
            scope.record_build_loads(&records[1..], &mut loads);
            scope.record_build_loads(&[source("//pkg:BUILD", &[&canonical])], &mut loads);
            let mut targets = vec![
                rule_target("//pkg:consumer"),
                source("@hub//:BUILD.bazel", &[]),
            ];
            let mut coverage = ScopedCoverage::default();
            coverage.record(&scope, &targets);
            assert!(scope
                .connect_external_readers(&loads, &mut targets, &coverage)
                .is_err());
            targets.push(source("@hub//:defs.bzl", &[]));
            coverage.record(&scope, &targets[coverage.direct_readers.len()..]);
            scope
                .connect_external_readers(&loads, &mut targets, &coverage)
                .unwrap();
            assert_eq!(
                targets[0].rule.as_option().unwrap().rule_input,
                ["//external:hub"]
            );
            assert_eq!(
                targets[1].source_file.as_option().unwrap().subinclude,
                std::slice::from_ref(&canonical)
            );
            records.reverse();
        }
    }

    #[test]
    fn typed_repository_inputs_exclude_string_dictionary_keys() {
        use crate::proto::blaze_query::attribute::Discriminator as Kind;
        let mut attr = Attribute {
            r#type: Kind::LabelDictUnary,
            ..Default::default()
        };
        attr.label_dict_unary_value.push(Default::default());
        let entry = &mut attr.label_dict_unary_value[0];
        entry.key = "//:string".into();
        entry.value = "//:label".into();
        assert_eq!(
            attribute_label_values(&attr).collect::<Vec<_>>(),
            ["//:label"]
        );
        // The stock main-repository scan still sees both strings.
        assert_eq!(
            attribute_main_repo_labels(&[attr]).collect::<BTreeSet<_>>(),
            ["//:string", "//:label"].into()
        );
    }

    #[test]
    fn label_list_dictionary_reads_both_wire_encodings() {
        // LABEL_LIST_DICT in mod show_repo uses StringListDictEntry (field 11).
        // The schema also has LabelListDictEntry (field 10), with the same shape.
        for field in [0x5a, 0x52] {
            let mut wire = b"\x0a\x04deps\x10\x0c".to_vec();
            wire.push(field);
            wire.extend_from_slice(b"\x12\x0a\x07//:text\x12\x07//:file\x68\x01");
            let attribute = Attribute::decode_from_slice(&wire).unwrap();
            assert_eq!(
                attribute_label_values(&attribute).collect::<Vec<_>>(),
                ["//:file"]
            );
        }
    }

    #[test]
    fn isolated_usage_requires_a_complete_unique_root_identity() {
        let hub = "owner++_ext+++usage+hub";
        let spoke = "owner++_ext+++usage+spoke";
        let config = "owner++_ext+++usage+registry";
        let path = "owner++_ext+++usage+path";
        let peer = "owner++ext+neighbor";
        let other = "owner++_ext+++second+tool";
        let survey = ExtensionRepositories {
            imported: [hub.to_owned(), peer.to_owned(), other.to_owned()].into(),
            generated: [hub, spoke, config, path, peer, other]
                .map(str::to_owned)
                .into(),
            ..Default::default()
        };
        let mut rows: Vec<_> = [hub, spoke, config, path, peer, other]
            .map(|canonical| Repository {
                canonical_name: Some(canonical.to_owned()),
                original_name: Some(canonical.rsplit_once('+').unwrap().1.to_owned()),
                repo_rule_bzl_label: Some("@@owner+//:rules.bzl".into()),
                repo_rule_name: Some(
                    if canonical == path {
                        "new_unknown_class"
                    } else {
                        "archive"
                    }
                    .into(),
                ),
                ..Default::default()
            })
            .into();
        rows[1].attribute.push(Attribute {
            name: "registry_config".into(),
            r#type: attribute::Discriminator::Label,
            string_value: Some(format!("@@{config}//:dl")),
            ..Default::default()
        });
        let mapping =
            serde_json::to_vec(&serde_json::json!({"renamed":hub,"peer":peer,"mod":"owner+"}))
                .unwrap();
        let requested = "@renamed";
        let resolve = |rows: &[Repository], survey: &ExtensionRepositories| {
            resolve_isolated_root(requested, &mapping, survey, rows)
        };
        let refuses = |result: Result<IsolatedRepoScope>, expected: &str| {
            let error = match result {
                Ok(_) => panic!("isolated root unexpectedly accepted"),
                Err(error) => format!("{error:#}"),
            };
            assert!(error.contains(expected), "{error}");
        };
        for _ in 0..2 {
            assert_eq!(
                resolve(&rows, &survey).unwrap().members,
                [hub, spoke, config, path].map(str::to_owned).into()
            );
            rows.reverse();
        }
        // A substituted anchor cannot qualify a different isolated or unisolated group.
        for substituted in [peer, "foreign++_ext+dep+1+usage+hub"] {
            let diverted =
                serde_json::to_vec(&serde_json::json!({"renamed":substituted,"mod":"owner+"}))
                    .unwrap();
            refuses(
                resolve_isolated_root(requested, &diverted, &survey, &rows),
                substituted,
            );
        }
        // Separate imported members prevent the hub from covering the whole usage.
        let mut split = survey.clone();
        split.imported.insert(spoke.into());
        refuses(
            resolve(&rows, &split),
            &format!("extra imported repository {spoke}"),
        );
        // A known outside repository-time read has no guaranteed ordinary target edge.
        let mut outside = rows.clone();
        outside[4].attribute = outside[1].attribute.clone();
        refuses(
            resolve(&outside, &survey),
            &format!("outside repository {peer} attribute registry_config reads @@{config}//:dl"),
        );
        // Same wire slot, different meaning. String list dictionaries do not
        // declare a repository input; lists of labels can read this usage.
        let mut dict = Attribute {
            name: "settings".into(),
            r#type: attribute::Discriminator::StringListDict,
            ..Default::default()
        };
        dict.string_list_dict_value.push(Default::default());
        dict.string_list_dict_value[0].key = "//:text".into();
        dict.string_list_dict_value[0].value = vec![format!("@@{config}//:dl")];
        outside[4].attribute = vec![dict];
        assert!(resolve(&outside, &survey).is_ok());
        outside[4].attribute[0].r#type = attribute::Discriminator::LabelListDict;
        refuses(
            resolve(&outside, &survey),
            &format!("outside repository {peer} attribute settings reads @@{config}//:dl"),
        );
        // A repository with an output label is outside the supported read contract.
        for kind in [
            attribute::Discriminator::Output,
            attribute::Discriminator::OutputList,
        ] {
            outside[4].attribute[0].r#type = kind;
            refuses(
                resolve(&outside, &survey),
                &format!("attribute settings has unsupported type {kind:?}"),
            );
        }
        let mut ambiguous = survey;
        ambiguous
            .imported
            .insert("foreign++_ext+++usage+hub".into());
        refuses(resolve(&rows, &ambiguous), "foreign++_ext+++usage+hub");
        ambiguous.imported.remove("foreign++_ext+++usage+hub");
        let mut missing = rows.clone();
        missing.retain(|row| row.canonical_name.as_deref() != Some(spoke));
        refuses(resolve(&missing, &ambiguous), spoke);
        let mut replaced = rows.clone();
        replaced[1].original_name = Some("different".into());
        refuses(resolve(&replaced, &ambiguous), "different");
        let malformed = "owner++_ext+++usage+";
        ambiguous.generated.insert(malformed.into());
        let mut empty_name = rows.clone();
        empty_name.push(Repository {
            canonical_name: Some(malformed.into()),
            original_name: Some(String::new()),
            repo_rule_bzl_label: Some("@@owner+//:rules.bzl".into()),
            repo_rule_name: Some("archive".into()),
            ..Default::default()
        });
        refuses(resolve(&empty_name, &ambiguous), malformed);
        ambiguous.generated.remove(malformed);
        rows[1].attribute[0].string_value = Some("@ambiguous".into());
        refuses(resolve(&rows, &ambiguous), "unresolved label @ambiguous");
        rows[1].attribute[0].r#type = attribute::Discriminator::Unknown;
        refuses(resolve(&rows, &ambiguous), "unsupported type UNKNOWN");
    }

    #[test]
    fn extension_repos_carry_their_owning_modules_definition() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("rules.patch"), "patch v1").unwrap();
        let mapping = parse_repo_mapping(
            r#"{"":"","rules_rs":"rules_rs+","rules_rust":"rules_rs++rules_rust+rules_rust"}"#,
        );
        let repositories = || {
            vec![
                Repository {
                    canonical_name: Some("rules_rs+".into()),
                    repo_rule_name: Some("http_archive".into()),
                    attribute: vec![Attribute {
                        name: "patches".into(),
                        r#type: attribute::Discriminator::LabelList,
                        string_list_value: vec!["//:rules.patch".into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                Repository {
                    canonical_name: Some("rules_rs++rules_rust+rules_rust".into()),
                    repo_rule_name: Some("_rules_rust_repo".into()),
                    ..Default::default()
                },
                Repository {
                    canonical_name: Some("rules_rs++crate+bd_crates__anyhow".into()),
                    repo_rule_name: Some("crate_repo".into()),
                    ..Default::default()
                },
                Repository {
                    canonical_name: Some("rules_rs++_crate+++isolated_crates+crate_repo".into()),
                    repo_rule_name: Some("crate_repo".into()),
                    ..Default::default()
                },
                Repository {
                    canonical_name: Some("+local+root_repo".into()),
                    ..Default::default()
                },
            ]
        };
        let module_hash = |targets: &[Target], name: &str| {
            targets
                .iter()
                .find(|target| target_name(target) == Some(name))
                .unwrap_or_else(|| panic!("{name} missing"))
                .rule
                .as_option()
                .unwrap()
                .attribute
                .iter()
                .find(|attribute| attribute.name == "_bazel_diff_module_hash")
                .and_then(|attribute| attribute.string_value.clone())
        };
        let lower = || {
            lower_repositories(
                repositories(),
                &mapping,
                ModuleDependencies {
                    edges: &BTreeMap::new(),
                    mappings: &BTreeMap::new(),
                },
                None,
                &LabelOnlyRepositories::default(),
                workspace.path(),
                &mut materialize_repository_inputs,
            )
        };

        let before = lower();
        let hub = module_hash(&before, "//external:rules_rust").unwrap();
        // Every repository of the module's extensions carries the same digest of the module.
        for name in [
            "//external:rules_rs++crate+bd_crates__anyhow",
            "//external:rules_rs++_crate+++isolated_crates+crate_repo",
        ] {
            assert_eq!(module_hash(&before, name), Some(hub.clone()));
        }
        // The module itself and the root's own extension repos carry none.
        assert!(module_hash(&before, "//external:rules_rs").is_none());
        assert!(module_hash(&before, "//external:+local+root_repo").is_none());

        // Editing the module's override patch changes its extension repositories too (the
        // #522 case for a module consumed only through its extensions), in either mode.
        fs::write(workspace.path().join("rules.patch"), "patch v2").unwrap();
        let after = lower();
        assert_ne!(module_hash(&after, "//external:rules_rust").unwrap(), hub);

        // The digest ignores a mirror move of a pinned download, like the hasher does.
        let paths = RepositoryWorkspace::new(workspace.path());
        let pinned = |urls: &str| Repository {
            repo_rule_name: Some("http_archive".into()),
            attribute: vec![
                Attribute {
                    name: "urls".into(),
                    r#type: attribute::Discriminator::StringList,
                    string_list_value: vec![urls.into()],
                    ..Default::default()
                },
                Attribute {
                    name: "integrity".into(),
                    r#type: attribute::Discriminator::String,
                    string_value: Some("sha256-abc".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            prepared_definition_hash(&pinned("https://a/x.zip"), &paths),
            prepared_definition_hash(&pinned("https://b/x.zip"), &paths)
        );
        let mut unpinned = pinned("https://a/x.zip");
        unpinned.attribute.pop();
        let mut moved = pinned("https://b/x.zip");
        moved.attribute.pop();
        assert_ne!(
            prepared_definition_hash(&unpinned, &paths),
            prepared_definition_hash(&moved, &paths)
        );

        // The digest also ignores the checkout path in a `file://%workspace%` registry's patch
        // URLs, which their integrity pins; a changed integrity still changes it.
        let patched = |root: &str, integrity: &str| Repository {
            repo_rule_name: Some("http_archive".into()),
            attribute: vec![Attribute {
                name: "remote_patches".into(),
                r#type: attribute::Discriminator::StringDict,
                string_dict_value: vec![crate::proto::blaze_query::StringDictEntry {
                    key: format!("file://{root}/registry/patches/fix.patch"),
                    value: integrity.into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            prepared_definition_hash(&patched("/one", "sha256-p"), &paths),
            prepared_definition_hash(&patched("/two", "sha256-p"), &paths)
        );
        assert_ne!(
            prepared_definition_hash(&patched("/one", "sha256-p"), &paths),
            prepared_definition_hash(&patched("/one", "sha256-q"), &paths)
        );
    }

    /// Fake Bazel for the extension-repository flow: a root that imports `rules_go` and a
    /// `pypi` hub, `rules_go` importing `go_toolchains`, and two repositories per extension
    /// that nobody imports. `show_repo` answers with the pre-encoded `repositories`, and
    /// `query` with `//app:lib`.
    #[cfg(unix)]
    fn extension_flow_script(
        workspace: &Path,
        repositories: &[Repository],
        failing_representative: Option<&str>,
    ) -> PathBuf {
        let repo_proto = workspace.join("show-repo.pb");
        write_delimited(&repo_proto, repositories);
        let query_proto = workspace.join("query-result.pb");
        write_delimited(&query_proto, &[rule_target("//app:lib")]);
        let calls = workspace.join("calls.log");
        let script = workspace.join("fake-bazel.sh");
        let fail = failing_representative.unwrap_or("never-matches");
        let body = format!(
            r#"#!/bin/sh
args="$*"
echo "$args" >> "{calls}"
if [ "$1 $2" = "info output_base" ]; then
  pwd
  exit 0
fi
if echo "$args" | grep -q "mod graph --output=json"; then
  if [ -f omit-graph-hint ]; then
    echo '{{"key":"<root>","name":"root","dependencies":[{{"key":"rules_go@0.62.0","name":"rules_go","dependencies":[]}}]}}'
  else
    echo '{{"key":"<root>","name":"root","dependencies":[{{"key":"rules_go@0.62.0","name":"rules_go","dependencies":[]}},{{"key":"rules_python@1.8.4","name":"rules_python","dependencies":[]}}]}}'
  fi
  exit 0
fi
if echo "$args" | grep -q "mod graph"; then
  echo "root"
  exit 0
fi
if echo "$args" | grep -q "version"; then
  echo "Build label: 8.7.0"
  exit 0
fi
if [ "$2" = "dump_repo_mapping" ]; then
  shift 2
  if [ -z "$1" ]; then
    echo '{{"":"","rules_go":"rules_go+","rules_python":"rules_python+","pypi":"rules_python++pip+pypi","bazel_tools":"bazel_tools"}}'
    shift
    if [ "$#" -eq 0 ] || [ -f truncated-survey ]; then exit 0; fi
    if [ -f unavailable-survey ]; then exit 2; fi
  fi
  for repo in "$@"; do
    if [ "$repo" = "unknown_hidden+" ]; then echo "Unknown module repository" >&2; exit 2; fi
    if [ "$repo" = "{fail}" ]; then
      echo "ERROR: extension evaluation failed for $repo" >&2
      exit 2
    fi
    case "$repo" in
      rules_go+) echo '{{"go_toolchains":"rules_go++go_sdk+go_toolchains","bazel_tools":"bazel_tools","rules_go":"rules_go+"}}' ;;
      rules_python+) echo '{{"pypi":"rules_python++pip+pypi","rules_python":"rules_python+"}}' ;;
      rules_go++go_sdk+*) echo '{{"go_toolchains":"rules_go++go_sdk+go_toolchains","go_default_sdk":"rules_go++go_sdk+go_default_sdk","rules_go":"rules_go+"}}' ;;
      rules_python++pip+*) echo '{{"pypi":"rules_python++pip+pypi","pypi_312_six":"rules_python++pip+pypi_312_six","rules_python":"rules_python+"}}' ;;
      *) echo '{{"rules_go":"rules_go+","pypi":"rules_python++pip+pypi","rules_python":"rules_python+"}}' ;;
    esac
  done
  exit 0
fi
if echo "$args" | grep -q "mod show_repo"; then
  cat "{repos}"
  exit 0
fi
response="{query}"
for arg do
  case "$previous" in
    --output_file) output=$arg ;;
    --query_file) expression=$arg ;;
    --starlark:file) formatter=$arg ;;
  esac
  previous=$arg
done
if [ "$1" = cquery ]; then
  response=configured.pb
  if [ -n "$formatter" ]; then
    response=compatible.txt
    if grep -q ToolchainInfo "$formatter"; then response=providers.txt; fi
  fi
elif [ -f selected-inputs.pb ] && grep -q '^let d = deps(' "$expression"; then
  if grep -q '//:sdk' "$expression"; then exit 7; fi
  response=selected-inputs.pb
elif [ -f ordinary-sdk.pb ] && grep -q '^let d = set(' "$expression"; then
  response=ordinary-sdk.pb
fi
if [ -n "$output" ]; then cp "$response" "$output"; exit $?; fi
exit 1
"#,
            calls = calls.display(),
            repos = repo_proto.display(),
            query = query_proto.display(),
        );
        fs::write(&script, body).unwrap();
        script
    }

    #[test]
    #[cfg(unix)]
    fn ordinary_hashes_follow_configured_toolchains_and_files_through_an_external_rule() {
        use crate::hash::{generate_hashes, HashOptions};
        use crate::model::impacted_targets;

        const APP: &str = "@pypi//:app";
        const ALIAS: &str = "@pypi//extra:lookup";
        const BRIDGE: &str = "@rules_python//:bridge";
        const SIBLING: &str = "rules_python++pip+pypi_312_six";
        const DATA: &str = "@@rules_python++pip+pypi_312_six//:data";
        const CLIENT: &str = "//:uses_app";
        const DATA_CLIENT: &str = "//:uses_data";
        const SDK: &str = "//:sdk";
        let workspace = tempfile::tempdir().unwrap();
        let dir = workspace.path();
        let mut repos: Vec<_> = ["rules_python+", "rules_python++pip+pypi", SIBLING]
            .into_iter()
            .map(|name| Repository {
                canonical_name: Some(name.into()),
                repo_rule_name: Some("http_archive".into()),
                ..Default::default()
            })
            .collect();
        let script = extension_flow_script(dir, &repos, None);
        let mut bazel = probe_options(dir, script);
        bazel.fine_grained_external_repos.insert("@pypi".into());
        let options = HashOptions {
            bazel,
            content_hashes: None,
            ignored_attributes: Default::default(),
            seed_filepaths: Default::default(),
            modified_filepaths: Default::default(),
            track_deps: true,
            always_affected_tags: Default::default(),
            partial_tree: None,
        };
        let rule = |name: &str, inputs: &[&str]| {
            let mut target = rule_target(name);
            target.rule.as_option_mut().unwrap().rule_input =
                inputs.iter().map(|name| (*name).to_owned()).collect();
            target
        };
        let version = Target {
            r#type: target::Discriminator::SourceFile,
            source_file: buffa::MessageField::some(SourceFile {
                name: "//:version".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        write_delimited(
            &dir.join("query-result.pb"),
            &[
                rule(CLIENT, &[APP]),
                rule(DATA_CLIENT, &[ALIAS, "@rules_python//:wrapper"]),
                rule(APP, &["@rules_python//:wrapper"]),
                version,
            ],
        );
        // The fine repository's alias is referenced by a main rule but was not in the
        // primary response. Its selected actual forwards data from the generated sibling.
        write_delimited(
            &dir.join("ordinary-sdk.pb"),
            &[rule(SDK, &["//:version", "@other//:unused"])],
        );
        let mut alias = rule(ALIAS, &[BRIDGE]);
        alias.rule.as_option_mut().unwrap().rule_class = "alias".into();
        write_delimited(
            &dir.join("selected-inputs.pb"),
            &[alias, rule(BRIDGE, &[DATA]), rule(DATA, &[])],
        );
        let edges = [
            (CLIENT, APP),
            (DATA_CLIENT, ALIAS),
            (ALIAS, BRIDGE),
            (BRIDGE, DATA),
            (APP, SDK),
            (SDK, "//:version"),
        ];
        let rows = edges
            .iter()
            .map(|(parent, child)| {
                let mut row = rule_target(parent);
                row.rule
                    .as_option_mut()
                    .unwrap()
                    .configured_rule_input
                    .push(ConfiguredRuleInput {
                        label: Some((*child).into()),
                        configuration_id: Some(1),
                        ..Default::default()
                    });
                configured(row, Some("abc123"), 1)
            })
            .collect::<Vec<_>>();
        write_delimited(&dir.join("configured.pb"), &[results(rows)]);
        fs::write(dir.join("providers.txt"), SDK).unwrap();
        let compatible = dir.join("compatible.txt");
        fs::write(&compatible, edges.map(|(parent, _)| parent).join("\n")).unwrap();
        fs::write(dir.join("version"), "1").unwrap();
        let hashes = || generate_hashes(&options).unwrap();
        let base = hashes();
        assert!(base.dep_edges[APP].iter().any(|input| input == SDK));
        assert!(base.dep_edges[ALIAS].iter().any(|input| input == BRIDGE));
        assert!(base.dep_edges[BRIDGE].iter().any(|input| input == DATA));
        let changed = |before: &crate::model::HashFileData, after: &crate::model::HashFileData| {
            impacted_targets(&before.hashes, &after.hashes, None, false)
                .unwrap()
                .into_iter()
                .filter(|label| label == CLIENT || label == DATA_CLIENT)
                .collect::<Vec<_>>()
        };
        fs::write(dir.join("version"), "2").unwrap();
        assert_eq!(changed(&base, &hashes()), [CLIENT]);
        fs::write(dir.join("version"), "1").unwrap();

        // A sibling RepoSpec changes without changing the SDK that the application consumes.
        repos[2].attribute.push(Attribute {
            name: "sha256".into(),
            r#type: attribute::Discriminator::String,
            string_value: Some("a".repeat(64)),
            ..Default::default()
        });
        write_delimited(&dir.join("show-repo.pb"), &repos);
        assert_eq!(changed(&base, &hashes()), [DATA_CLIENT]);

        // A missing caller in the configured response preserves the legacy owner/hub paths.
        fs::write(compatible, [DATA_CLIENT, APP, BRIDGE].join("\n")).unwrap();
        let partial = hashes();
        assert!(!partial.dep_edges.contains_key(ALIAS));
        repos[2].attribute.clear();
        write_delimited(&dir.join("show-repo.pb"), &repos);
        assert_eq!(changed(&partial, &hashes()), [CLIENT, DATA_CLIENT]);
    }

    #[test]
    #[cfg(unix)]
    fn query_bzlmod_repos_enumerates_extension_repositories() {
        let workspace = tempfile::tempdir().unwrap();
        let repositories = [
            "rules_go+",
            "rules_python+",
            "rules_python++pip+pypi",
            "rules_python++pip+pypi_312_six",
            "rules_go++go_sdk+go_toolchains",
            "rules_go++go_sdk+go_default_sdk",
        ]
        .into_iter()
        .map(|canonical| Repository {
            canonical_name: Some(canonical.into()),
            repo_rule_name: Some("http_archive".into()),
            ..Default::default()
        })
        .collect::<Vec<_>>();
        let script = extension_flow_script(workspace.path(), &repositories, None);
        let options = BazelOptions {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("/bin/sh"),
            startup_options: vec![script.to_str().unwrap().to_owned()],
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: true,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        };

        // A legacy //external rule can have the same name as a synthetic repository.
        let overlapping = "//external:rules_python++pip+pypi_312_six";
        write_delimited(
            &workspace.path().join("query-result.pb"),
            &[rule_target("//app:lib"), rule_target(overlapping)],
        );
        let targets = options.query_all_targets().unwrap();
        let names = targets
            .iter()
            .filter_map(target_name)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            names,
            BTreeSet::from([
                "//app:lib",
                "//external:pypi",
                "//external:rules_go",
                "//external:rules_go++go_sdk+go_default_sdk",
                "//external:rules_go++go_sdk+go_toolchains",
                "//external:rules_python",
                "//external:rules_python++pip+pypi_312_six",
            ])
        );
        let inputs_of = |name: &str| {
            targets
                .iter()
                .find(|target| target_name(target) == Some(name))
                .unwrap()
                .rule
                .as_option()
                .unwrap()
                .rule_input
                .clone()
        };
        assert_eq!(
            inputs_of("//external:rules_go"),
            [
                "//external:rules_go++go_sdk+go_default_sdk",
                "//external:rules_go++go_sdk+go_toolchains",
            ]
        );
        assert_eq!(
            inputs_of("//external:pypi"),
            ["//external:rules_python++pip+pypi_312_six"]
        );
        assert_eq!(
            inputs_of("//external:rules_go++go_sdk+go_toolchains"),
            ["//external:rules_go++go_sdk+go_default_sdk"]
        );

        // Every generated repository was asked of `show_repo`, in one invocation, and the
        // mapping rounds were one over the modules and one over a representative per extension.
        let calls = fs::read_to_string(workspace.path().join("calls.log")).unwrap();
        let show_repo = calls
            .lines()
            .filter(|line| line.contains("mod show_repo"))
            .collect::<Vec<_>>();
        assert_eq!(show_repo.len(), 1, "{calls}");
        for repo in [
            "@@rules_go++go_sdk+go_default_sdk",
            "@@rules_python++pip+pypi_312_six",
            "@@rules_python++pip+pypi",
            "@@rules_go+",
        ] {
            assert!(show_repo[0].contains(repo), "{}", show_repo[0]);
        }
        let dumps = calls
            .lines()
            .filter(|line| line.contains("mod dump_repo_mapping"))
            .collect::<Vec<_>>();
        // The successful root probe is reused before the dependent module and
        // representative mapping rounds.
        assert_eq!(dumps.len(), 3, "{calls}");
        assert!(
            dumps[1].ends_with("mod dump_repo_mapping rules_go+ rules_python+"),
            "{calls}"
        );
        assert!(
            dumps[2].ends_with(
                "mod dump_repo_mapping rules_go++go_sdk+go_toolchains rules_python++pip+pypi"
            ),
            "{calls}"
        );

        // Definition hashing must use the same rule that deduplication keeps.
        let options = crate::hash::HashOptions {
            bazel: options,
            content_hashes: None,
            ignored_attributes: HashSet::new(),
            seed_filepaths: BTreeSet::new(),
            modified_filepaths: BTreeSet::new(),
            track_deps: true,
            always_affected_tags: HashSet::new(),
            partial_tree: None,
        };
        let raw = crate::hash::hash_targets(&options, targets).unwrap();
        let streamed = crate::hash::generate_hashes(&options).unwrap();
        assert_eq!(raw.hashes[overlapping], streamed.hashes[overlapping]);
    }

    #[test]
    #[cfg(unix)]
    fn module_survey_preserves_ordered_targets_and_falls_back_on_incomplete_answers() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path();
        let repo = Repository {
            canonical_name: Some("rules_go++go_sdk+go_default_sdk".into()),
            repo_rule_name: Some("http_archive".into()),
            ..Default::default()
        };
        let options = probe_options(path, extension_flow_script(path, &[repo], None));
        let original = options
            .query_all_targets_with(materialize_repository_inputs_for_batch)
            .unwrap();
        fs::write(path.join("MODULE.bazel"), "").unwrap();
        for (marker, expected_dumps) in [
            (None, 2),
            (Some("truncated-survey"), 4),
            (Some("unavailable-survey"), 4),
        ] {
            if let Some(name) = marker {
                fs::write(path.join(name), "").unwrap();
            }
            fs::write(path.join("calls.log"), "").unwrap();
            let queried = options
                .query_all_targets_with(materialize_repository_inputs_for_batch)
                .unwrap();
            assert_eq!(queried.targets, original.targets);
            assert_eq!(queried.root_mapping, original.root_mapping);
            assert_eq!(queried.module_graph_json, original.module_graph_json);
            let calls = fs::read_to_string(path.join("calls.log")).unwrap();
            let dumps = calls
                .lines()
                .filter(|line| line.contains("mod dump_repo_mapping"))
                .collect::<Vec<_>>();
            assert_eq!(dumps.len(), expected_dumps, "{calls}");
            assert!(
                dumps[0].ends_with("mod dump_repo_mapping  rules_go+ rules_python+"),
                "{calls}"
            );
            assert_eq!(
                calls
                    .lines()
                    .filter(|line| line.contains("mod graph --output=json"))
                    .count(),
                1,
                "{calls}"
            );
            if let Some(name) = marker {
                fs::remove_file(path.join(name)).unwrap();
            }
        }

        // The root mapping, not the graph hint, decides which modules must be read.
        fs::remove_file(path.join("MODULE.bazel")).unwrap();
        fs::write(path.join("omit-graph-hint"), "").unwrap();
        let original = options
            .query_all_targets_with(materialize_repository_inputs_for_batch)
            .unwrap();
        fs::write(path.join("MODULE.bazel"), "").unwrap();
        fs::write(path.join("calls.log"), "").unwrap();
        let queried = options
            .query_all_targets_with(materialize_repository_inputs_for_batch)
            .unwrap();
        assert_eq!(queried.targets, original.targets);
        assert_eq!(queried.root_mapping, original.root_mapping);
        assert_eq!(queried.module_graph_json, original.module_graph_json);
        let calls = fs::read_to_string(path.join("calls.log")).unwrap();
        let dumps = calls
            .lines()
            .filter(|line| line.contains("mod dump_repo_mapping"))
            .collect::<Vec<_>>();
        assert_eq!(dumps.len(), 3, "{calls}");
        assert!(
            dumps[0].ends_with("mod dump_repo_mapping  rules_go+"),
            "{calls}"
        );
        assert!(
            dumps[1].ends_with("mod dump_repo_mapping rules_python+"),
            "{calls}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn extension_repository_enumeration_skips_broken_extensions_only_with_keep_going() {
        let workspace = tempfile::tempdir().unwrap();
        let repositories = ["rules_go+", "rules_python+", "rules_python++pip+pypi"]
            .into_iter()
            .map(|canonical| Repository {
                canonical_name: Some(canonical.into()),
                repo_rule_name: Some("http_archive".into()),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let script = extension_flow_script(
            workspace.path(),
            &repositories,
            Some("rules_go++go_sdk+go_toolchains"),
        );
        let mut options = BazelOptions {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("/bin/sh"),
            startup_options: vec![script.to_str().unwrap().to_owned()],
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: true,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        };
        let error = options.query_all_targets().unwrap_err().to_string();
        assert!(error.contains("extension evaluation failed"), "{error}");

        // With --keep_going the broken extension is skipped and the healthy one still enumerated.
        options.keep_going = true;
        let targets = options.query_all_targets().unwrap();
        let names = targets
            .iter()
            .filter_map(target_name)
            .collect::<BTreeSet<_>>();
        assert!(names.contains("//external:pypi"), "{names:?}");
        assert!(names.contains("//external:rules_go"), "{names:?}");
        let calls = fs::read_to_string(workspace.path().join("calls.log")).unwrap();
        let show_repo = calls
            .lines()
            .rev()
            .find(|line| line.contains("mod show_repo"))
            .unwrap();
        assert!(
            show_repo.contains("@@rules_python++pip+pypi"),
            "{show_repo}"
        );
        assert!(!show_repo.contains("go_default_sdk"), "{show_repo}");

        // Recovery can lose a hidden module without a failed extension. Keep the
        // global owner on that survey, even if the visible module read succeeded.
        let visible = BTreeMap::from([("rules_python+".into(), vec!["rules_python".into()])]);
        let mut failed = FailedExtensions::default();
        let resolved = |hidden, failed: &mut FailedExtensions| {
            options
                .extension_repositories(&visible, hidden, BTreeMap::new(), true, failed)
                .unwrap()
        };
        assert!(resolved(Vec::new(), &mut failed).complete_mappings);
        let recovered = resolved(vec!["unknown_hidden+".into()], &mut failed);
        assert!(!recovered.complete_mappings);
        assert!(failed.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn dependency_fingerprint_fallback_chain_covers_streamed_and_text_paths() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("MODULE.bazel"),
            "module(name = \"test\")\n",
        )
        .unwrap();

        let selected_repos_marker = workspace.path().join("selected-repos-called");
        let script = workspace.path().join("fake-bazel.sh");
        let body = format!(
            r#"#!/bin/sh
args="$*"
if echo "$args" | grep -q "mod graph --output=json"; then
  printf '{{"modules":["m"]}}\n'
  exit 0
fi
if echo "$args" | grep -q "mod graph"; then
  echo "root"
  exit 0
fi
if echo "$args" | grep -q "mod dump_repo_mapping"; then
  echo '{{"pip":"rules_python+0.31.0","local":"local~override","main":"main"}}'
  exit 0
fi
if echo "$args" | grep -q "mod show_repo" && echo "$args" | grep -q -- "--output=streamed_proto"; then
  exit 2
fi
if echo "$args" | grep -q "mod show_repo" && echo "$args" | grep -q -- "--all_visible_repos" && echo "$args" | grep -q -- "--output=text"; then
  exit 3
fi
if echo "$args" | grep -q "mod show_repo" && echo "$args" | grep -q -- "--output=text"; then
  : > "{}"
  echo "selected-repos-text"
  exit 0
fi
exit 1
"#,
            selected_repos_marker.display(),
        );
        fs::write(&script, body).unwrap();

        let options = BazelOptions {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("/bin/sh"),
            startup_options: vec![script.to_str().unwrap().to_owned()],
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: false,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        };

        let fingerprint = options.dependency_fingerprint().unwrap();
        assert_eq!(fingerprint.len(), 64);
        assert!(fingerprint
            .chars()
            .all(|character| character.is_ascii_hexdigit()));
        assert!(selected_repos_marker.is_file());
    }

    #[test]
    #[cfg(unix)]
    fn query_bzlmod_repos_honors_keep_going() {
        let workspace = tempfile::tempdir().unwrap();
        let query_proto = workspace.path().join("query-result.pb");
        write_delimited(&query_proto, &[rule_target("//app:lib")]);

        // Fake Bazel where `mod graph` succeeds (`is_bzlmod_enabled()` is true),
        // `version` returns 8.6.1, `mod dump_repo_mapping` returns an external repo,
        // `mod show_repo` fails, and `query` writes `//app:lib` to `--output_file`.
        let show_repo_fail_script = workspace.path().join("fake-bazel-show-repo-fail.sh");
        let body = format!(
            r#"#!/bin/sh
args="$*"
if echo "$args" | grep -q "mod graph"; then
  echo "root"
  exit 0
fi
if echo "$args" | grep -q "version"; then
  echo "Build label: 8.6.1"
  exit 0
fi
if echo "$args" | grep -q "mod dump_repo_mapping"; then
  echo '{{"pip":"rules_python+0.31.0"}}'
  exit 0
fi
if echo "$args" | grep -q "mod show_repo"; then
  echo "transient fetch failure" >&2
  exit 2
fi
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output_file" ]; then
    cp "{}" "$2"
    exit 0
  fi
  shift
done
exit 1
"#,
            query_proto.display(),
        );
        fs::write(&show_repo_fail_script, body).unwrap();

        let mut options = BazelOptions {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("/bin/sh"),
            startup_options: vec![show_repo_fail_script.to_str().unwrap().to_owned()],
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: true,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        };

        let show_repo_err = options.query_all_targets().unwrap_err();
        assert!(show_repo_err
            .to_string()
            .contains("bazel mod show_repo failed"));

        // With `keep_going = true`, `query_all_targets` logs a warning and falls back to main targets.
        options.keep_going = true;
        let targets = options.query_all_targets().unwrap();
        assert_eq!(
            targets.iter().filter_map(target_name).collect::<Vec<_>>(),
            ["//app:lib"]
        );
    }

    #[test]
    fn extension_failure_reads_both_bazel_wordings() {
        let not_generated = "ERROR: module extension @@transitive_dep+//:extensions.bzl%broken does not generate repository \"missing_repo\", yet it is imported as \"missing_repo\" in the usage at @@transitive_dep+//:MODULE.bazel:10:23\nERROR: Results may be incomplete as 1 extension failed.\n";
        let failure = extension_failure(not_generated).unwrap();
        assert_eq!(
            failure.extensions,
            BTreeSet::from(["@@transitive_dep+//:extensions.bzl%broken".to_owned()])
        );
        assert!(failure.answered);

        let evaluation = "ERROR: /cache/external/rules_swift+/swift/extensions.bzl:19:9: Traceback (most recent call last):\n\tFile \"/cache/external/rules_swift+/swift/extensions.bzl\", line 19, column 9, in _impl\n\t\tfail(\"boom\")\nError in fail: boom\nERROR: error evaluating module extension @@rules_swift+//swift:extensions.bzl%non_module_deps\n";
        let failure = extension_failure(evaluation).unwrap();
        assert_eq!(
            failure.extensions,
            BTreeSet::from(["@@rules_swift+//swift:extensions.bzl%non_module_deps".to_owned()])
        );
        assert!(!failure.answered);

        assert!(extension_failure("ERROR: Bzlmod has to be enabled\n").is_none());
        assert!(extension_failure("ERROR: Unknown command 'dump_repo_mapping'\n").is_none());
    }

    #[cfg(unix)]
    fn probe_options(workspace: &Path, script: PathBuf) -> BazelOptions {
        BazelOptions {
            workspace: workspace.to_path_buf(),
            bazel: PathBuf::from("/bin/sh"),
            startup_options: vec![script.to_str().unwrap().to_owned()],
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            cquery_root_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            label_only_module_extensions: BTreeSet::new(),
            isolated_dependency_root: None,
            exclude_external_targets: true,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        }
    }

    /// `is_bzlmod_enabled()` is decided by `dump_repo_mapping ""`, which a failed module
    /// extension in the graph does not touch; `mod graph`, which does, is the fallback for a
    /// Bazel without `dump_repo_mapping` and counts even when it only answered partially.
    #[test]
    #[cfg(unix)]
    fn bzlmod_probe_survives_a_failed_extension_and_an_old_bazel() {
        let workspace = tempfile::tempdir().unwrap();
        let script = |name: &str, dump: &str, graph: &str| {
            let path = workspace.path().join(name);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\nif [ \"$2\" = dump_repo_mapping ]; then\n{dump}\nfi\nif [ \"$2\" = graph ]; then\n{graph}\nfi\nexit 1\n"
                ),
            )
            .unwrap();
            path
        };
        let marker = "echo 'ERROR: error evaluating module extension @@dep+//:ext.bzl%broken' >&2; echo 'ERROR: Results may be incomplete as 1 extension failed.' >&2; exit 2";

        let mapping_answers_graph_fails = script(
            "probe-a.sh",
            "echo '{\"\":\"\",\"dep\":\"dep+\"}'; exit 0",
            marker,
        );
        assert!(probe_options(workspace.path(), mapping_answers_graph_fails).is_bzlmod_enabled());

        let no_dump_repo_mapping = script(
            "probe-b.sh",
            "echo \"ERROR: Unknown command 'dump_repo_mapping'\" >&2; exit 2",
            "echo '<root>'; exit 0",
        );
        assert!(probe_options(workspace.path(), no_dump_repo_mapping).is_bzlmod_enabled());

        let no_dump_repo_mapping_graph_partial = script("probe-c.sh", "exit 2", marker);
        assert!(
            probe_options(workspace.path(), no_dump_repo_mapping_graph_partial).is_bzlmod_enabled()
        );

        let bzlmod_disabled = script(
            "probe-d.sh",
            "echo 'ERROR: Bzlmod has to be enabled for mod command to work.' >&2; exit 2",
            "echo 'ERROR: Bzlmod has to be enabled for mod command to work.' >&2; exit 2",
        );
        assert!(!probe_options(workspace.path(), bzlmod_disabled).is_bzlmod_enabled());
    }

    /// Fake Bazel for a workspace whose transitive module `transitive_dep` imports
    /// `missing_repo` from an extension that does not generate it (#531), answering the way
    /// Bazel 8.7 and 9.3 do: `mod graph`, `dump_repo_mapping <module>` and `show_repo` print
    /// everything that does not need the extension, report it on stderr and exit 2; asking
    /// for one of the extension's repositories by name prints nothing and exits 37 (or 2 for
    /// `show_repo`) without the `Results may be incomplete` line.
    #[cfg(unix)]
    fn failed_extension_script(workspace: &Path, query_targets: &[Target]) -> PathBuf {
        fs::write(workspace.join("MODULE.bazel"), "").unwrap();
        let repo_proto = workspace.join("show-repo.pb");
        write_delimited(
            &repo_proto,
            &["direct_dep+", "transitive_dep+"]
                .into_iter()
                .map(|canonical| Repository {
                    canonical_name: Some(canonical.into()),
                    repo_rule_name: Some("local_repository".into()),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        );
        let query_proto = workspace.join("query-result.pb");
        write_delimited(&query_proto, query_targets);
        let calls = workspace.join("calls.log");
        let script = workspace.join("fake-bazel.sh");
        let body = format!(
            r#"#!/bin/sh
args="$*"
echo "$args" >> "{calls}"
err='ERROR: module extension @@transitive_dep+//:extensions.bzl%broken does not generate repository "missing_repo", yet it is imported as "missing_repo" in the usage at @@transitive_dep+//:MODULE.bazel:10:23'
marker='ERROR: Results may be incomplete as 1 extension failed.'
if echo "$args" | grep -q "mod graph --output=json"; then
  echo '{{"key":"<root>","name":"root","dependencies":[{{"key":"direct_dep@_","name":"direct_dep","dependencies":[{{"key":"transitive_dep@_","name":"transitive_dep","dependencies":[]}}]}},{{"key":"transitive_dep@_","name":"transitive_dep","dependencies":[]}}]}}'
  echo "$err" >&2; echo "$marker" >&2; exit 2
fi
if echo "$args" | grep -q "mod graph"; then
  echo "<root>"; echo "$err" >&2; echo "$marker" >&2; exit 2
fi
if echo "$args" | grep -q "version"; then
  echo "Build label: 8.7.0"
  exit 0
fi
if [ "$2" = "dump_repo_mapping" ] && [ -z "$3" ]; then
  echo '{{"":"","direct_dep":"direct_dep+","transitive_dep":"transitive_dep+","missing_repo":"transitive_dep++broken+missing_repo","bazel_tools":"bazel_tools"}}'
  exit 0
fi
if echo "$args" | grep -q "mod show_repo" && echo "$args" | grep -q "transitive_dep++broken+"; then
  echo "$err" >&2; echo "$err. Type 'bazel help mod' for syntax and help." >&2; exit 2
fi
if echo "$args" | grep -q "transitive_dep++broken+"; then
  echo "$err" >&2; exit 37
fi
if echo "$args" | grep -q "mod dump_repo_mapping"; then
  for repo in $(echo "$args" | sed 's/.*dump_repo_mapping//'); do
    case "$repo" in
      direct_dep+) echo '{{"direct_dep":"direct_dep+","transitive_dep":"transitive_dep+","bazel_tools":"bazel_tools"}}' ;;
      transitive_dep+) echo '{{"missing_repo":"transitive_dep++broken+missing_repo","transitive_dep":"transitive_dep+","bazel_tools":"bazel_tools"}}' ;;
    esac
  done
  exit 0
fi
if echo "$args" | grep -q "mod show_repo"; then
  cat "{repos}"
  echo "$err" >&2; echo "$marker" >&2; exit 2
fi
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output_file" ]; then
    cp "{query}" "$2"
    exit 0
  fi
  shift
done
exit 1
"#,
            calls = calls.display(),
            repos = repo_proto.display(),
            query = query_proto.display(),
        );
        fs::write(&script, &body).unwrap();
        script
    }

    #[cfg(unix)]
    fn rule_with_inputs(name: &str, inputs: &[&str]) -> Target {
        let mut target = Target::default();
        target.r#type = target::Discriminator::Rule;
        target.rule = buffa::MessageField::some(Rule {
            name: name.to_owned(),
            rule_class: "sh_library".to_owned(),
            rule_input: inputs.iter().map(|input| (*input).to_owned()).collect(),
            ..Default::default()
        });
        target
    }

    /// The module repositories are hashed although `bazel mod` exits 2 over the failed
    /// extension, and the extension's own repository, which the root imports, is left out
    /// of the `show_repo` batch instead of failing it.
    #[test]
    #[cfg(unix)]
    fn query_bzlmod_repos_skips_a_failed_extension() {
        let workspace = tempfile::tempdir().unwrap();
        let script = failed_extension_script(workspace.path(), &[rule_target("//app:lib")]);
        let options = probe_options(workspace.path(), script);

        let targets = options.query_all_targets().unwrap();
        let names = targets
            .iter()
            .filter_map(target_name)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            names,
            BTreeSet::from([
                "//app:lib",
                "//external:direct_dep",
                "//external:transitive_dep",
            ])
        );
        let calls = fs::read_to_string(workspace.path().join("calls.log")).unwrap();
        let show_repo = calls
            .lines()
            .filter(|line| line.contains("mod show_repo"))
            .collect::<Vec<_>>();
        assert!(
            show_repo.last().is_some_and(|line| {
                line.contains("@@direct_dep+")
                    && line.contains("@@transitive_dep+")
                    && !line.contains("missing_repo")
            }),
            "{calls}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn isolated_root_requires_a_supported_complete_ordinary_survey() {
        let refuses = |options: &BazelOptions, expected: &str| {
            let mut transformed = false;
            let result = options.query_all_targets_with(|_, _| transformed = true);
            let error = match result {
                Ok(_) => panic!("isolated root unexpectedly accepted"),
                Err(error) => error,
            };
            let error = format!("{error:#}");
            assert!(error.contains(expected), "{error}");
            assert!(
                error.contains(&format!(
                    "--isolatedDependencyRoot={}",
                    options.isolated_dependency_root.as_deref().unwrap()
                )),
                "{error}"
            );
            assert!(
                !transformed,
                "repository targets were hashed before admission"
            );
        };
        let workspace = tempfile::tempdir().unwrap();
        let script = failed_extension_script(workspace.path(), &[rule_target("//app:lib")]);
        let mut options = probe_options(workspace.path(), script.clone());
        options.isolated_dependency_root = Some("@missing_repo".into());
        refuses(
            &options,
            "module extension @@transitive_dep+//:extensions.bzl%broken failed",
        );

        let body = fs::read_to_string(&script)
            .unwrap()
            .replace("Build label: 8.7.0", "Build label: 9.0.0");
        fs::write(script, body).unwrap();
        refuses(
            &options,
            "Bazel 9.0.0 does not support Bzlmod repository discovery",
        );
        options.isolated_dependency_root = None;
        assert_eq!(
            options
                .query_all_targets()
                .unwrap()
                .iter()
                .filter_map(target_name)
                .collect::<Vec<_>>(),
            ["//app:lib"]
        );

        options.isolated_dependency_root = Some("@missing_repo".into());
        options.use_cquery = true;
        let calls = query_calls(workspace.path());
        refuses(&options, "requires ordinary query");
        assert_eq!(query_calls(workspace.path()), calls);

        let disabled = tempfile::tempdir().unwrap();
        let script = query_script(disabled.path(), "8.7.0", &[], 0, 0);
        let mut options = probe_options(disabled.path(), script);
        options.isolated_dependency_root = Some("@hub".into());
        refuses(&options, "Bzlmod repository discovery is unavailable");
    }

    /// A target that depends on a repository of the failed extension has an input the
    /// hashes cannot cover: an error, or a warning under `--keep_going`.
    #[test]
    #[cfg(unix)]
    fn referenced_failed_extension_repo_is_an_error_without_keep_going() {
        let workspace = tempfile::tempdir().unwrap();
        let script = failed_extension_script(
            workspace.path(),
            &[
                rule_target("//app:lib"),
                rule_with_inputs("//app:uses_missing", &["//app:lib", "@missing_repo//:x"]),
                rule_with_inputs(
                    "//app:uses_missing_canonical",
                    &["@@transitive_dep++broken+unimported_repo//:x"],
                ),
            ],
        );
        let mut options = probe_options(workspace.path(), script);

        let error = options.query_all_targets().unwrap_err().to_string();
        assert!(
            error.contains(
                "//app:uses_missing depends on @missing_repo//:x, whose repository mapping failed"
            ),
            "{error}"
        );
        assert!(
            error.contains("reported module extensions: @@transitive_dep+//:extensions.bzl%broken"),
            "{error}"
        );
        assert!(
            error.contains(
                "//app:uses_missing_canonical depends on @@transitive_dep++broken+unimported_repo//:x"
            ),
            "{error}"
        );

        options.keep_going = true;
        let targets = options.query_all_targets().unwrap();
        let names = targets
            .iter()
            .filter_map(target_name)
            .collect::<BTreeSet<_>>();
        assert!(names.contains("//app:uses_missing"), "{names:?}");
        assert!(names.contains("//external:direct_dep"), "{names:?}");
    }

    fn configured(
        target: Target,
        checksum: Option<&str>,
        configuration_id: u32,
    ) -> ConfiguredTarget {
        ConfiguredTarget {
            target: buffa::MessageField::some(target),
            configuration: match checksum {
                Some(checksum) => buffa::MessageField::some(Configuration {
                    checksum: checksum.to_owned(),
                    ..Default::default()
                }),
                None => buffa::MessageField::none(),
            },
            configuration_id,
            ..Default::default()
        }
    }

    fn results(results: Vec<ConfiguredTarget>) -> CqueryResult {
        CqueryResult {
            results,
            ..Default::default()
        }
    }

    #[test]
    fn cquery_targets_are_named_by_their_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let source = Target {
            r#type: target::Discriminator::SourceFile,
            source_file: buffa::MessageField::some(SourceFile {
                name: "//a:a.txt".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let generated = Target {
            r#type: target::Discriminator::GeneratedFile,
            generated_file: buffa::MessageField::some(GeneratedFile {
                name: "//a:out".into(),
                generating_rule: "//a:a".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        // Bazel streams the configuration table after the results that refer to it, so a
        // result without an inline checksum is named once the stream has ended.
        let messages = [
            results(vec![configured(rule_target("//a:a"), Some("abc123"), 1)]),
            results(vec![configured(rule_target("//a:a"), None, 2)]),
            results(vec![
                configured(source, Some("null"), 0),
                configured(generated, Some("abc123"), 1),
            ]),
            CqueryResult {
                configurations: vec![Configuration {
                    id: 2,
                    checksum: "def456".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ];
        let streamed = directory.path().join("cquery-stream.pb");
        write_delimited(&streamed, &messages);
        let mut seen = Vec::new();
        let targets = decode_cquery_stream(
            &streamed,
            true,
            &mut |batch| {
                seen.extend(
                    batch
                        .iter()
                        .map(|target| target_name(target).unwrap().to_owned()),
                )
            },
            None,
            None,
        )
        .unwrap();
        let names = targets.iter().filter_map(target_name).collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "//a:a|abc123",
                "//a:a.txt",
                "//a:out|abc123",
                "//a:a|def456"
            ]
        );
        assert_eq!(seen, names, "the transform sees configured names");
        assert_eq!(
            targets[2]
                .generated_file
                .as_option()
                .unwrap()
                .generating_rule,
            "//a:a|abc123"
        );
        assert_eq!(deduplicate_targets(targets).unwrap().len(), 4);

        let non_streamed = directory.path().join("cquery.pb");
        let mut combined = results(vec![configured(rule_target("//a:a"), None, 2)]);
        combined.configurations = vec![Configuration {
            id: 2,
            checksum: "def456".into(),
            ..Default::default()
        }];
        fs::write(&non_streamed, combined.encode_to_vec()).unwrap();
        let targets = decode_cquery_stream(&non_streamed, false, &mut |_| {}, None, None).unwrap();
        assert_eq!(target_name(&targets[0]), Some("//a:a|def456"));
    }

    #[test]
    fn configured_root_decoding_requires_unambiguous_configurations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cquery.pb");
        let source = |name: &str| Target {
            r#type: target::Discriminator::SourceFile,
            source_file: buffa::MessageField::some(SourceFile {
                name: name.to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let compatible = HashSet::from(["//a:missing".to_owned(), "//a:source|abc".to_owned()]);
        write_delimited(
            &path,
            &[
                results(vec![configured(source("//a:source.txt"), None, 7)]),
                CqueryResult {
                    configurations: vec![Configuration {
                        id: 7,
                        checksum: "null".to_owned(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
        );
        let mut roots = Vec::new();
        decode_cquery_stream(
            &path,
            true,
            &mut |_| {},
            Some(&mut roots),
            Some(&compatible),
        )
        .unwrap();
        assert_eq!(
            roots,
            [("//a:source.txt".to_owned(), "//a:source.txt".to_owned())]
        );

        write_delimited(
            &path,
            &[results(vec![configured(
                rule_target("//a:missing"),
                None,
                7,
            )])],
        );
        let error =
            decode_cquery_stream(&path, true, &mut |_| {}, None, Some(&compatible)).unwrap_err();
        assert!(error.to_string().contains("missing configuration 7"));
        let legacy = decode_cquery_stream(&path, true, &mut |_| {}, None, None).unwrap();
        assert_eq!(target_name(&legacy[0]), Some("//a:missing"));

        write_delimited(
            &path,
            &[results(vec![configured(
                source("//a:source|abc"),
                Some("null"),
                0,
            )])],
        );
        let error =
            decode_cquery_stream(&path, true, &mut |_| {}, None, Some(&compatible)).unwrap_err();
        assert!(error
            .to_string()
            .contains("//a:source|abc has a bare label ambiguous"));
    }

    #[test]
    fn explicit_aspect_completion_requires_requested_terminal_marker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cquery.pb");
        let row = results(vec![configured(rule_target("//a:root"), Some("abc123"), 0)]);
        let marker = CqueryResult {
            explicit_aspect_inputs: true,
            ..Default::default()
        };
        let verified = |frames: &[CqueryResult], expected| {
            write_delimited(&path, frames);
            decode_cquery_graph(&path, true, &mut |_| {}, None, None, true, expected)
                .unwrap()
                .into_verified()
                .is_some()
        };
        assert!(!verified(std::slice::from_ref(&row), true));
        assert!(verified(&[row.clone(), marker.clone()], true));
        assert!(!verified(&[row.clone(), marker.clone()], false));
        assert!(!verified(&[marker.clone(), row.clone()], true));
        assert!(!verified(
            &[row.clone(), marker.clone(), CqueryResult::default()],
            true
        ));
        let mut combined = row.clone();
        combined.explicit_aspect_inputs = true;
        assert!(!verified(std::slice::from_ref(&combined), true));

        fs::write(&path, combined.encode_to_vec()).unwrap();
        assert!(
            decode_cquery_graph(&path, false, &mut |_| {}, None, None, true, true)
                .unwrap()
                .into_verified()
                .is_some()
        );
        assert!(
            decode_cquery_graph(&path, false, &mut |_| {}, None, None, true, false)
                .unwrap()
                .into_verified()
                .is_none()
        );

        write_delimited(&path, &[row, marker]);
        let ordinary = decode_cquery_stream(&path, true, &mut |_| {}, None, None).unwrap();
        assert_eq!(target_name(&ordinary[0]), Some("//a:root|abc123"));
        let mut truncated = fs::read(&path).unwrap();
        truncated.pop();
        fs::write(&path, truncated).unwrap();
        assert!(decode_cquery_graph(&path, true, &mut |_| {}, None, None, true, true).is_err());
    }

    #[test]
    fn explicit_aspects_resolve_query_local_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cquery.pb");
        let decode = |configuration_id, aspect_id, parameters: &[(&str, &str)]| {
            let predecessor_id = aspect_id + 10;
            let input = |label: &str| ConfiguredRuleInput {
                label: Some(label.to_owned()),
                configuration_checksum: Some("abc123".to_owned()),
                configuration_id: Some(configuration_id),
                ..Default::default()
            };
            let mut root = rule_target("//a:root");
            root.rule.as_option_mut().unwrap().configured_rule_input = vec![ConfiguredRuleInput {
                aspect_id: Some(aspect_id),
                ..input("//a:base")
            }];
            write_delimited(
                &path,
                &[
                    CqueryResult {
                        results: vec![
                            configured(root, Some("abc123"), configuration_id),
                            configured(rule_target("//a:base"), Some("abc123"), configuration_id),
                            configured(rule_target("//a:tool"), Some("abc123"), configuration_id),
                        ],
                        aspect_results: vec![ConfiguredAspectResult {
                            aspect_id,
                            configured_rule_input: vec![
                                input("//a:tool"),
                                ConfiguredRuleInput {
                                    aspect_id: Some(predecessor_id),
                                    ..input("//a:base")
                                },
                            ],
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                    CqueryResult {
                        aspects: vec![
                            ConfiguredAspect {
                                id: aspect_id,
                                aspect_descriptor: buffa::MessageField::some(AspectDescriptor {
                                    name: "//a:defs.bzl%visit".to_owned(),
                                    parameters: parameters
                                        .iter()
                                        .map(|&(key, value)| KeyValuePair {
                                            key: key.to_owned(),
                                            value: value.to_owned(),
                                            ..Default::default()
                                        })
                                        .collect(),
                                    ..Default::default()
                                }),
                                base_target: buffa::MessageField::some(input("//a:base")),
                                base_aspect_ids: vec![predecessor_id],
                                ..Default::default()
                            },
                            ConfiguredAspect {
                                id: predecessor_id,
                                aspect_descriptor: buffa::MessageField::some(AspectDescriptor {
                                    name: "//a:defs.bzl%base".to_owned(),
                                    ..Default::default()
                                }),
                                base_target: buffa::MessageField::some(input("//a:base")),
                                ..Default::default()
                            },
                        ],
                        configurations: vec![Configuration {
                            id: configuration_id,
                            checksum: "abc123".to_owned(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                ],
            );
            decode_cquery_graph(&path, true, &mut |_| {}, None, None, true, false)?
                .into_verified()
                .context("typed aspect records were returned")
        };
        let before = decode(1, 7, &[("z", "one"), ("a", "two")]).unwrap();
        let after = decode(9, 3, &[("a", "two"), ("z", "one")]).unwrap();
        assert_eq!(before.aspects.len(), 1);
        assert_eq!(after.aspects.len(), 1);
        assert_eq!(before.aspects[0].key, after.aspects[0].key);
        assert_eq!(before.aspects[0].inputs, after.aspects[0].inputs);
        assert_eq!(before.aspects[0].inputs[0], "//a:tool|abc123");
        assert!(before.aspects[0].inputs[1].starts_with(EXPLICIT_ASPECT_PREFIX));
        for graph in [&before.targets, &after.targets] {
            let root = graph
                .iter()
                .find_map(|target| {
                    target
                        .rule
                        .as_option()
                        .filter(|rule| rule.name == "//a:root|abc123")
                })
                .unwrap();
            assert_eq!(
                root.configured_rule_input[0].label.as_deref(),
                Some(before.aspects[0].key.as_str())
            );
            assert_eq!(root.configured_rule_input[0].aspect_id, None);
        }
        let changed = decode(9, 3, &[("a", "three"), ("z", "one")]).unwrap();
        assert_ne!(before.aspects[0].key, changed.aspects[0].key);
        assert!(decode(1, 0, &[]).is_err());
    }

    #[test]
    fn configured_keys_split_into_label_and_checksum() {
        assert_eq!(configured_key("//a:a", "0f"), "//a:a|0f");
        assert_eq!(split_configured("//a:a|abc123"), ("//a:a", Some("abc123")));
        assert_eq!(split_configured("//a:a"), ("//a:a", None));
        // `|` is legal in a target name; only a hexadecimal suffix is a checksum.
        assert_eq!(split_configured("//a:odd|name"), ("//a:odd|name", None));
        assert_eq!(
            split_configured("//a:odd|name|0f"),
            ("//a:odd|name", Some("0f"))
        );
        assert_eq!(target_label("//a:a|"), "//a:a|");
        assert_eq!(target_label("//a:a|abc123"), "//a:a");
    }

    /// Fake Bazel for the `query`/`cquery` runners: `version` reports `version`; the
    /// compatible-target cquery (`--output starlark`) lists `compatible`, one per line, with a
    /// blank line that the runner must ignore; the proto cquery copies `cquery_proto` to its
    /// `--output_file`, or prints it when the runner redirects stdout instead (a Bazel older
    /// than 8.2), then exits with `cquery_exit`; `query` copies `query_proto` the same way and
    /// exits with `query_exit`,
    /// reporting on stderr when that is non-zero. Every invocation is appended to `calls.log`.
    #[cfg(unix)]
    fn query_script(
        workspace: &Path,
        version: &str,
        compatible: &[&str],
        query_exit: i32,
        cquery_exit: i32,
    ) -> PathBuf {
        let result = results(vec![
            configured(rule_target("//app:lib"), Some("abc123"), 1),
            configured(rule_target("//app:incompatible"), Some("abc123"), 1),
            configured(rule_target("//app:tool"), Some("def456"), 2),
        ]);
        let streamed_proto = workspace.join("cquery-stream.pb");
        write_delimited(&streamed_proto, std::slice::from_ref(&result));
        let proto = workspace.join("cquery-result.pb");
        fs::write(&proto, result.encode_to_vec()).unwrap();
        let query_proto = workspace.join("query-result.pb");
        write_delimited(&query_proto, &[rule_target("//app:lib")]);
        let compatible_file = workspace.join("compatible.txt");
        fs::write(
            &compatible_file,
            compatible
                .iter()
                .map(|label| format!("{label}\n"))
                .collect::<String>()
                .replacen('\n', "\n\n", 1),
        )
        .unwrap();
        let script = workspace.join("fake-bazel.sh");
        let body = format!(
            r#"#!/bin/sh
echo "$*" >> "{calls}"
copy_or_print() {{
  for arg do
    if [ "$next" = 1 ]; then cp "$1" "$arg"; return 0; fi
    [ "$arg" = "--output_file" ] && next=1
  done
  cat "$1"
}}
if [ "$1" = version ]; then
  echo "Build label: {version}"
  exit 0
fi
if [ "$1" = cquery ]; then
  if echo "$*" | grep -q -- "--output starlark"; then
    copy_or_print "{compatible}" "$@"
    exit 0
  fi
  if echo "$*" | grep -q -- "--output proto"; then
    copy_or_print "{proto}" "$@"
  else
    copy_or_print "{streamed}" "$@"
  fi
  exit {cquery_exit}
fi
if [ "$1" = query ]; then
  copy_or_print "{query}" "$@"
  if [ {query_exit} -ne 0 ]; then
    echo "ERROR: no such package 'missing': BUILD file not found" >&2
  fi
  exit {query_exit}
fi
exit 1
"#,
            calls = workspace.join("calls.log").display(),
            compatible = compatible_file.display(),
            streamed = streamed_proto.display(),
            proto = proto.display(),
            query = query_proto.display(),
        );
        fs::write(&script, body).unwrap();
        script
    }

    #[cfg(unix)]
    fn query_calls(workspace: &Path) -> Vec<String> {
        fs::read_to_string(workspace.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    #[cfg(unix)]
    fn acquired_package_inputs_refresh_only_real_builds() {
        const BUILD: &str = "@sdk_rules//:BUILD";
        const LOAD: &str = "@sdk_rules//:defs.bzl";
        const CHANGED_LOAD: &str = "@sdk_rules//:changed.bzl";
        let workspace = tempfile::tempdir().unwrap();
        let dir = workspace.path();
        let source = |name: &str, loads: &[&str], errors: Option<bool>| Target {
            r#type: target::Discriminator::SourceFile,
            source_file: buffa::MessageField::some(SourceFile {
                name: name.into(),
                subinclude: loads.iter().map(|label| (*label).into()).collect(),
                package_contains_errors: errors,
                ..Default::default()
            }),
            ..Default::default()
        };
        let script = dir.join("query.sh");
        fs::write(
            &script,
            r#"set -eu
[ "$1" = query ]
strict=no
while [ "$#" -gt 0 ]; do
  case "$1" in
    --query_file) shift; expression=$(cat "$1") ;;
    --output_file) shift; output="$1" ;;
    --keep_going) strict=no ;;
    --nokeep_going) strict=yes ;;
  esac
  shift
done
[ "$strict" = yes ]
echo "$expression" >> calls.log
[ "$expression" = "set('@sdk_rules//:BUILD')" ]
cp response.pb "$output"
if [ -f fail ]; then exit 3; fi
"#,
        )
        .unwrap();
        let mut options = probe_options(dir, script);
        options.command_options.push("--keep_going".into());
        let rows = || {
            vec![
                source(BUILD, &[], None),
                source("@load_only//:BUILD", &[], None),
                source(LOAD, &[], None),
                source(CHANGED_LOAD, &[], None),
                rule_with_inputs("@sdk_rules//:sdk_impl", &[]),
            ]
        };
        let collect = |targets: &mut [Target]| {
            options
                .package_inputs_for_acquired_rules(targets, 4, Some(BazelVersion(8, 8, 2)))
                .unwrap()
        };
        write_delimited(
            &dir.join("response.pb"),
            &[source(BUILD, &[LOAD], Some(false))],
        );
        let mut ambiguous = rows();
        let recovered = collect(&mut ambiguous);
        let mut real = rows();
        real[0] = source(BUILD, &[LOAD], Some(false));
        assert_eq!(collect(&mut real), recovered);
        assert_eq!(ambiguous[0], real[0]);
        assert_eq!(query_calls(dir), ["set('@sdk_rules//:BUILD')"]);
        assert_eq!(recovered["@sdk_rules//"], [BUILD, LOAD]);
        assert!(ambiguous[1]
            .source_file
            .as_option()
            .unwrap()
            .subinclude
            .is_empty());

        // A changed real load remains a changed input to the acquired rule.
        write_delimited(
            &dir.join("response.pb"),
            &[source(BUILD, &[CHANGED_LOAD], Some(false))],
        );
        assert_eq!(collect(&mut rows())["@sdk_rules//"], [BUILD, CHANGED_LOAD]);

        // A missing positive marker or partial query cannot replace the old row.
        write_delimited(&dir.join("response.pb"), &[source(BUILD, &[LOAD], None)]);
        assert_eq!(collect(&mut rows())["@sdk_rules//"], [BUILD]);
        write_delimited(
            &dir.join("response.pb"),
            &[source(BUILD, &[LOAD], Some(false))],
        );
        fs::write(dir.join("fail"), []).unwrap();
        assert_eq!(collect(&mut rows())["@sdk_rules//"], [BUILD]);
    }

    #[test]
    #[cfg(unix)]
    fn external_filegroup_inputs_follow_only_observed_groups_and_sources() {
        const OUTER: &str = "@@hub+//:files";
        const INNER: &str = "@@tools+//lib:inner";
        const COPY: &str = "@@tools+//lib:copy.txt";
        const TOOL: &str = "@@hub+//:compiler";
        const KNOWN: &str = "@@bazel_tools//tools/genrule:known";
        const SETUP: &str = "@@bazel_tools//tools/genrule:genrule-setup.sh";
        let workspace = tempfile::tempdir().unwrap();
        let dir = workspace.path();
        let native = |name, class: &str, inputs: &[&str]| {
            let mut target = rule_with_inputs(name, inputs);
            let rule = target.rule.as_option_mut().unwrap();
            rule.rule_class = class.into();
            rule.rule_class_key = Some(class.into());
            target
        };
        let source = |name: &str| Target {
            r#type: target::Discriminator::SourceFile,
            source_file: buffa::MessageField::some(SourceFile {
                name: name.into(),
                location: Some(format!("/source/{}:1:1", name.rsplit_once(':').unwrap().1)),
                visibility_label: vec!["//visibility:public".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let outer = native(OUTER, "filegroup", &[INNER]);
        let inner = native(INNER, "filegroup", &[COPY]);
        let copy = source(COPY);
        let setup = source(SETUP);
        let tool = rule_with_inputs(TOOL, &["@@unloaded+//:implementation"]);
        let known = native(KNOWN, "filegroup", &[SETUP]);
        for row in [&outer, &inner, &copy, &setup, &tool, &known] {
            let file = dir.join(target_name(row).unwrap());
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            write_delimited(&file, std::slice::from_ref(row));
        }

        let reader = native("//:reader", "genrule", &[OUTER]);
        let full_reader = native("//:reader", "genrule", &[OUTER, TOOL, SETUP]);
        write_delimited(
            &dir.join("full-parent"),
            &[full_reader, outer.clone(), tool, setup.clone()],
        );
        write_delimited(&dir.join("filtered-parent"), &[reader.clone(), outer]);
        let build = source("//:BUILD.bazel");
        write_delimited(
            &dir.join("loads"),
            &[
                build.clone(),
                source("@@hub+//:defs.bzl"),
                source("@@boundary+//:BUILD.bazel"),
            ],
        );
        // Both readers share a BUILD.bazel with an omitted load list. Only the .bzl
        // returned by buildfiles can seed a package; the boundary BUILD cannot.
        let mut primary = [
            build,
            reader,
            rule_with_inputs("//:also_reads_tool", &[TOOL, "@@boundary+//:unloaded"]),
            rule_with_inputs("//:known", &[KNOWN]),
            setup,
        ];

        // Return only the requested label frames. An opaque tool's own implementation
        // has no frame. The real Bazel E2E checks the meaning of these projection flags.
        let script = dir.join("query.sh");
        fs::write(&script, r#"set -eu
[ "$1" = query ]
while [ "$#" -gt 0 ]; do
  case "$1" in
    --query_file) shift; expression=$(cat "$1") ;;
    --notool_deps) tools=no ;;
    --tool_deps) tools=yes ;;
    --noimplicit_deps) implicit=no ;;
    --implicit_deps) implicit=yes ;;
  esac
  shift
done
echo "$tools $implicit $expression" >> calls.log
case "$expression" in
  "buildfiles(set('//:reader'))"|"buildfiles(set('//:also_reads_tool'))")
    cat loads
    if [ -f fail-loads ]; then exit 3; fi ;;
  "deps(set('//:reader'), 1)")
    case "$tools $implicit" in
      "yes yes") cat full-parent ;;
      "no no") cat filtered-parent ;;
      *) exit 1 ;;
    esac ;;
  set\(*)
    for label in $(printf '%s\n' "$expression" | sed "s/^set(//; s/)$//; s/'//g"); do cat "$label"; done ;;
  *) exit 1 ;;
esac
"#).unwrap();
        let mut options = probe_options(dir, script);
        options.command_options = vec!["--notool_deps".into(), "--noimplicit_deps".into()];
        let mut sources = BTreeMap::new();
        let mut parents = BTreeMap::new();
        let (rows, groups) = options
            .external_filegroup_inputs(
                &primary,
                Some(b"{}"),
                Some(BazelVersion(8, 0, 0)),
                &mut sources,
                &mut parents,
            )
            .unwrap();

        assert_eq!(
            rows.iter().filter_map(target_name).collect::<BTreeSet<_>>(),
            BTreeSet::from([OUTER, INNER, COPY, KNOWN])
        );
        assert_eq!(groups[OUTER].declaration.rule_input, [INNER]);
        assert_eq!(groups[INNER].declaration.rule_input, [COPY]);
        assert_eq!(
            parents["//:reader"].declaration.rule_input,
            [OUTER, TOOL, SETUP]
        );
        assert_eq!(sources[COPY].location, "/source/copy.txt:1:1");
        assert_eq!(sources[SETUP].visibility, ["//visibility:public"]);
        assert!(query_calls(dir)
            .iter()
            .any(|call| call.starts_with("no no set(")));

        // A strict query may emit valid rows before failing. Already known groups
        // are still acquired; partial loads cannot authorize new candidate packages.
        fs::write(dir.join("fail-loads"), "").unwrap();
        for reported_error in [None, Some(false), Some(true)] {
            primary[0]
                .source_file
                .as_option_mut()
                .unwrap()
                .package_contains_errors = reported_error;
            compact_target(&mut primary[0]);
            let calls_before = query_calls(dir).len();
            let (rows, groups) = options
                .external_filegroup_inputs(
                    &primary,
                    Some(b"{}"),
                    Some(BazelVersion(8, 0, 0)),
                    &mut BTreeMap::new(),
                    &mut BTreeMap::new(),
                )
                .unwrap();
            assert_eq!(
                rows.iter().filter_map(target_name).collect::<Vec<_>>(),
                [KNOWN]
            );
            assert_eq!(
                groups.keys().map(String::as_str).collect::<Vec<_>>(),
                [KNOWN]
            );
            let calls = query_calls(dir);
            assert_eq!(
                calls[calls_before..]
                    .iter()
                    .any(|call| call.contains("buildfiles(")),
                reported_error.is_none()
            );
        }
    }

    /// `cquery` first asks Bazel which targets are compatible with the target platform, then
    /// streams the configured targets and keeps only the compatible ones, matched by label
    /// with the configuration checksum stripped. A Bazel before 7.0 has no streamed cquery
    /// output, so the runner asks for `--output proto` instead, and one before 8.2 has no
    /// `--output_file`, so stdout is redirected to the output file.
    #[test]
    #[cfg(unix)]
    fn cquery_filters_to_compatible_targets_across_bazel_versions() {
        for (version, output, output_file) in [
            ("8.7.0", "streamed_proto", true),
            ("7.4.1", "streamed_proto", false),
            ("6.5.0", "proto", false),
        ] {
            let workspace = tempfile::tempdir().unwrap();
            let script = query_script(
                workspace.path(),
                version,
                &["//app:lib", "//app:tool"],
                0,
                0,
            );
            let mut options = probe_options(workspace.path(), script);
            options.use_cquery = true;
            options.cquery_options = vec!["--define=flavor=test".into()];
            options.command_options = vec!["--query-only".into()];

            let targets = options.query("deps(//app:all)", true).unwrap();
            let names = targets.iter().filter_map(target_name).collect::<Vec<_>>();
            assert_eq!(
                names,
                ["//app:lib|abc123", "//app:tool|def456"],
                "Bazel {version}: the incompatible target is dropped, the rest keep their configuration"
            );

            let calls = query_calls(workspace.path());
            assert_eq!(calls.len(), 3, "{calls:?}");
            assert_eq!(calls[0], "version");
            let compatible = &calls[1];
            assert!(
                compatible
                    .starts_with("cquery --define=flavor=test --output starlark --starlark:file "),
                "{compatible}"
            );
            assert!(
                compatible.contains("--consistent_labels --query_file"),
                "{compatible}"
            );
            assert!(!compatible.contains("--query-only"), "{compatible}");
            assert_eq!(
                compatible.contains("--output_file"),
                output_file,
                "Bazel {version}: {compatible}"
            );
            let streamed = &calls[2];
            assert!(
                streamed.starts_with(&format!(
                    "cquery --proto:instantiation_stack --define=flavor=test --transitions=lite --output {output} --consistent_labels --query_file "
                )),
                "Bazel {version}: {streamed}"
            );
            assert!(!streamed.contains("--order_output=no"), "{streamed}");
            assert!(!streamed.contains("--keep_going"), "{streamed}");
            assert_eq!(
                streamed.contains("--output_file"),
                output_file,
                "Bazel {version}: {streamed}"
            );
        }
    }

    /// A completion marker does not upgrade a partial command result to verified coverage.
    #[test]
    #[cfg(unix)]
    fn verification_rejects_partial_query_even_with_completion_marker() {
        let workspace = tempfile::tempdir().unwrap();
        let script = query_script(workspace.path(), "8.7.0", &["//app:lib"], 0, 3);
        write_delimited(
            &workspace.path().join("cquery-stream.pb"),
            &[
                results(vec![configured(
                    rule_target("//app:lib"),
                    Some("abc123"),
                    0,
                )]),
                CqueryResult {
                    explicit_aspect_inputs: true,
                    ..Default::default()
                },
            ],
        );
        let mut options = probe_options(workspace.path(), script);
        options.use_cquery = true;
        options.keep_going = true;
        options.cquery_root_expression = Some("//app:lib".into());
        let ordinary = options
            .query_with_capture(
                "deps(config(//app:lib, target))",
                true,
                Some(BazelVersion(8, 7, 0)),
                false,
                &mut |_, _| {},
                CqueryCapture::Standard,
                |_| true,
            )
            .unwrap();
        assert_eq!(target_name(&ordinary.targets[0]), Some("//app:lib|abc123"));
        let error = options
            .query_with_capture(
                "deps(config(//app:lib, target))",
                true,
                Some(BazelVersion(8, 7, 0)),
                false,
                &mut |_, _| {},
                CqueryCapture::Verification,
                |_| true,
            )
            .err()
            .expect("partial verification must be unavailable");
        assert!(error
            .to_string()
            .contains("Bazel query failed, exit code 3"));
    }

    /// Ordinary query accepts only exit 3 under keep-going; other failures retain their stderr.
    #[test]
    #[cfg(unix)]
    fn query_surfaces_bazel_failures_and_tolerates_partial_answers_with_keep_going() {
        let workspace = tempfile::tempdir().unwrap();
        let script = query_script(workspace.path(), "8.7.0", &[], 1, 0);
        let mut options = probe_options(workspace.path(), script);
        options.command_options = vec!["--noimplicit_deps".into()];

        let error = options.query("//app:all", false).unwrap_err().to_string();
        assert!(
            error.contains("Bazel query failed, exit code 1: ERROR: no such package 'missing'"),
            "{error}"
        );
        let calls = query_calls(workspace.path());
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(
            calls[1].starts_with(
                "query --output streamed_proto --proto:instantiation_stack --order_output=no --noimplicit_deps --query_file "
            ),
            "{}",
            calls[1]
        );

        // Exit code 1 is still an error under --keep_going; only 3 is a partial answer.
        options.keep_going = true;
        let error = options.query("//app:all", false).unwrap_err().to_string();
        assert!(error.contains("Bazel query failed, exit code 1"), "{error}");
        assert!(
            query_calls(workspace.path())[3].contains("--keep_going"),
            "{:?}",
            query_calls(workspace.path())
        );

        let partial = tempfile::tempdir().unwrap();
        let script = query_script(partial.path(), "8.7.0", &[], 3, 0);
        let mut options = probe_options(partial.path(), script);
        let error = options.query("//app:all", false).unwrap_err().to_string();
        assert!(error.contains("Bazel query failed, exit code 3"), "{error}");
        options.keep_going = true;
        let targets = options.query("//app:all", false).unwrap();
        assert_eq!(
            targets.iter().filter_map(target_name).collect::<Vec<_>>(),
            ["//app:lib"]
        );

        // The compatible-target cquery failing is an error of its own, before any proto cquery.
        let incompatible = tempfile::tempdir().unwrap();
        let script = incompatible.path().join("fake-bazel.sh");
        fs::write(
            &script,
            "#!/bin/sh\necho \"$*\" >> calls.log\nif [ \"$1\" = version ]; then echo 'Build label: 8.7.0'; exit 0; fi\necho 'ERROR: no such target' >&2\nexit 1\n",
        )
        .unwrap();
        let mut options = probe_options(incompatible.path(), script);
        options.use_cquery = true;
        let error = options
            .query("deps(//app:all)", true)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("compatible-target cquery failed: ERROR: no such target"),
            "{error}"
        );
        assert_eq!(query_calls(incompatible.path()).len(), 2);
    }
}
