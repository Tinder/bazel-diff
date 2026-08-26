use crate::hash::{generate_hashes, HashOptions};
use crate::model::{
    filter_and_sort_labels, impacted_targets, impacted_targets_with_distances, HashFileData,
};
use crate::module_graph::impacted_with_module_changes;
use anyhow::{anyhow, bail, Context, Result};
use s3::creds::Credentials;
use s3::error::S3Error;
use s3::{Bucket, Region};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use url::form_urlencoded;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub hash_options: HashOptions,
    pub git_path: PathBuf,
    pub port: u16,
    pub request_timeout: Duration,
    pub cache_dir: PathBuf,
    pub track_deps: bool,
    pub no_initial_fetch: bool,
    pub warmup_revisions: Vec<String>,
    pub cache_max_age: Option<Duration>,
    pub cache_max_entries: Option<usize>,
    pub cache_max_size: Option<u64>,
    pub cache_prune_interval: Duration,
    pub remote_cache: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_prefix: String,
    pub s3_region: Option<String>,
    pub s3_endpoint: Option<String>,
    pub s3_force_path_style: bool,
}

struct State {
    config: ServerConfig,
    ready: AtomicBool,
    workspace_lock: Mutex<()>,
    started: Instant,
    fingerprint: String,
    remote: Option<RemoteCache>,
}

struct RemoteCache {
    bucket: Box<Bucket>,
    prefix: String,
}

impl RemoteCache {
    fn new(config: &ServerConfig) -> Result<Option<Self>> {
        let Some(bucket_name) = &config.s3_bucket else {
            return Ok(None);
        };
        let region_name = config
            .s3_region
            .clone()
            .or_else(|| std::env::var("AWS_REGION").ok())
            .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
            .unwrap_or_else(|| "us-east-1".to_owned());
        let region = match &config.s3_endpoint {
            Some(endpoint) => Region::Custom {
                region: region_name,
                endpoint: endpoint.clone(),
            },
            None => region_name.parse().context("invalid S3 region")?,
        };
        let credentials = Credentials::default().context("failed to resolve AWS credentials")?;
        let mut bucket =
            Bucket::new(bucket_name, region, credentials).context("failed to create S3 client")?;
        if config.s3_force_path_style {
            bucket = bucket.with_path_style();
        }
        Ok(Some(Self {
            bucket,
            prefix: normalize_s3_prefix(&config.s3_prefix),
        }))
    }

    fn object_key(&self, key: &str) -> String {
        format!("{}{key}.json", self.prefix)
    }

    fn get(&self, key: &str) -> Option<Vec<u8>> {
        match self.bucket.get_object(self.object_key(key)) {
            Ok(response) if response.status_code() == 200 => Some(response.to_vec()),
            Ok(_) => None,
            Err(error) if is_s3_not_found(&error) => None,
            Err(error) => {
                eprintln!(
                    "[Warn] S3 cache read of {} failed (treating as a miss): {error}",
                    self.object_key(key)
                );
                None
            }
        }
    }

    fn put(&self, key: &str, data: &[u8]) {
        if let Err(error) = self.bucket.put_object(self.object_key(key), data) {
            eprintln!(
                "[Warn] S3 cache write of {} failed (entry not shared): {error}",
                self.object_key(key)
            );
        }
    }

    fn contains(&self, key: &str) -> bool {
        match self.bucket.head_object(self.object_key(key)) {
            Ok((_, status)) => status == 200,
            Err(error) if is_s3_not_found(&error) => false,
            Err(error) => {
                eprintln!(
                    "[Warn] S3 cache check of {} failed (treating as a miss): {error}",
                    self.object_key(key)
                );
                false
            }
        }
    }
}

fn is_s3_not_found(error: &S3Error) -> bool {
    matches!(error, S3Error::HttpFailWithBody(404, _))
}

fn normalize_s3_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryBody {
    from: Option<String>,
    to: Option<String>,
    target_type: Option<Vec<String>>,
    modified_filepaths: Option<Vec<String>>,
    profile: Option<bool>,
}

#[derive(Debug)]
struct QueryInputs {
    from: String,
    to: String,
    target_types: Option<HashSet<String>>,
    modified_filepaths: BTreeSet<PathBuf>,
    profile: bool,
}

#[derive(Clone, Copy, Debug)]
enum QueryKind {
    Labels,
    Distances,
    DepEdges,
}

pub fn parse_duration(value: &str) -> Result<Duration> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        bail!("invalid duration '{value}'");
    }
    let mut seconds = 0u64;
    let mut digits = String::new();
    for character in normalized.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        if digits.is_empty() {
            bail!("invalid duration '{value}'");
        }
        let amount = digits.parse::<u64>()?;
        digits.clear();
        seconds = seconds
            .checked_add(match character {
                'd' => amount.saturating_mul(86_400),
                'h' => amount.saturating_mul(3_600),
                'm' => amount.saturating_mul(60),
                's' => amount,
                _ => bail!("invalid duration unit in '{value}'"),
            })
            .ok_or_else(|| anyhow!("duration overflow: {value}"))?;
    }
    if !digits.is_empty() {
        bail!("invalid duration '{value}'");
    }
    Ok(Duration::from_secs(seconds))
}

pub fn parse_byte_size(value: &str) -> Result<u64> {
    let normalized = value
        .trim()
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    let digits = normalized
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty() {
        bail!("invalid size '{value}'");
    }
    let unit = &normalized[digits.len()..];
    let multiplier = match unit {
        "" | "b" => 1,
        "k" | "kb" => 1024,
        "m" | "mb" => 1024u64.pow(2),
        "g" | "gb" => 1024u64.pow(3),
        "t" | "tb" => 1024u64.pow(4),
        _ => bail!("invalid size unit in '{value}'"),
    };
    digits
        .parse::<u64>()?
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("size overflow: {value}"))
}

