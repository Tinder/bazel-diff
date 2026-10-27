use crate::cancel::{self, Cancellation};
use crate::proto::analysis::{ConfiguredTarget, CqueryResult};
use crate::proto::blaze_query::{
    target, Attribute, ConfiguredRuleInput, PackageGroup, Repository, Rule, Target,
};
use anyhow::{anyhow, bail, Context, Result};
use buffa::Message;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::time::Instant;
use tempfile::NamedTempFile;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct BazelVersion(u32, u32, u32);

#[derive(Clone, Debug)]
pub struct BazelOptions {
    pub workspace: PathBuf,
    pub bazel: PathBuf,
    pub startup_options: Vec<String>,
    pub command_options: Vec<String>,
    pub cquery_options: Vec<String>,
    pub use_cquery: bool,
    pub cquery_expression: Option<String>,
    pub keep_going: bool,
    pub fine_grained_external_repos: BTreeSet<String>,
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
        Ok((String::from_utf8_lossy(&stdout).trim().to_owned(), complete))
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

    pub fn query_all_targets(&self) -> Result<Vec<Target>> {
        let QueriedTargets {
            targets,
            module_graph_json: _module_graph_json,
            root_mapping: _root_mapping,
        } = self.query_all_targets_with(materialize_repository_inputs_for_batch)?;
        Ok(targets)
    }

