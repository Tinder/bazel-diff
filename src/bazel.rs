use crate::proto::analysis::CqueryResult;
use crate::proto::blaze_query::{
    target, Attribute, ConfiguredRuleInput, PackageGroup, Repository, Rule, Target,
};
use anyhow::{anyhow, bail, Context, Result};
use buffa::Message;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

    fn run_capture(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut command = self.command();
        command.args(args).stdin(Stdio::null());
        if self.verbose {
            eprintln!("[Info] Command: {command:?}");
        }
        command
            .output()
            .with_context(|| format!("failed to execute {}", self.bazel.display()))
    }

    pub fn is_bzlmod_enabled(&self) -> bool {
        self.run_capture(&["mod", "graph"])
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    pub fn module_graph_json(&self) -> Option<String> {
        let output = self.run_capture(&["mod", "graph", "--output=json"]).ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn version(&self) -> Result<BazelVersion> {
        let output = self.run_capture(&["version"])?;
        if !output.status.success() {
            bail!("Bazel version command failed, exit code {}", output.status);
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let version = text
            .lines()
            .find_map(|line| {
                line.strip_prefix("Build label: ")
                    .or_else(|| line.strip_prefix("bazel "))
            })
            .ok_or_else(|| anyhow!("Bazel version command returned unexpected output: {text}"))?;
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
        Ok(BazelVersion(
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        ))
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
        self.query_all_targets_with(|_| {})
    }

    pub(crate) fn query_all_targets_with(
        &self,
        mut transform: impl FnMut(&mut Target),
    ) -> Result<Vec<Target>> {
        let bzlmod_repos = if self.is_bzlmod_enabled()
            && self.version().is_ok_and(|version| {
                version >= BazelVersion(8, 6, 0) && version != BazelVersion(9, 0, 0)
            }) {
            self.query_bzlmod_repos(&mut transform)
                .unwrap_or_else(|error| {
                    eprintln!("[Warn] failed to hash Bzlmod repositories: {error:#}");
                    Vec::new()
                })
        } else {
            Vec::new()
        };
        let mut queries = Vec::new();
        if !self.exclude_external_targets {
            queries.push("'//external:all-targets'".to_owned());
        }
        if self.use_cquery {
            let expression = self
                .cquery_expression
                .clone()
                .unwrap_or_else(|| "deps(//...:all-targets)".to_owned());
            let mut targets =
                self.query_with(&self.with_exclude_filter(expression), true, &mut transform)?;
            if !self.exclude_external_targets {
                match self.query_with("'//external:all-targets'", false, &mut transform) {
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
            match self.query_with(&full, false, &mut transform) {
                Ok(mut targets) => {
                    targets.extend(bzlmod_repos.clone());
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
                    let mut targets = self.query_with(&without_external, false, &mut transform)?;
                    targets.extend(bzlmod_repos);
                    deduplicate_targets(targets)
                }
                Err(error) => Err(error),
            }
        }
    }

    fn query_bzlmod_repos(&self, transform: &mut impl FnMut(&mut Target)) -> Result<Vec<Target>> {
        let mapping_output = self.run_capture(&["mod", "dump_repo_mapping", ""])?;
        if !mapping_output.status.success() {
            bail!("bazel mod dump_repo_mapping failed");
        }
        let mut canonical_to_apparent = BTreeMap::<String, Vec<String>>::new();
        for line in String::from_utf8_lossy(&mapping_output.stdout).lines() {
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
        let canonical_names = canonical_to_apparent
            .keys()
            .filter(|name| name.contains('+') || name.contains('~'))
            .map(|name| format!("@@{name}"))
            .collect::<Vec<_>>();
        if canonical_names.is_empty() {
            return Ok(Vec::new());
        }
        let output_file = NamedTempFile::new().context("create mod show_repo output")?;
        let mut command = self.command();
        command.arg("mod").arg("show_repo").args(&canonical_names);
        command
            .arg("--output=streamed_proto")
            .stdout(File::create(output_file.path())?)
            .stdin(Stdio::null())
            .stderr(Stdio::piped());
        let output = command.output().context("execute bazel mod show_repo")?;
        if !output.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        if !output.status.success() {
            bail!("bazel mod show_repo failed with {}", output.status);
        }
        let repositories = decode_delimited::<Repository>(output_file.path())?;
        let module_edges = self
            .module_graph_json()
            .as_deref()
            .map(parse_module_dependency_edges)
            .unwrap_or_default();
        let module_to_canonical = repositories
            .iter()
            .filter_map(|repository| {
                let canonical = repository.canonical_name.as_deref()?;
                (canonical.matches('+').count() == 1)
                    .then(|| (canonical.split('+').next().unwrap_or(canonical), canonical))
            })
            .map(|(module, canonical)| (module.to_owned(), canonical.to_owned()))
            .collect::<BTreeMap<_, _>>();

        let mut targets = Vec::new();
        for repository in repositories {
            let canonical = repository.canonical_name.as_deref().unwrap_or_default();
            let module = (canonical.matches('+').count() == 1)
                .then(|| canonical.split('+').next().unwrap_or(canonical));
            let dep_apparent = module
                .into_iter()
                .flat_map(|module| module_edges.get(module).into_iter().flatten())
                .filter_map(|dependency| module_to_canonical.get(dependency))
                .flat_map(|canonical| canonical_to_apparent.get(canonical).into_iter().flatten())
                .cloned()
                .collect::<BTreeSet<_>>();
            let apparent_names = canonical_to_apparent
                .get(canonical)
                .cloned()
                .unwrap_or_else(|| vec![canonical.to_owned()]);
            for apparent in apparent_names {
                let mut target =
                    repository_target(&repository, &apparent, &dep_apparent, &self.workspace);
                transform(&mut target);
                targets.push(target);
            }
        }
        Ok(targets)
    }

    pub fn query(&self, expression: &str, use_cquery: bool) -> Result<Vec<Target>> {
        self.query_with(expression, use_cquery, &mut |_| {})
    }

    fn query_with(
        &self,
        expression: &str,
        use_cquery: bool,
        transform: &mut impl FnMut(&mut Target),
    ) -> Result<Vec<Target>> {
        let version = self.version()?;
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
        let output = command.output().context("failed to execute Bazel query")?;
        if !output.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        let code = output.status.code().unwrap_or(-1);
        if code != 0 && !(self.keep_going && code == 3) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Bazel query failed, exit code {code}: {stderr}");
        }

        let mut targets = if use_cquery {
            decode_cquery_stream(output_file.path(), streamed_cquery, transform)?
        } else {
            decode_target_stream(output_file.path(), transform)?
        };
        if let Some(compatible) = compatible {
            targets
                .retain(|target| target_name(target).is_some_and(|name| compatible.contains(name)));
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
        let output = command
            .output()
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

fn decode_target_stream(
    path: &Path,
    transform: &mut impl FnMut(&mut Target),
) -> Result<Vec<Target>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut targets = Vec::new();
    loop {
        if reader.fill_buf()?.is_empty() {
            break;
        }
        let target = buffa::DecodeOptions::new().decode_length_delimited_reader(&mut reader)?;
        if let Some(mut target) = lower_target(target) {
            transform(&mut target);
            targets.push(target);
        }
    }
    Ok(targets)
}

fn decode_cquery_stream(
    path: &Path,
    streamed: bool,
    transform: &mut impl FnMut(&mut Target),
) -> Result<Vec<Target>> {
    let results = if streamed {
        let mut reader = BufReader::new(File::open(path)?);
        let mut targets = Vec::new();
        loop {
            if reader.fill_buf()?.is_empty() {
                break;
            }
            let result: CqueryResult =
                buffa::DecodeOptions::new().decode_length_delimited_reader(&mut reader)?;
            for mut target in result
                .results
                .into_iter()
                .filter_map(|configured| configured.target.into_option())
                .filter_map(lower_target)
            {
                transform(&mut target);
                targets.push(target);
            }
        }
        return Ok(targets);
    } else {
        let bytes = fs::read(path)?;
        vec![CqueryResult::decode_from_slice(&bytes)?]
    };
    let mut targets = results
        .into_iter()
        .flat_map(|result| result.results)
        .filter_map(|configured| configured.target.into_option())
        .filter_map(lower_target)
        .collect::<Vec<_>>();
    for target in &mut targets {
        transform(target);
    }
    Ok(targets)
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
    workspace: &Path,
) -> Target {
    let mut rule = Rule {
        name: format!("//external:{apparent}"),
        rule_class: repository
            .repo_rule_name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "bzlmod_repo".to_owned()),
        attribute: repository.attribute.clone(),
        ..Default::default()
    };
    if let Some(content_hash) = local_repository_content_hash(repository, workspace) {
        let mut attribute = Attribute::default();
        attribute.name = "_bazel_diff_content_hash".to_owned();
        attribute.r#type = crate::proto::blaze_query::attribute::Discriminator::String;
        attribute.string_value = Some(content_hash);
        rule.attribute.push(attribute);
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
            format!("{}|{checksum}", input.label.as_deref().unwrap_or_default())
        }
        _ => input.label.clone().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::blaze_query::{attribute, SourceFile};

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
    }
}