pub fn serve(config: ServerConfig) -> Result<()> {
    fs::create_dir_all(&config.cache_dir)?;
    let fingerprint = configuration_fingerprint(&config);
    let remote = RemoteCache::new(&config)?;
    let state = Arc::new(State {
        config,
        ready: AtomicBool::new(false),
        workspace_lock: Mutex::new(()),
        started: Instant::now(),
        fingerprint,
        remote,
    });
    prune_cache(&state)?;
    start_cache_pruner(&state)?;
    if !state.config.no_initial_fetch {
        git(
            &state,
            &[
                String::from("fetch"),
                String::from("--all"),
                String::from("--tags"),
            ],
        )?;
    }
    for revision in &state.config.warmup_revisions {
        if let Err(error) = warm_revision(&state, revision) {
            eprintln!("[Warn] failed to warm revision {revision}: {error:#}");
        }
    }
    state.ready.store(true, Ordering::Release);
    let server = Server::http(("0.0.0.0", state.config.port))
        .map_err(|error| anyhow!("failed to bind HTTP server: {error}"))?;
    eprintln!(
        "[Info] bazel-diff query service listening on port {}",
        state.config.port
    );
    for request in server.incoming_requests() {
        let state = Arc::clone(&state);
        std::thread::spawn(move || handle_request(request, state));
    }
    Ok(())
}

fn start_cache_pruner(state: &Arc<State>) -> Result<()> {
    if state.config.cache_max_age.is_none()
        && state.config.cache_max_entries.is_none()
        && state.config.cache_max_size.is_none()
    {
        return Ok(());
    }
    let interval = state
        .config
        .cache_prune_interval
        .max(Duration::from_secs(1));
    let state = Arc::clone(state);
    std::thread::Builder::new()
        .name("bazel-diff-cache-pruner".to_owned())
        .spawn(move || loop {
            std::thread::sleep(interval);
            if let Err(error) = prune_cache(&state) {
                eprintln!("[Warn] cache prune pass failed; retrying next interval: {error:#}");
            }
        })
        .context("failed to start cache pruner")?;
    Ok(())
}

fn handle_request(request: Request, state: Arc<State>) {
    if let Err(error) = route(request, &state) {
        eprintln!("[Error] failed to handle HTTP request: {error:#}");
    }
}

fn route(mut request: Request, state: &Arc<State>) -> Result<()> {
    let raw_url = request.url().to_owned();
    let (path, query) = raw_url.split_once('?').unwrap_or((&raw_url, ""));
    match path {
        "/health" => {
            let ready = state.ready.load(Ordering::Acquire);
            respond_text(
                request,
                if ready { 200 } else { 503 },
                if ready { "OK\n" } else { "NOT_READY\n" },
            )
        }
        "/metrics" => {
            if request.method() != &Method::Get {
                return respond_json(
                    request,
                    405,
                    &json!({"error": "method not allowed, use GET"}),
                );
            }
            respond_json(request, 200, &metrics(state))
        }
        "/impacted_targets" | "/impacted_targets_with_distances" | "/dependency_edges" => {
            if !state.ready.load(Ordering::Acquire) {
                return respond_json(request, 503, &json!({"error": "service not ready"}));
            }
            if request.method() != &Method::Get && request.method() != &Method::Post {
                return respond_json(
                    request,
                    405,
                    &json!({"error": "method not allowed, use GET or POST"}),
                );
            }
            let inputs = match parse_query_inputs(&mut request, query) {
                Ok(inputs) => inputs,
                Err(error) => {
                    return respond_json(request, 400, &json!({"error": error.to_string()}))
                }
            };
            let kind = match path {
                "/dependency_edges" => QueryKind::DepEdges,
                "/impacted_targets_with_distances" => QueryKind::Distances,
                _ => QueryKind::Labels,
            };
            if matches!(kind, QueryKind::Distances | QueryKind::DepEdges)
                && !state.config.track_deps
            {
                let unavailable = match kind {
                    QueryKind::DepEdges => "dependency edges unavailable",
                    _ => "distances unavailable",
                };
                return respond_json(
                    request,
                    400,
                    &json!({"error": format!("{unavailable}: server started without --trackDeps")}),
                );
            }
            let timeout = state.config.request_timeout;
            let state_for_compute = Arc::clone(state);
            let (sender, receiver) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let _ = sender.send(compute_query(&state_for_compute, inputs, kind));
            });
            let computed = if timeout.is_zero() {
                receiver
                    .recv()
                    .map_err(|_| anyhow!("query worker stopped"))?
            } else {
                match receiver.recv_timeout(timeout) {
                    Ok(result) => result,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        return respond_json(
                            request,
                            504,
                            &json!({"error": format!("request timed out after {}s", timeout.as_secs())}),
                        )
                    }
                    Err(_) => Err(anyhow!("query worker stopped")),
                }
            };
            match computed {
                Ok(value) => respond_json(request, 200, &value),
                Err(error) => respond_json(request, 400, &json!({"error": error.to_string()})),
            }
        }
        _ => respond_json(request, 404, &json!({"error": "not found"})),
    }
}

fn parse_query_inputs(request: &mut Request, query: &str) -> Result<QueryInputs> {
    if request.method() == &Method::Post {
        let mut raw = String::new();
        request.as_reader().read_to_string(&mut raw)?;
        if raw.trim().is_empty() {
            bail!("missing JSON body with 'from' and 'to'");
        }
        let body: QueryBody = serde_json::from_str(&raw).context("invalid JSON body")?;
        let from = body
            .from
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("missing required fields 'from' and 'to'"))?;
        let to = body
            .to
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("missing required fields 'from' and 'to'"))?;
        return Ok(QueryInputs {
            from,
            to,
            target_types: normalized_types(body.target_type),
            modified_filepaths: body
                .modified_filepaths
                .unwrap_or_default()
                .into_iter()
                .map(|path| path.trim().to_owned())
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .collect(),
            profile: body.profile.unwrap_or(false),
        });
    }
    let params = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<std::collections::HashMap<_, _>>();
    let from = params
        .get("from")
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| anyhow!("missing required query parameters 'from' and 'to'"))?;
    let to = params
        .get("to")
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| anyhow!("missing required query parameters 'from' and 'to'"))?;
    Ok(QueryInputs {
        from,
        to,
        target_types: normalized_types(
            params
                .get("targetType")
                .map(|value| value.split(',').map(str::to_owned).collect()),
        ),
        modified_filepaths: BTreeSet::new(),
        profile: params.get("profile").is_some_and(|value| value == "true"),
    })
}