    pub(crate) fn query_all_targets_with(
        &self,
        mut transform: impl FnMut(&mut [Target], Option<RepositoryInputs>),
    ) -> Result<QueriedTargets> {
        let mut bzlmod = BzlmodRepos::default();
        let checked_version = self.version();
        let supports_bzlmod = checked_version
            .as_ref()
            .is_ok_and(|found| *found >= BazelVersion(8, 6, 0) && *found != BazelVersion(9, 0, 0));
        let mut survey = ModuleSurvey::default();
        let (bzlmod_enabled, root_mapping) =
            self.probe_query_repositories(supports_bzlmod, &mut survey);
        let version = match checked_version {
            Ok(found) => Some(found),
            Err(error) => {
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
        if bzlmod_enabled && supports_bzlmod {
            match self.query_bzlmod_repos(root_mapping, survey, &mut |target, inputs| {
                transform(std::slice::from_mut(target), inputs);
            }) {
                Ok(repos) => bzlmod = repos,
                Err(error) if self.keep_going => {
                    eprintln!("[Warn] failed to hash Bzlmod repositories: {error:#}");
                }
                Err(error) => return Err(error),
            }
        }
        let BzlmodRepos {
            targets: bzlmod_repos,
            mut failed_extensions,
            canonical_to_apparent,
            module_graph_json,
            root_mapping,
        } = bzlmod;
        let targets = self.collect_all_targets(bzlmod_repos, |expression, use_cquery| {
            self.query_with(expression, use_cquery, version, &mut |batch| {
                transform(batch, None)
            })
        })?;
        self.check_failed_extension_references(
            &targets,
            &mut failed_extensions,
            &canonical_to_apparent,
        )?;
        Ok(QueriedTargets {
            targets,
            module_graph_json,
            root_mapping,
        })
    }

    fn collect_all_targets(
        &self,
        bzlmod_repos: Vec<Target>,
        mut query: impl FnMut(&str, bool) -> Result<Vec<Target>>,
    ) -> Result<Vec<Target>> {
        let mut queries = Vec::new();
        if !self.exclude_external_targets {
            queries.push("'//external:all-targets'".to_owned());
        }
        if self.use_cquery {
            let expression = self
                .cquery_expression
                .clone()
                .unwrap_or_else(|| "deps(//...:all-targets)".to_owned());
            let mut targets = query(&self.with_exclude_filter(expression), true)?;
            if !self.exclude_external_targets {
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
                Err(error)
                    if !self.exclude_external_targets
                        && is_external_package_error(&error.to_string()) =>
                {
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
        transform: &mut impl FnMut(&mut Target, Option<RepositoryInputs>),
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
                Err(error) if self.keep_going => {
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
            .map(|json| hidden_module_repo_names(json, &canonical_to_apparent))
            .unwrap_or_default();
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
            Err(error) if self.keep_going => {
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
        let targets = lower_repositories(
            repositories,
            &canonical_to_apparent,
            &module_edges,
            // In query mode a toolchain is resolved, never declared, so the Go SDK repo behind
            // `go_sdk.download(patches = ...)` is on no target's input list and the module that
            // owns the extension stands in for it. cquery sees the resolved toolchain as a real
            // input, so there the nodes alone carry the change and the stand-in edges would only
            // widen the impacted set.
            (!self.use_cquery).then_some(&extension_repos.imported),
            &self.workspace,
            transform,
        );
        Ok(BzlmodRepos {
            targets,
            failed_extensions: failed,
            canonical_to_apparent,
            module_graph_json: module_graph,
            root_mapping: retain_root_mapping.then_some(root_mapping),
        })
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
        let mut complete_mappings =
            complete_inputs && !hidden_module_repos.is_empty() && failed.is_empty();
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
            if complete_mappings {
                module_mappings.extend(module_repos.iter().cloned().zip(mappings));
                complete_mappings = module_mappings.len() == module_repos.len();
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
        if !representatives.is_empty() {
            let names = representatives.values().cloned().collect::<Vec<_>>();
            let mappings = match self.dump_repo_mappings(&names, failed, &mut complete_mappings) {
                Ok(mappings) => mappings,
                Err(_) => {
                    complete_mappings = false;
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
        if !complete_mappings {
            module_mappings.clear();
        }
        Ok(ExtensionRepositories {
            imported,
            generated,
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

    pub fn query(&self, expression: &str, use_cquery: bool) -> Result<Vec<Target>> {
        self.query_with(expression, use_cquery, None, &mut |_| {})
    }

    fn query_with(
        &self,
        expression: &str,
        use_cquery: bool,
        version: Option<BazelVersion>,
        transform: &mut impl FnMut(&mut [Target]),
    ) -> Result<Vec<Target>> {
        let version = match version {
            Some(version) => version,
            None => self.version()?,
        };
        let compatible = if use_cquery {
            Some(self.query_compatible_labels(expression)?)
        } else {
            None
        };
        let query_file = NamedTempFile::new().context("create query file")?;
        fs::write(query_file.path(), expression).context("write query file")?;
        let output_file = NamedTempFile::new().context("create query output file")?;
        let streamed_cquery = version >= BazelVersion(7, 0, 0);
        let supports_output_file = version >= BazelVersion(8, 2, 0);

        let mut command = self.command();
        command.arg(if use_cquery { "cquery" } else { "query" });
        if use_cquery {
            command.arg("--transitions=lite");
        }
        command
            .arg("--output")
            .arg(if use_cquery && !streamed_cquery {
                "proto"
            } else {
                "streamed_proto"
            })
            .arg("--proto:instantiation_stack");
        if !use_cquery {
            command.arg("--order_output=no");
        }
        if self.keep_going {
            command.arg("--keep_going");
        }
        if use_cquery {
            command
                .args(&self.cquery_options)
                .arg("--consistent_labels");
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
        if code != 0 && !(self.keep_going && code == 3) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Bazel query failed, exit code {code}: {stderr}");
        }
        if !output.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }

        let mut targets = if use_cquery {
            decode_cquery_stream(output_file.path(), streamed_cquery, &mut |target| {
                transform(std::slice::from_mut(target));
            })?
        } else {
            decode_target_stream(output_file.path(), transform)?
        };
        if let Some(compatible) = compatible {
            targets.retain(|target| {
                target_name(target).is_some_and(|name| compatible.contains(target_label(name)))
            });
        }
        Ok(targets)
    }

    fn query_compatible_labels(&self, expression: &str) -> Result<HashSet<String>> {
        let query_file = NamedTempFile::new().context("create query file")?;
        fs::write(query_file.path(), expression)?;
        let starlark_file = NamedTempFile::new().context("create cquery formatter")?;
        fs::write(
            starlark_file.path(),
            r#"def format(target):
    if providers(target) == None:
        return ""
    if "IncompatiblePlatformProvider" not in providers(target):
        target_repr = repr(target)
        if "<alias target" in target_repr:
            return target_repr.split(" ")[2]
        return str(target.label)
    return ""
"#,
        )?;
        let mut command = self.command();
        command
            .arg("cquery")
            .arg("--output")
            .arg("starlark")
            .arg("--starlark:file")
            .arg(starlark_file.path())
            .args(&self.cquery_options)
            .arg("--consistent_labels")
            .arg("--query_file")
            .arg(query_file.path())
            .stdin(Stdio::null());
        let output = self
            .run(&mut command)
            .context("execute compatible-target cquery")?;
        if !output.status.success() {
            bail!(
                "compatible-target cquery failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect())
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
#[derive(Debug, Default, PartialEq, Eq)]
struct ExtensionRepositories {
    /// The ones some module (the root included) imports with `use_repo`: the hubs, toolchain
    /// registries and rule repos that targets name.
    imported: BTreeSet<String>,
    /// Every repository of every extension in `imported`, the imported ones included.
    generated: BTreeSet<String>,
    /// Complete module mappings from the ordinary module read, keyed by their requested canonical names.
    module_mappings: BTreeMap<String, RepoMapping>,
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
fn extension_usage_prefix(canonical: &str) -> Option<&str> {
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

/// Targets with the accepted module graph and successful unfiltered root mapping from discovery.
pub(crate) struct QueriedTargets {
    pub(crate) targets: Vec<Target>,
    pub(crate) module_graph_json: Option<String>,
    pub(crate) root_mapping: Option<Vec<u8>>,
}

/// The repositories `bazel mod` describes for a Bzlmod workspace, lowered to `//external:`
/// nodes, with what it could not describe.
#[derive(Default)]
struct BzlmodRepos {
    targets: Vec<Target>,
    /// Extension diagnostics and canonical repositories whose mappings failed.
    failed_extensions: FailedExtensions,
    /// The root module's repository mapping, canonical name -> apparent names.
    canonical_to_apparent: BTreeMap<String, Vec<String>>,
    /// The accepted module graph, shared by repository edges and hash-file metadata.
    module_graph_json: Option<String>,
    /// Unfiltered stdout from a successful initial Bzlmod probe, shared with external hashing.
    root_mapping: Option<Vec<u8>>,
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

pub(crate) type RepositoryInputs = Arc<RepositoryInputGroup>;

pub(crate) struct RepositoryInputGroup {
    pub(crate) extension: String,
    pub(crate) labels: Vec<String>,
}

fn materialize_repository_inputs_for_batch(batch: &mut [Target], inputs: Option<RepositoryInputs>) {
    if let Some(inputs) = inputs {
        for target in batch {
            materialize_repository_inputs(target, Some(inputs.clone()));
        }
    }
}

fn materialize_repository_inputs(target: &mut Target, shared: Option<RepositoryInputs>) {
    if let Some(inputs) = shared {
        target.rule.as_option_mut().unwrap().rule_input = inputs.labels.clone();
    }
}

fn lower_repositories(
    repositories: Vec<Repository>,
    canonical_to_apparent: &BTreeMap<String, Vec<String>>,
    module_edges: &BTreeMap<String, BTreeSet<String>>,
    extension_edges: Option<&BTreeSet<String>>,
    workspace: &Path,
    transform: &mut impl FnMut(&mut Target, Option<RepositoryInputs>),
) -> Vec<Target> {
    let workspace = RepositoryWorkspace::new(workspace);
    // A module the root cannot see has no apparent name there; its node is named by canonical
    // name, matching the `apparent_names` fallback below.
    let node_names = |canonical: &str| {
        canonical_to_apparent
            .get(canonical)
            .cloned()
            .unwrap_or_else(|| vec![canonical.to_owned()])
    };
    let module_to_canonical = repositories
        .iter()
        .filter_map(|repository| {
            let canonical = repository.canonical_name.as_deref()?;
            (canonical.matches('+').count() == 1)
                .then(|| (canonical.split('+').next().unwrap_or(canonical), canonical))
        })
        .map(|(module, canonical)| (module.to_owned(), canonical.to_owned()))
        .collect::<BTreeMap<_, _>>();

    // An extension's repositories are built by the owning module's extension code, from tags
    // the owning module's definition shapes, so that definition -- attributes, override patches,
    // a local override's tree -- is folded into each of them as an attribute: a patched
    // `rules_rs` is a different `@rules_rust` and `@bd_crates`, which is how a target that never
    // names `@rules_rs//` sees the patch. An attribute rather than an edge, so the query-mode
    // edges from a module to its extension repositories below do not close a cycle.
    let module_definitions = repositories
        .iter()
        .filter_map(|repository| {
            let canonical = repository.canonical_name.as_deref()?;
            (!is_extension_repo(canonical)).then(|| {
                (
                    canonical.to_owned(),
                    repository_definition_hash(repository, &workspace),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();

    // Query mode cannot see how a target reaches an extension's repositories: a toolchain is
    // resolved rather than declared, and a hub's aliases live in a BUILD file the query never
    // loads. Two conservative stand-ins keep the change reachable: the module that owns an
    // extension depends on everything it generates (every rules_go target names `@rules_go//`),
    // and each imported repository of an extension depends on the siblings nobody imports (the
    // `@bd_crates` hub reaches its `bd_crates__*` spokes). Spokes themselves stay leaves, so no
    // cycle forms.
    let mut module_extension_repos = BTreeMap::<String, BTreeSet<String>>::new();
    let mut extension_spokes = BTreeMap::<String, BTreeSet<String>>::new();
    if let Some(imported) = extension_edges {
        for repository in &repositories {
            let canonical = repository.canonical_name.as_deref().unwrap_or_default();
            let Some(extension) = extension_usage_prefix(canonical) else {
                continue;
            };
            if let Some((module_repo, _definition_hash)) =
                extension_repo_owner(extension, &module_definitions)
                    .filter(|(module_repo, _definition_hash)| !module_repo.is_empty())
            {
                module_extension_repos
                    .entry(module_repo.clone())
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
    let mut shared_groups = HashMap::<String, RepositoryInputs>::new();
    let empty = BTreeSet::new();
    for repository in repositories {
        let canonical = repository.canonical_name.as_deref().unwrap_or_default();
        let extension = extension_usage_prefix(canonical);
        let owner_definition_hash = extension
            .and_then(|extension| extension_repo_owner(extension, &module_definitions))
            .map(|(_module_repo, definition_hash)| definition_hash.as_str());
        let module = (canonical.matches('+').count() == 1)
            .then(|| canonical.split('+').next().unwrap_or(canonical));
        let mut dependencies = module
            .into_iter()
            .flat_map(|module| module_edges.get(module).into_iter().flatten())
            .filter_map(|dependency| module_to_canonical.get(dependency))
            .flat_map(|canonical| node_names(canonical))
            .collect::<BTreeSet<_>>();
        if let Some(repos) = module_extension_repos.get(canonical) {
            dependencies.extend(repos.iter().cloned());
        }
        let spokes = extension
            .filter(|_| extension_edges.is_some_and(|imported| imported.contains(canonical)))
            .and_then(|extension| extension_spokes.get_key_value(extension));
        let group = if dependencies.is_empty() {
            spokes
        } else {
            if let Some((_, names)) = spokes {
                dependencies.extend(names.iter().cloned());
            }
            None
        };
        for apparent in node_names(canonical) {
            // Imported hubs of one extension share its ordered hidden siblings. Pass
            // the same allocation to the hash traversal when no owner-specific edge
            // needs adding or removing. Raw query callers expand it into the protobuf.
            let (inputs, shared) = match group {
                Some((extension, names)) if !names.contains(&apparent) => {
                    let shared = shared_groups.entry(extension.clone()).or_insert_with(|| {
                        Arc::new(RepositoryInputGroup {
                            extension: extension.clone(),
                            labels: names
                                .iter()
                                .map(|name| format!("//external:{name}"))
                                .collect(),
                        })
                    });
                    (&empty, Some(Arc::clone(shared)))
                }
                Some((_, names)) => (names, None),
                None => (&dependencies, None),
            };
            let mut target = repository_target(
                &repository,
                &apparent,
                inputs,
                owner_definition_hash,
                &workspace,
            );
            transform(&mut target, shared);
            targets.push(target);
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

fn is_external_package_error(message: &str) -> bool {
    message.contains("no such package 'external'")
        || message.contains("//external package is not available")
}

fn deduplicate_targets(targets: Vec<Target>) -> Result<Vec<Target>> {
    let mut seen = HashSet::new();
    Ok(targets
        .into_iter()
        .filter(|target| target_name(target).is_some_and(|name| seen.insert(name.to_owned())))
        .collect())
}

pub fn decode_delimited<M: Message>(path: &Path) -> Result<Vec<M>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut messages = Vec::new();
    loop {
        if reader.fill_buf()?.is_empty() {
            break;
        }
        messages.push(buffa::DecodeOptions::new().decode_length_delimited_reader(&mut reader)?);
    }
    Ok(messages)
}

/// Upper bounds on one decode batch. Batching keeps peak memory proportional to
/// the batch rather than to the whole query output, which for a large workspace
/// is gigabytes.
const DECODE_BATCH_MESSAGES: usize = 8192;
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
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return usize::try_from(value)
                .map(Some)
                .map_err(|_| anyhow!("length prefix {value} does not fit in usize"));
        }
        shift += 7;
        if shift >= 64 {
            bail!("length prefix in Bazel query output overflows a u64");
        }
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
        batch.buffer.reserve(length);
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

fn decode_target_stream(
    path: &Path,
    transform: &mut impl FnMut(&mut [Target]),
) -> Result<Vec<Target>> {
    let mut reader = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut targets = Vec::new();
    let mut batch = RawBatch::default();
    let mut decoded = Vec::new();
    while read_raw_batch(&mut reader, &mut batch)? {
        batch
            .slices()
            .map(|bytes| {
                <Target as Message>::decode_from_slice(bytes)
                    .map_err(anyhow::Error::from)
                    .map(lower_target)
            })
            .collect_into_vec(&mut decoded);
        let start = targets.len();
        targets.reserve(decoded.len());
        for lowered in decoded.drain(..) {
            if let Some(target) = lowered? {
                targets.push(target);
            }
        }
        transform(&mut targets[start..]);
    }
    Ok(targets)
}

fn decode_cquery_stream(
    path: &Path,
    streamed: bool,
    transform: &mut impl FnMut(&mut Target),
) -> Result<Vec<Target>> {
    let mut decoder = CqueryDecoder::default();
    if streamed {
        let mut reader = BufReader::new(File::open(path)?);
        loop {
            if reader.fill_buf()?.is_empty() {
                break;
            }
            let result: CqueryResult =
                buffa::DecodeOptions::new().decode_length_delimited_reader(&mut reader)?;
            decoder.push(result, transform);
        }
    } else {
        let bytes = fs::read(path)?;
        decoder.push(CqueryResult::decode_from_slice(&bytes)?, transform);
    }
    Ok(decoder.finish(transform))
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

/// Bazel reports the null configuration (source files, package groups) with this checksum.
const NULL_CONFIGURATION_CHECKSUM: &str = "null";

/// Decodes `CqueryResult` messages into targets named by their configured identity.
///
/// Each result carries its configuration inline (the deprecated `configuration` field, which
/// Bazel still populates) and by id into `CqueryResult.configurations`. A streamed result's
/// id table only arrives after the results that use it, so a result whose inline checksum is
/// missing waits until the stream ends to be named.
#[derive(Default)]
struct CqueryDecoder {
    targets: Vec<Target>,
    configurations: HashMap<u32, String>,
    pending: Vec<(Target, u32)>,
}

impl CqueryDecoder {
    fn push(&mut self, result: CqueryResult, transform: &mut impl FnMut(&mut Target)) {
        for configuration in result.configurations {
            if let Some(checksum) = non_null_checksum(&configuration.checksum) {
                self.configurations
                    .insert(configuration.id, checksum.to_owned());
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
            let inline = configuration
                .as_option()
                .and_then(|configuration| non_null_checksum(&configuration.checksum))
                .map(str::to_owned);
            match inline.or_else(|| self.configurations.get(&configuration_id).cloned()) {
                Some(checksum) => self.accept(target, Some(&checksum), transform),
                None if configuration_id != 0 => self.pending.push((target, configuration_id)),
                None => self.accept(target, None, transform),
            }
        }
    }

    fn accept(
        &mut self,
        target: Target,
        checksum: Option<&str>,
        transform: &mut impl FnMut(&mut Target),
    ) {
        let Some(mut target) = lower_target(target) else {
            return;
        };
        if let Some(checksum) = checksum {
            configure_target(&mut target, checksum);
        }
        transform(&mut target);
        self.targets.push(target);
    }

    fn finish(mut self, transform: &mut impl FnMut(&mut Target)) -> Vec<Target> {
        let pending = std::mem::take(&mut self.pending);
        for (target, configuration_id) in pending {
            let checksum = self.configurations.get(&configuration_id).cloned();
            self.accept(target, checksum.as_deref(), transform);
        }
        self.targets
    }
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

fn lower_target(mut target: Target) -> Option<Target> {
    match target.r#type {
        target::Discriminator::Rule
        | target::Discriminator::SourceFile
        | target::Discriminator::GeneratedFile => {
            compact_target(&mut target);
            Some(target)
        }
        target::Discriminator::PackageGroup => {
            let group = target.package_group.take()?;
            target.r#type = target::Discriminator::Rule;
            target.rule = buffa::MessageField::some(package_group_rule(&group));
            compact_target(&mut target);
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
            rule.rule_class_key = None;
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
            source.package_contains_errors = None;
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
    rule
}

fn repository_target(
    repository: &Repository,
    apparent: &str,
    dependencies: &BTreeSet<String>,
    owner_definition_hash: Option<&str>,
    workspace: &RepositoryWorkspace<'_>,
) -> Target {
    let mut rule = Rule {
        name: format!("//external:{apparent}"),
        rule_class: repository
            .repo_rule_name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "bzlmod_repo".to_owned()),
        attribute: workspace_relative_attributes(&repository.attribute, &workspace.roots),
        ..Default::default()
    };
    let mut push_attribute = |name: &str, value: String| {
        let mut attribute = Attribute::default();
        attribute.name = name.to_owned();
        attribute.r#type = crate::proto::blaze_query::attribute::Discriminator::String;
        attribute.string_value = Some(value);
        rule.attribute.push(attribute);
    };
    if let Some(content_hash) = local_repository_content_hash(repository, workspace.path) {
        push_attribute("_bazel_diff_content_hash", content_hash);
    }
    if let Some(inputs_hash) = repository_local_inputs_hash(repository, workspace.path) {
        push_attribute("_bazel_diff_local_inputs_hash", inputs_hash);
    }
    if let Some(owner_hash) = owner_definition_hash {
        push_attribute("_bazel_diff_module_hash", owner_hash.to_owned());
    }
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
    repository
        .attribute
        .iter()
        .flat_map(|attribute| {
            attribute
                .string_value
                .iter()
                .chain(&attribute.string_list_value)
                .chain(attribute.string_dict_value.iter().map(|entry| &entry.value))
                .chain(
                    attribute
                        .label_dict_unary_value
                        .iter()
                        .flat_map(|entry| [&entry.key, &entry.value]),
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
                .chain(
                    attribute
                        .string_list_dict_value
                        .iter()
                        .flat_map(|entry| &entry.value),
                )
        })
        .map(String::as_str)
        .filter(|value| crate::hash::main_repo_path(value).is_some())
        .collect()
}

/// Digest of a repository's own definition: its rule, its attributes, and the local tree and
/// main-repository files behind them. Like the target hasher, it leaves out URLs whose content an
/// integrity pins: `url`/`urls` under `sha256`/`integrity`, and the URLs
/// `without_integrity_pinned_urls` drops.
fn repository_definition_hash(
    repository: &Repository,
    workspace: &RepositoryWorkspace<'_>,
) -> String {
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
    let attributes = workspace_relative_attributes(&repository.attribute, &workspace.roots);
    for attribute in &attributes {
        if pinned && matches!(attribute.name.as_str(), "url" | "urls") {
            continue;
        }
        encoded.clear();
        match crate::hash::without_integrity_pinned_urls(attribute, &attributes) {
            Some(stripped) => stripped.encode_length_delimited(&mut encoded),
            None => attribute.encode_length_delimited(&mut encoded),
        }
        hasher.update(&encoded);
    }
    hasher.update([0]);
    if let Some(content_hash) = local_repository_content_hash(repository, workspace.path) {
        hasher.update(content_hash);
    }
    hasher.update([0]);
    if let Some(inputs_hash) = repository_local_inputs_hash(repository, workspace.path) {
        hasher.update(inputs_hash);
    }
    hex::encode(hasher.finalize())
}

/// Hashes the contents of the main-repository files named by `repository`'s attributes, in label
/// order. `None` when the attributes name no such file that exists in the workspace.
fn repository_local_inputs_hash(repository: &Repository, workspace: &Path) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut hashed_any = false;
    for label in repository_local_input_labels(repository) {
        let Some(path) = crate::hash::main_repo_path(label) else {
            continue;
        };
        let absolute = workspace.join(path);
        let mut content = Sha256::new();
        if absolute.is_file() {
            content.update(fs::read(&absolute).ok()?);
        } else if absolute.is_dir() {
            crate::hash::hash_directory_into(&mut content, &absolute).ok()?;
        } else {
            continue;
        }
        hasher.update(label.as_bytes());
        hasher.update([0]);
        hasher.update(content.finalize());
        hasher.update([0]);
        hashed_any = true;
    }
    hashed_any.then(|| hex::encode(hasher.finalize()))
}

fn local_repository_content_hash(repository: &Repository, workspace: &Path) -> Option<String> {
    if repository.repo_rule_name.as_deref() != Some("local_repository") {
        return None;
    }
    let path = repository
        .attribute
        .iter()
        .find(|attribute| attribute.name == "path")
        .and_then(|attribute| attribute.string_value.as_deref())?;
    let path = PathBuf::from(path);
    let root = if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    };
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
}

impl<'a> RepositoryWorkspace<'a> {
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            roots: workspace_roots(path),
        }
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
fn workspace_relative_attributes(attributes: &[Attribute], roots: &[String]) -> Vec<Attribute> {
    let rewrite = |value: &mut String| {
        if let Cow::Owned(relative) = workspace_relative(value, roots) {
            *value = relative;
        }
    };
    let mut attributes = attributes.to_vec();
    for attribute in &mut attributes {
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
    attributes
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
        .filter(|(_, name)| !visible_modules.contains(name.as_str()))
        .map(|(key, name)| {
            let version = key.rsplit_once('@').map_or("", |(_, version)| version);
            let canonical = if WELL_KNOWN_MODULES.contains(&name.as_str()) {
                name.clone()
            } else if versions_by_name.get(name.as_str()).copied().unwrap_or(0) > 1 {
                format!("{name}+{version}")
            } else {
                format!("{name}+")
            };
            (key.clone(), canonical)
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
    use crate::proto::analysis::{Configuration, ConfiguredTarget, CqueryResult};
    use crate::proto::blaze_query::{
        attribute, EnvironmentGroup, GeneratedFile, LabelDictUnaryEntry, SourceFile,
    };
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
        let targets = vec![
            rule_target("//a:a"),
            rule_target("//a:a"),
            rule_target("//b:b"),
        ];
        let targets = deduplicate_targets(targets).unwrap();
        let names = targets.iter().filter_map(target_name).collect::<Vec<_>>();
        assert_eq!(names, ["//a:a", "//b:b"]);
        assert!(is_external_package_error("no such package 'external'"));
        assert!(is_external_package_error(
            "//external package is not available"
        ));
        assert!(!is_external_package_error("another error"));
    }

    #[test]
    fn decodes_query_and_cquery_streams() {
        let directory = tempfile::tempdir().unwrap();
        let query_path = directory.path().join("query.pb");
        write_delimited(&query_path, &[rule_target("//a:a"), rule_target("//b:b")]);
        let mut seen = Vec::new();
        let targets = decode_target_stream(&query_path, &mut |batch| {
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
            target_name(&decode_cquery_stream(&streamed, true, &mut |_| {}).unwrap()[0]),
            Some("//c:c")
        );

        let non_streamed = directory.path().join("cquery.pb");
        fs::write(&non_streamed, result.encode_to_vec()).unwrap();
        assert_eq!(
            target_name(&decode_cquery_stream(&non_streamed, false, &mut |_| {}).unwrap()[0]),
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
    }

    #[test]
    fn decode_target_stream_preserves_order_across_batches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("query.pb");
        let written = (0..DECODE_BATCH_MESSAGES + 5)
            .map(|index| rule_target(&format!("//pkg:target{index}")))
            .collect::<Vec<_>>();
        write_delimited(&path, &written);
        let mut seen = Vec::new();
        let decoded = decode_target_stream(&path, &mut |batch| {
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
    fn decode_target_stream_rejects_truncated_message() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("truncated.pb");
        // A length prefix of 16 followed by only two bytes of payload.
        fs::write(&path, [0x10u8, 0x00, 0x00]).unwrap();
        assert!(decode_target_stream(&path, &mut |_| {}).is_err());
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
            let target = repository_target(
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
            repository_definition_hash(
                &crate_repository(first.path()),
                &RepositoryWorkspace::new(first.path())
            ),
            repository_definition_hash(
                &crate_repository(second.path()),
                &RepositoryWorkspace::new(second.path())
            )
        );
        // A path outside the workspace keeps its spelling.
        let relative = workspace_relative_attributes(
            &crate_repository(first.path()).attribute,
            &workspace_roots(first.path()),
        );
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

        let target = repository_target(
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
        let baseline = repository_local_inputs_hash(&repository, workspace.path()).unwrap();
        assert_eq!(
            repository_local_inputs_hash(&repository, workspace.path()).unwrap(),
            baseline
        );

        // Patch content, the build file, and the overlay directory all reach the hash.
        fs::write(workspace.path().join("dep.patch"), "patch v2").unwrap();
        let patched = repository_local_inputs_hash(&repository, workspace.path()).unwrap();
        assert_ne!(patched, baseline);
        fs::write(workspace.path().join("third_party/dep.BUILD"), "build v2").unwrap();
        let rebuilt = repository_local_inputs_hash(&repository, workspace.path()).unwrap();
        assert_ne!(rebuilt, patched);
        fs::write(
            workspace.path().join("third_party/overlay/BUILD.bazel"),
            "overlay v2",
        )
        .unwrap();
        assert_ne!(
            repository_local_inputs_hash(&repository, workspace.path()).unwrap(),
            rebuilt
        );

        let target = repository_target(
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
        assert!(repository_local_inputs_hash(&remote_only, workspace.path()).is_none());
        let target = repository_target(
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
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
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
            keep_going: false,
            fine_grained_external_repos: BTreeSet::from(["@repo".to_owned()]),
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
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            exclude_external_targets: false,
            exclude_targets_query: None,
            no_bazelrc: false,
            verbose: false,
            cancellation: None,
        };
        let mut queries = Vec::new();
        let targets = options
            .collect_all_targets(Vec::new(), |expression, cquery| {
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
            &edges,
            None,
            Path::new("."),
            &mut |target, _| transformed.push(target_name(target).unwrap().to_owned()),
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
                &BTreeMap::new(),
                None,
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
            &parse_module_dependency_edges(graph),
            None,
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
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
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
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
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
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
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
              {"key":"leaf@1.0","name":"leaf","dependencies":[]}]}
          ]
        }"#;
        assert_eq!(
            hidden_module_repo_names(graph, &mapping),
            BTreeMap::from([
                ("bazel_tools@_".to_owned(), "bazel_tools".to_owned()),
                ("leaf@1.0".to_owned(), "leaf+".to_owned()),
                ("multi@1.0".to_owned(), "multi+1.0".to_owned()),
                ("multi@2.0".to_owned(), "multi+2.0".to_owned()),
            ])
        );
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
            &edges,
            Some(&imported),
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

        let cquery = lower_repositories(
            repositories(),
            &mapping,
            &edges,
            None,
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
                &BTreeMap::new(),
                None,
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
            repository_definition_hash(&pinned("https://a/x.zip"), &paths),
            repository_definition_hash(&pinned("https://b/x.zip"), &paths)
        );
        let mut unpinned = pinned("https://a/x.zip");
        unpinned.attribute.pop();
        let mut moved = pinned("https://b/x.zip");
        moved.attribute.pop();
        assert_ne!(
            repository_definition_hash(&unpinned, &paths),
            repository_definition_hash(&moved, &paths)
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
            repository_definition_hash(&patched("/one", "sha256-p"), &paths),
            repository_definition_hash(&patched("/two", "sha256-p"), &paths)
        );
        assert_ne!(
            repository_definition_hash(&patched("/one", "sha256-p"), &paths),
            repository_definition_hash(&patched("/one", "sha256-q"), &paths)
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
        fs::write(&script, body).unwrap();
        script
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
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
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
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
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
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
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
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
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
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
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
        let targets = decode_cquery_stream(&streamed, true, &mut |target| {
            seen.push(target_name(target).unwrap().to_owned())
        })
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
        let targets = decode_cquery_stream(&non_streamed, false, &mut |_| {}).unwrap();
        assert_eq!(target_name(&targets[0]), Some("//a:a|def456"));
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
    /// than 8.2); `query` copies `query_proto` the same way and exits with `query_exit`,
    /// reporting on stderr when that is non-zero. Every invocation is appended to `calls.log`.
    #[cfg(unix)]
    fn query_script(
        workspace: &Path,
        version: &str,
        compatible: &[&str],
        query_exit: i32,
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
    printf '{compatible}'
    exit 0
  fi
  if echo "$*" | grep -q -- "--output proto"; then
    copy_or_print "{proto}" "$@"
  else
    copy_or_print "{streamed}" "$@"
  fi
  exit 0
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
            compatible = compatible
                .iter()
                .map(|label| format!("{label}\n"))
                .collect::<String>()
                .replacen('\n', "\n\n", 1),
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
            let script = query_script(workspace.path(), version, &["//app:lib", "//app:tool"], 0);
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
                compatible.starts_with("cquery --output starlark --starlark:file "),
                "{compatible}"
            );
            assert!(
                compatible.contains("--define=flavor=test --consistent_labels --query_file"),
                "{compatible}"
            );
            assert!(!compatible.contains("--query-only"), "{compatible}");
            let streamed = &calls[2];
            assert!(
                streamed.starts_with(&format!(
                    "cquery --transitions=lite --output {output} --proto:instantiation_stack --define=flavor=test --consistent_labels --query_file "
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

    /// A failed compatible-target cquery stops the query instead of silently keeping every
    /// target, and a plain `query` that exits non-zero is an error that quotes Bazel's stderr --
    /// unless `--keep_going` was asked for and Bazel exited 3, the code it uses for a partial
    /// answer, in which case the output it did produce is used.
    #[test]
    #[cfg(unix)]
    fn query_surfaces_bazel_failures_and_tolerates_partial_answers_with_keep_going() {
        let workspace = tempfile::tempdir().unwrap();
        let script = query_script(workspace.path(), "8.7.0", &[], 1);
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
        let script = query_script(partial.path(), "8.7.0", &[], 3);
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