fn normalized_types(types: Option<Vec<String>>) -> Option<HashSet<String>> {
    types
        .map(|types| {
            types
                .into_iter()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .collect::<HashSet<_>>()
        })
        .filter(|types| !types.is_empty())
}

fn compute_query(state: &Arc<State>, inputs: QueryInputs, kind: QueryKind) -> Result<Value> {
    let started = Instant::now();
    let _guard = state
        .workspace_lock
        .lock()
        .map_err(|_| anyhow!("workspace lock poisoned"))?;
    let resolve_started = Instant::now();
    let (from_sha, to_sha) = resolve_both(state, &inputs.from, &inputs.to)?;
    let resolve_millis = resolve_started.elapsed().as_millis() as u64;
    if matches!(kind, QueryKind::DepEdges) {
        // Same graph generate-hashes writes to --depEdgesFile: the end revision's
        // full (unscoped) dependency map. modifiedFilepaths must not shrink it —
        // PRI/CRI BFS-walk this file from the app target and need every edge.
        let (to, _) = get_hashes_locked(state, &to_sha, &BTreeSet::new())?;
        prune_cache(state)?;
        return Ok(serde_json::to_value(&to.dep_edges)?);
    }
    let from_started = Instant::now();
    let (from, from_hit) = get_hashes_locked(state, &from_sha, &inputs.modified_filepaths)?;
    let from_millis = from_started.elapsed().as_millis() as u64;
    let to_started = Instant::now();
    let (to, to_hit) = get_hashes_locked(state, &to_sha, &inputs.modified_filepaths)?;
    let to_millis = to_started.elapsed().as_millis() as u64;
    let diff_started = Instant::now();
    checkout(state, &to_sha)?;
    let exclude_external = state.config.hash_options.bazel.is_bzlmod_enabled();
    let impacted =
        impacted_with_module_changes(&from, &to, Some(&state.config.hash_options.bazel))?;
    let impacted_value = if matches!(kind, QueryKind::Distances) {
        let hash_impacted = impacted_targets(&from.hashes, &to.hashes, None, false)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut distance_from = from.hashes.clone();
        for label in impacted.difference(&hash_impacted) {
            distance_from.remove(label);
        }
        serde_json::to_value(impacted_targets_with_distances(
            &distance_from,
            &to.hashes,
            &to.dep_edges,
            inputs.target_types.as_ref(),
            exclude_external,
        )?)?
    } else {
        serde_json::to_value(filter_and_sort_labels(
            impacted,
            &from.hashes,
            &to.hashes,
            inputs.target_types.as_ref(),
            exclude_external,
        )?)?
    };
    let diff_millis = diff_started.elapsed().as_millis() as u64;
    let mut result = serde_json::Map::from_iter([
        ("from".to_owned(), Value::String(from_sha.clone())),
        ("to".to_owned(), Value::String(to_sha.clone())),
        ("impactedTargets".to_owned(), impacted_value),
    ]);
    if inputs.profile {
        result.insert(
            "profile".to_owned(),
            json!({
                "totalDurationMillis": started.elapsed().as_millis() as u64,
                "resolveRevisionsDurationMillis": resolve_millis,
                "hashRetrievals": [
                    {"sha": from_sha, "cacheHit": from_hit, "durationMillis": from_millis},
                    {"sha": to_sha, "cacheHit": to_hit, "durationMillis": to_millis}
                ],
                "diffDurationMillis": diff_millis,
                "diffModuleGraphChanged": from.module_graph_json != to.module_graph_json
            }),
        );
        let (used, max) = memory_usage();
        result.insert(
            "memoryProfile".to_owned(),
            json!({
                "heapUsedBeforeBytes": used,
                "heapUsedAfterBytes": used,
                "heapUsedDeltaBytes": 0,
                "heapMaxBytes": max,
                "gcCollections": 0,
                "gcTimeMillis": 0
            }),
        );
    }
    prune_cache(state)?;
    Ok(Value::Object(result))
}

fn warm_revision(state: &Arc<State>, revision: &str) -> Result<()> {
    let _guard = state
        .workspace_lock
        .lock()
        .map_err(|_| anyhow!("workspace lock poisoned"))?;
    let sha = resolve_sha(state, revision)?;
    get_hashes_locked(state, &sha, &BTreeSet::new())?;
    Ok(())
}

fn get_hashes_locked(
    state: &Arc<State>,
    sha: &str,
    modified: &BTreeSet<PathBuf>,
) -> Result<(HashFileData, bool)> {
    let key = cache_key(state, sha, modified);
    let path = state.config.cache_dir.join(format!("{key}.json"));
    if path.is_file() {
        let data = HashFileData::read(&path)?;
        touch(&path);
        return Ok((data, true));
    }
    if let Some(remote) = &state.remote {
        if remote.contains(&key) {
            if let Some(bytes) = remote.get(&key) {
                let data = HashFileData::from_slice(&bytes)?;
                fs::write(&path, &bytes)?;
                return Ok((data, true));
            }
        }
    }
    checkout(state, sha)?;
    let mut options = state.config.hash_options.clone();
    options.modified_filepaths = modified.clone();
    options.track_deps = state.config.track_deps;
    let data = generate_hashes(&options)?;
    let bytes = serde_json::to_vec(&data.serialized(true, state.config.track_deps))?;
    let temporary = state.config.cache_dir.join(format!("{key}.tmp"));
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)?;
    if let Some(remote) = &state.remote {
        let bytes = fs::read(state.config.cache_dir.join(format!("{key}.json")))?;
        remote.put(&key, &bytes);
    }
    Ok((data, false))
}

fn configuration_fingerprint(config: &ServerConfig) -> String {
    let options = &config.hash_options;
    let mut hasher = Sha256::new();
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.update([0]);
    for value in [
        options.bazel.startup_options.join("\0"),
        options.bazel.command_options.join("\0"),
        options.bazel.cquery_options.join("\0"),
        options.bazel.use_cquery.to_string(),
        options.bazel.cquery_expression.clone().unwrap_or_default(),
        options.bazel.keep_going.to_string(),
        options.bazel.exclude_external_targets.to_string(),
        options
            .bazel
            .exclude_targets_query
            .clone()
            .unwrap_or_default(),
        options
            .bazel
            .fine_grained_external_repos
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
        options
            .ignored_attributes
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(","),
        options
            .seed_filepaths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\0"),
        options
            .always_affected_tags
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(","),
        config.track_deps.to_string(),
    ] {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())[..12].to_owned()
}

fn cache_key(state: &State, sha: &str, modified: &BTreeSet<PathBuf>) -> String {
    let base = format!("{sha}.{}", state.fingerprint);
    if modified.is_empty() {
        return base;
    }
    let mut hasher = Sha256::new();
    for path in modified {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(b"\n");
    }
    format!("{base}.{}", &hex::encode(hasher.finalize())[..12])
}

fn resolve_both(state: &State, from: &str, to: &str) -> Result<(String, String)> {
    if let (Ok(from), Ok(to)) = (resolve_sha(state, from), resolve_sha(state, to)) {
        return Ok((from, to));
    }
    git(
        state,
        &[
            String::from("fetch"),
            String::from("--all"),
            String::from("--tags"),
        ],
    )?;
    for revision in BTreeSet::from([from, to]) {
        if resolve_sha(state, revision).is_err() {
            fetch_revision(state, revision);
        }
    }
    Ok((resolve_sha(state, from)?, resolve_sha(state, to)?))
}

fn fetch_revision(state: &State, revision: &str) -> bool {
    let Ok(output) = git_output(state, &[String::from("remote")]) else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|remote| !remote.is_empty())
        .any(|remote| {
            git(
                state,
                &[
                    String::from("fetch"),
                    remote.to_owned(),
                    revision.to_owned(),
                ],
            )
            .is_ok()
        })
}

fn resolve_sha(state: &State, revision: &str) -> Result<String> {
    let output = git_output(
        state,
        &[
            String::from("rev-parse"),
            String::from("--verify"),
            format!("{revision}^{{commit}}"),
        ],
    )?;
    if !output.status.success() {
        bail!("revision '{revision}' was not found");
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if sha.len() != 40 {
        bail!("revision '{revision}' resolved to invalid SHA '{sha}'");
    }
    Ok(sha)
}

fn checkout(state: &State, sha: &str) -> Result<()> {
    let args = [
        String::from("-c"),
        String::from("advice.detachedHead=false"),
        String::from("checkout"),
        String::from("--force"),
        sha.to_owned(),
    ];
    if git(state, &args).is_ok() {
        return Ok(());
    }
    let index_lock = state
        .config
        .hash_options
        .bazel
        .workspace
        .join(".git/index.lock");
    if !index_lock.is_file() {
        bail!("git checkout failed for revision {sha}");
    }
    fs::remove_file(&index_lock).context("failed to remove stale .git/index.lock")?;
    git(state, &args)
}

fn git(state: &State, args: &[String]) -> Result<()> {
    let output = git_output(state, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn git_output(state: &State, args: &[String]) -> Result<std::process::Output> {
    Command::new(&state.config.git_path)
        .current_dir(&state.config.hash_options.bazel.workspace)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to execute {}", state.config.git_path.display()))
}

fn metrics(state: &State) -> Value {
    let entries = cache_entries(&state.config.cache_dir);
    let total = entries.iter().map(|entry| entry.size).sum::<u64>();
    let (used, max) = memory_usage();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptimeSeconds": state.started.elapsed().as_secs(),
        "ready": state.ready.load(Ordering::Acquire),
        "gitEngine": "subprocess",
        "trackDeps": state.config.track_deps,
        "cache": {
            "directory": state.config.cache_dir.to_string_lossy(),
            "remote": state.config.remote_cache,
            "entries": entries.len(),
            "sizeBytes": total,
            "sizeHuman": human_bytes(total),
        },
        "jvm": {"usedBytes": used, "maxBytes": max}
    })
}

#[derive(Clone)]
struct CacheEntry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

fn cache_entries(directory: &Path) -> Vec<CacheEntry> {
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            Some(CacheEntry {
                path,
                size: metadata.len(),
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            })
        })
        .collect()
}

fn prune_cache(state: &State) -> Result<()> {
    let mut entries = cache_entries(&state.config.cache_dir);
    entries.sort_by_key(|entry| entry.modified);
    let now = SystemTime::now();
    if let Some(max_age) = state.config.cache_max_age {
        for entry in entries.clone() {
            if now.duration_since(entry.modified).unwrap_or_default() > max_age {
                let _ = fs::remove_file(entry.path);
            }
        }
        entries = cache_entries(&state.config.cache_dir);
        entries.sort_by_key(|entry| entry.modified);
    }
    if let Some(max_entries) = state.config.cache_max_entries {
        while entries.len() > max_entries {
            let entry = entries.remove(0);
            let _ = fs::remove_file(entry.path);
        }
    }
    if let Some(max_size) = state.config.cache_max_size {
        let mut total = entries.iter().map(|entry| entry.size).sum::<u64>();
        while total > max_size && !entries.is_empty() {
            let entry = entries.remove(0);
            total = total.saturating_sub(entry.size);
            let _ = fs::remove_file(entry.path);
        }
    }
    Ok(())
}

fn touch(path: &Path) {
    // Rewriting the same bytes is portable and gives pruning a genuine
    // last-use timestamp without another platform-specific dependency.
    if let Ok(bytes) = fs::read(path) {
        let _ = fs::write(path, bytes);
    }
}

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let units = ["KB", "MB", "GB", "TB"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}

fn memory_usage() -> (u64, u64) {
    let page_size = 4096u64;
    let used = fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|value| {
            value
                .split_whitespace()
                .nth(1)
                .and_then(|pages| pages.parse::<u64>().ok())
        })
        .unwrap_or(0)
        .saturating_mul(page_size);
    let max = fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    (used, max)
}

fn respond_json(request: Request, status: u16, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let response = Response::from_data(body)
        .with_status_code(StatusCode(status))
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap());
    request
        .respond(response)
        .map_err(|error| anyhow!("failed to write response: {error}"))
}

fn respond_text(request: Request, status: u16, body: &str) -> Result<()> {
    request
        .respond(Response::from_string(body).with_status_code(StatusCode(status)))
        .map_err(|error| anyhow!("failed to write response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bazel::BazelOptions;
    use crate::model::TargetHash;
    use std::collections::HashSet;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::mpsc as std_mpsc;
    use std::thread;

    fn test_config() -> ServerConfig {
        ServerConfig {
            hash_options: HashOptions {
                bazel: BazelOptions {
                    workspace: PathBuf::from("."),
                    bazel: PathBuf::from("bazel"),
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
                },
                content_hashes: None,
                ignored_attributes: HashSet::new(),
                seed_filepaths: BTreeSet::new(),
                modified_filepaths: BTreeSet::new(),
                track_deps: false,
                always_affected_tags: HashSet::new(),
            },
            git_path: PathBuf::from("git"),
            port: 0,
            request_timeout: Duration::ZERO,
            cache_dir: PathBuf::from("."),
            track_deps: false,
            no_initial_fetch: true,
            warmup_revisions: Vec::new(),
            cache_max_age: None,
            cache_max_entries: None,
            cache_max_size: None,
            cache_prune_interval: Duration::from_secs(60),
            remote_cache: None,
            s3_bucket: None,
            s3_prefix: String::new(),
            s3_region: None,
            s3_endpoint: None,
            s3_force_path_style: false,
        }
    }

    fn hash(kind: &str, value: &str, direct: &str) -> TargetHash {
        TargetHash {
            kind: kind.to_owned(),
            hash: value.to_owned(),
            direct_hash: direct.to_owned(),
            deps: Vec::new(),
        }
    }

    fn write_hashes(path: &Path, data: &HashFileData, include_deps: bool) {
        fs::write(
            path,
            serde_json::to_vec(&data.serialized(true, include_deps)).unwrap(),
        )
        .unwrap();
    }

    fn git_command(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn initialize_git_repo() -> (tempfile::TempDir, String, String) {
        let repo = tempfile::tempdir().unwrap();
        git_command(repo.path(), &["init", "-q"]);
        git_command(repo.path(), &["config", "user.email", "test@example.com"]);
        git_command(repo.path(), &["config", "user.name", "Test"]);
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        git_command(repo.path(), &["add", "file.txt"]);
        git_command(repo.path(), &["commit", "-q", "-m", "one"]);
        let first = git_command(repo.path(), &["rev-parse", "HEAD"]);
        fs::write(repo.path().join("file.txt"), "two").unwrap();
        git_command(repo.path(), &["commit", "-q", "-am", "two"]);
        let second = git_command(repo.path(), &["rev-parse", "HEAD"]);
        (repo, first, second)
    }

    fn state_for_repo(repo: &Path, cache: &Path, track_deps: bool) -> Arc<State> {
        let mut config = test_config();
        config.hash_options.bazel.workspace = repo.to_path_buf();
        config.hash_options.bazel.bazel = PathBuf::from("/bin/false");
        config.git_path = PathBuf::from("git");
        config.cache_dir = cache.to_path_buf();
        config.track_deps = track_deps;
        config.hash_options.track_deps = track_deps;
        let fingerprint = configuration_fingerprint(&config);
        Arc::new(State {
            config,
            ready: AtomicBool::new(true),
            workspace_lock: Mutex::new(()),
            started: Instant::now(),
            fingerprint,
            remote: None,
        })
    }

    fn request(state: &Arc<State>, request: &str) -> String {
        let server = Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let request = request.to_owned();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let incoming = server.recv().unwrap();
        route(incoming, state).unwrap();
        client.join().unwrap()
    }

    struct CapturedS3Request {
        url: String,
    }

    fn remote_cache_with_responses(
        prefix: &str,
        responses: Vec<(u16, Vec<u8>)>,
    ) -> (
        RemoteCache,
        std_mpsc::Receiver<CapturedS3Request>,
        thread::JoinHandle<()>,
    ) {
        let server = Server::http("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (sender, receiver) = std_mpsc::channel();
        let handle = thread::spawn(move || {
            for (status, body) in responses {
                let request = server.recv().unwrap();
                sender
                    .send(CapturedS3Request {
                        url: request.url().to_owned(),
                    })
                    .unwrap();
                request
                    .respond(Response::from_data(body).with_status_code(StatusCode(status)))
                    .unwrap();
            }
        });
        let credentials = Credentials::new(Some("key"), Some("secret"), None, None, None).unwrap();
        let bucket = Bucket::new(
            "bucket",
            Region::Custom {
                region: "us-east-1".to_owned(),
                endpoint,
            },
            credentials,
        )
        .unwrap()
        .with_path_style();
        (
            RemoteCache {
                bucket,
                prefix: normalize_s3_prefix(prefix),
            },
            receiver,
            handle,
        )
    }

    #[test]
    fn parses_compound_duration() {
        assert_eq!(parse_duration("1d12h30m").unwrap().as_secs(), 131_400);
        assert!(parse_duration("1hour").is_err());
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        for invalid in ["", "10", "s", "1x", "1h30", "18446744073709551615d1d"] {
            assert!(parse_duration(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn parses_binary_byte_size() {
        assert_eq!(parse_byte_size("10GB").unwrap(), 10 * 1024u64.pow(3));
        assert_eq!(parse_byte_size("42").unwrap(), 42);
        assert_eq!(parse_byte_size("1kb").unwrap(), 1024);
        assert_eq!(parse_byte_size("2 MB").unwrap(), 2 * 1024u64.pow(2));
        assert_eq!(parse_byte_size("3t").unwrap(), 3 * 1024u64.pow(4));
        for invalid in ["", "kb", "1pb", "18446744073709551615tb"] {
            assert!(parse_byte_size(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn cache_fingerprint_includes_all_hash_affecting_sets() {
        let base = test_config();
        let base_fingerprint = configuration_fingerprint(&base);

        let mut seeded = base.clone();
        seeded
            .hash_options
            .seed_filepaths
            .insert(PathBuf::from("seed.txt"));
        assert_ne!(configuration_fingerprint(&seeded), base_fingerprint);

        let mut always_affected = base;
        always_affected
            .hash_options
            .always_affected_tags
            .insert("external".to_owned());
        assert_ne!(
            configuration_fingerprint(&always_affected),
            base_fingerprint
        );
    }

    #[test]
    fn normalizes_remote_cache_configuration() {
        assert_eq!(normalize_s3_prefix(""), "");
        assert_eq!(normalize_s3_prefix("/"), "");
        assert_eq!(normalize_s3_prefix("/team/cache/"), "team/cache/");
        assert!(is_s3_not_found(&S3Error::HttpFailWithBody(
            404,
            "missing".into()
        )));
        assert!(!is_s3_not_found(&S3Error::HttpFailWithBody(
            500,
            "failure".into()
        )));
        assert!(RemoteCache::new(&test_config()).unwrap().is_none());
    }

    #[test]
    fn remote_cache_operations_use_normalized_key_and_succeed() {
        let (remote, requests, handle) = remote_cache_with_responses(
            "/team/repo/",
            vec![
                (200, b"hello".to_vec()),
                (200, Vec::new()),
                (200, Vec::new()),
            ],
        );
        assert_eq!(remote.object_key("sha.fp"), "team/repo/sha.fp.json");
        assert_eq!(remote.get("sha.fp"), Some(b"hello".to_vec()));
        remote.put("sha.fp", b"data");
        assert!(remote.contains("sha.fp"));
        let captured = (0..3)
            .map(|_| requests.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect::<Vec<_>>();
        handle.join().unwrap();
        assert!(captured
            .iter()
            .all(|request| request.url.contains("/bucket/team/repo/sha.fp.json")));
    }

    #[test]
    fn remote_cache_misses_and_errors_degrade_without_failing() {
        let (remote, requests, handle) = remote_cache_with_responses(
            "",
            vec![
                (404, b"missing".to_vec()),
                (500, b"failure".to_vec()),
                (404, Vec::new()),
                (500, Vec::new()),
                (500, Vec::new()),
            ],
        );
        assert_eq!(remote.get("missing"), None);
        assert_eq!(remote.get("failure"), None);
        assert!(!remote.contains("missing"));
        assert!(!remote.contains("failure"));
        remote.put("failure", b"data");
        for _ in 0..5 {
            requests.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        handle.join().unwrap();
    }

    #[test]
    fn normalizes_target_types() {
        assert_eq!(
            normalized_types(Some(vec![
                " Rule ".into(),
                "".into(),
                "SourceFile".into(),
                "Rule".into(),
            ])),
            Some(HashSet::from(["Rule".into(), "SourceFile".into()]))
        );
        assert!(normalized_types(None).is_none());
        assert!(normalized_types(Some(vec![" ".into()])).is_none());
    }

    #[test]
    fn cache_keys_are_stable_and_modified_paths_are_ordered() {
        let repo = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), false);
        let base = cache_key(&state, "abc", &BTreeSet::new());
        assert_eq!(base, format!("abc.{}", state.fingerprint));
        let modified = cache_key(
            &state,
            "abc",
            &BTreeSet::from([PathBuf::from("b"), PathBuf::from("a")]),
        );
        assert!(modified.starts_with(&format!("{base}.")));
        assert_ne!(modified, base);
        assert_eq!(
            modified,
            cache_key(
                &state,
                "abc",
                &BTreeSet::from([PathBuf::from("a"), PathBuf::from("b")])
            )
        );
    }

    #[test]
    fn git_resolution_checkout_and_errors_use_real_repository() {
        let (repo, first, second) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), false);
        assert_eq!(resolve_sha(&state, "HEAD").unwrap(), second);
        assert_eq!(
            resolve_both(&state, &first, &second).unwrap(),
            (first.clone(), second.clone())
        );
        checkout(&state, &first).unwrap();
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).unwrap(),
            "one"
        );
        fs::write(repo.path().join(".git/index.lock"), "").unwrap();
        checkout(&state, &second).unwrap();
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).unwrap(),
            "two"
        );
        assert!(!repo.path().join(".git/index.lock").exists());
        assert!(resolve_sha(&state, "missing").is_err());
        assert!(checkout(&state, "missing").is_err());
        assert!(git(&state, &["not-a-command".into()]).is_err());

        let output = git_output(&state, &["rev-parse".into(), "HEAD".into()]).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), second);

        let mut bad_config = state.config.clone();
        bad_config.git_path = PathBuf::from("/definitely/missing/git");
        let bad = State {
            config: bad_config,
            ready: AtomicBool::new(true),
            workspace_lock: Mutex::new(()),
            started: Instant::now(),
            fingerprint: state.fingerprint.clone(),
            remote: None,
        };
        assert!(git_output(&bad, &[]).is_err());
    }

    #[test]
    fn targeted_fetch_tries_each_remote_and_handles_missing_remotes() {
        let root = tempfile::tempdir().unwrap();
        let good = root.path().join("good");
        let bad = root.path().join("bad");
        fs::create_dir(&good).unwrap();
        fs::create_dir(&bad).unwrap();
        for repository in [&good, &bad] {
            git_command(repository, &["init", "-q"]);
            git_command(repository, &["config", "user.email", "test@example.com"]);
            git_command(repository, &["config", "user.name", "Test"]);
        }
        fs::write(good.join("good.txt"), "good").unwrap();
        git_command(&good, &["add", "."]);
        git_command(&good, &["commit", "-q", "-m", "good"]);
        let wanted = git_command(&good, &["rev-parse", "HEAD"]);
        fs::write(bad.join("bad.txt"), "bad").unwrap();
        git_command(&bad, &["add", "."]);
        git_command(&bad, &["commit", "-q", "-m", "bad"]);

        let workspace = root.path().join("workspace");
        git_command(
            root.path(),
            &[
                "clone",
                "-q",
                bad.to_str().unwrap(),
                workspace.to_str().unwrap(),
            ],
        );
        git_command(&workspace, &["remote", "rename", "origin", "bad"]);
        git_command(
            &workspace,
            &["remote", "add", "good", good.to_str().unwrap()],
        );
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(&workspace, cache.path(), false);
        assert!(resolve_sha(&state, &wanted).is_err());
        assert!(fetch_revision(&state, &wanted));
        assert_eq!(resolve_sha(&state, &wanted).unwrap(), wanted);
        assert!(!fetch_revision(
            &state,
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        ));

        git_command(&workspace, &["remote", "remove", "bad"]);
        git_command(&workspace, &["remote", "remove", "good"]);
        assert!(!fetch_revision(&state, "HEAD"));

        let mut bad_config = state.config.clone();
        bad_config.git_path = PathBuf::from("/missing/git");
        let bad_state = State {
            config: bad_config,
            ready: AtomicBool::new(true),
            workspace_lock: Mutex::new(()),
            started: Instant::now(),
            fingerprint: state.fingerprint.clone(),
            remote: None,
        };
        assert!(!fetch_revision(&bad_state, "HEAD"));
    }

    #[test]
    fn local_hash_cache_hits_and_touches_entries() {
        let (repo, _, sha) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), true);
        let data = HashFileData {
            hashes: std::collections::BTreeMap::from([(
                "//app:lib".into(),
                hash("Rule", "overall", "direct"),
            )]),
            dep_edges: std::collections::BTreeMap::from([(
                "//app:lib".into(),
                vec!["//dep:lib".into()],
            )]),
            ..Default::default()
        };
        let key = cache_key(&state, &sha, &BTreeSet::new());
        let path = cache.path().join(format!("{key}.json"));
        write_hashes(&path, &data, true);
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let (loaded, hit) = get_hashes_locked(&state, &sha, &BTreeSet::new()).unwrap();
        assert!(hit);
        assert_eq!(loaded.hashes["//app:lib"].hash, "overall");
        assert_eq!(loaded.dep_edges["//app:lib"], ["//dep:lib"]);
        assert!(fs::metadata(path).unwrap().modified().unwrap() >= before);
    }

    #[test]
    fn remote_cache_hit_is_backfilled_and_next_read_is_local() {
        let (repo, _, sha) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let data = HashFileData {
            hashes: std::collections::BTreeMap::from([(
                "//app:lib".into(),
                hash("Rule", "overall", "direct"),
            )]),
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&data.serialized(true, false)).unwrap();
        let (remote, requests, handle) =
            remote_cache_with_responses("", vec![(200, Vec::new()), (200, bytes)]);
        let mut config = test_config();
        config.hash_options.bazel.workspace = repo.path().to_path_buf();
        config.git_path = PathBuf::from("git");
        config.cache_dir = cache.path().to_path_buf();
        let fingerprint = configuration_fingerprint(&config);
        let state = Arc::new(State {
            config,
            ready: AtomicBool::new(true),
            workspace_lock: Mutex::new(()),
            started: Instant::now(),
            fingerprint,
            remote: Some(remote),
        });

        let (first, first_hit) = get_hashes_locked(&state, &sha, &BTreeSet::new()).unwrap();
        assert!(first_hit);
        assert_eq!(first.hashes["//app:lib"].hash, "overall");
        let key = cache_key(&state, &sha, &BTreeSet::new());
        assert!(cache.path().join(format!("{key}.json")).is_file());
        let (second, second_hit) = get_hashes_locked(&state, &sha, &BTreeSet::new()).unwrap();
        assert!(second_hit);
        assert_eq!(second.hashes["//app:lib"].hash, "overall");

        for _ in 0..2 {
            requests.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        handle.join().unwrap();
    }

    #[test]
    fn compute_query_uses_cached_revisions_and_profiles() {
        let (repo, first, second) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), true);
        let from = HashFileData {
            hashes: std::collections::BTreeMap::from([
                ("//app:source".into(), hash("SourceFile", "old", "old")),
                ("//app:rule".into(), hash("Rule", "old-rule", "same-direct")),
            ]),
            ..Default::default()
        };
        let to = HashFileData {
            hashes: std::collections::BTreeMap::from([
                ("//app:source".into(), hash("SourceFile", "new", "new")),
                ("//app:rule".into(), hash("Rule", "new-rule", "same-direct")),
            ]),
            dep_edges: std::collections::BTreeMap::from([(
                "//app:rule".into(),
                vec!["//app:source".into()],
            )]),
            ..Default::default()
        };
        for (sha, data) in [(&first, &from), (&second, &to)] {
            let key = cache_key(&state, sha, &BTreeSet::new());
            write_hashes(
                &cache.path().join(format!("{key}.json")),
                data,
                state.config.track_deps,
            );
        }
        let inputs = QueryInputs {
            from: first,
            to: second.clone(),
            target_types: None,
            modified_filepaths: BTreeSet::new(),
            profile: true,
        };
        let result = compute_query(&state, inputs, QueryKind::Distances).unwrap();
        assert_eq!(result["to"], second);
        assert_eq!(result["impactedTargets"].as_array().unwrap().len(), 2);
        assert_eq!(result["profile"]["hashRetrievals"][0]["cacheHit"], true);
        assert!(result.get("memoryProfile").is_some());
        assert_eq!(git_command(repo.path(), &["rev-parse", "HEAD"]), second);
    }

    #[test]
    fn cache_pruning_applies_age_count_and_size_limits() {
        let repo = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        for (name, size) in [("a.json", 3), ("b.json", 5), ("c.json", 7)] {
            fs::write(cache.path().join(name), vec![b'x'; size]).unwrap();
            thread::sleep(Duration::from_millis(5));
        }
        fs::write(cache.path().join("ignored.tmp"), "keep").unwrap();

        let mut state = state_for_repo(repo.path(), cache.path(), false);
        Arc::get_mut(&mut state).unwrap().config.cache_max_entries = Some(2);
        prune_cache(&state).unwrap();
        assert_eq!(cache_entries(cache.path()).len(), 2);

        Arc::get_mut(&mut state).unwrap().config.cache_max_size = Some(7);
        prune_cache(&state).unwrap();
        assert!(
            cache_entries(cache.path())
                .iter()
                .map(|entry| entry.size)
                .sum::<u64>()
                <= 7
        );

        Arc::get_mut(&mut state).unwrap().config.cache_max_age = Some(Duration::ZERO);
        thread::sleep(Duration::from_millis(2));
        prune_cache(&state).unwrap();
        assert!(cache_entries(cache.path()).is_empty());
        assert!(cache.path().join("ignored.tmp").exists());
    }

    #[test]
    fn metrics_and_human_sizes_report_cache_state() {
        let repo = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::write(cache.path().join("one.json"), vec![0; 1536]).unwrap();
        let state = state_for_repo(repo.path(), cache.path(), true);
        let value = metrics(&state);
        assert_eq!(value["ready"], true);
        assert_eq!(value["trackDeps"], true);
        assert_eq!(value["cache"]["entries"], 1);
        assert_eq!(value["cache"]["sizeHuman"], "1.5 KB");
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1024u64.pow(2)), "1.0 MB");
        assert_eq!(human_bytes(1024u64.pow(4)), "1.0 TB");
        let _ = memory_usage();
    }

    #[test]
    fn routing_covers_health_metrics_and_errors() {
        let repo = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut state = state_for_repo(repo.path(), cache.path(), false);
        Arc::get_mut(&mut state)
            .unwrap()
            .ready
            .store(false, Ordering::Release);
        let response = request(
            &state,
            "GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 503"));
        assert!(response.ends_with("NOT_READY\n"));

        Arc::get_mut(&mut state)
            .unwrap()
            .ready
            .store(true, Ordering::Release);
        let response = request(
            &state,
            "GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("application/json"));
        assert!(response.contains("\"gitEngine\":\"subprocess\""));

        let response = request(
            &state,
            "POST /metrics HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 405"));

        let response = request(
            &state,
            "GET /missing HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 404"));

        let response = request(
            &state,
            "GET /impacted_targets HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(response.contains("missing required query parameters"));

        let response = request(
            &state,
            "GET /impacted_targets_with_distances?from=a&to=b HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(response.contains("distances unavailable"));

        let response = request(
            &state,
            "GET /dependency_edges?from=a&to=b HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(response.contains("dependency edges unavailable"));
    }

    #[test]
    fn routing_returns_success_profiles_filters_and_distances() {
        let (repo, first, second) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), true);
        let from = HashFileData {
            hashes: std::collections::BTreeMap::from([
                ("//app:source".into(), hash("SourceFile", "old", "old")),
                ("//app:rule".into(), hash("Rule", "old-rule", "same-direct")),
            ]),
            ..Default::default()
        };
        let to = HashFileData {
            hashes: std::collections::BTreeMap::from([
                ("//app:source".into(), hash("SourceFile", "new", "new")),
                ("//app:rule".into(), hash("Rule", "new-rule", "same-direct")),
            ]),
            dep_edges: std::collections::BTreeMap::from([(
                "//app:rule".into(),
                vec!["//app:source".into()],
            )]),
            ..Default::default()
        };
        for (sha, data) in [(&first, &from), (&second, &to)] {
            let key = cache_key(&state, sha, &BTreeSet::new());
            write_hashes(
                &cache.path().join(format!("{key}.json")),
                data,
                state.config.track_deps,
            );
        }

        let response = request(
            &state,
            &format!(
                "GET /impacted_targets?from={first}&to={second}&targetType=Rule&profile=true HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"impactedTargets\":[\"//app:rule\"]"));
        assert!(response.contains("\"profile\""));
        assert!(response.contains("\"memoryProfile\""));

        let response = request(
            &state,
            &format!(
                "GET /impacted_targets_with_distances?from={first}&to={second}&targetType=Rule&profile=false HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"targetDistance\":1"));
        assert!(!response.contains("\"profile\""));

        let response = request(
            &state,
            &format!(
                "GET /dependency_edges?from={first}&to={second}&profile=true HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"//app:rule\":[\"//app:source\"]"));
        assert!(
            !response.contains("\"impactedTargets\""),
            "body must be the generate-hashes graph, not a wrapped query response: {response}"
        );
        assert!(
            !response.contains("\"profile\""),
            "profile=true must not wrap the graph body: {response}"
        );
    }

    #[test]
    fn slow_request_times_out_with_504() {
        let repo = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut state = state_for_repo(repo.path(), cache.path(), false);
        Arc::get_mut(&mut state).unwrap().config.request_timeout = Duration::from_millis(20);
        let _guard = state.workspace_lock.lock().unwrap();
        let response = request(
            &state,
            "GET /impacted_targets?from=a&to=b HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 504"), "{response}");
        assert!(response.contains("request timed out"));
    }

    #[test]
    fn post_query_parsing_covers_json_and_validation() {
        let repo = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), false);
        let body = r#"{"from":"a","to":"b","targetType":[" Rule ",""],"modifiedFilepaths":[" x ",""],"profile":true}"#;
        let response = request(
            &state,
            &format!(
                "POST /impacted_targets HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            ),
        );
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(response.contains("\"error\":"));

        for body in ["", "not-json", r#"{"from":"","to":"b"}"#] {
            let response = request(
                &state,
                &format!(
                    "POST /impacted_targets HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                ),
            );
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        }
    }

    #[test]
    fn cache_pruner_is_disabled_without_limits() {
        let repo = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), false);
        start_cache_pruner(&state).unwrap();
    }
}
