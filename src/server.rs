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

pub fn parse_duration(value: &str) -> Result<Duration> {
    if value.is_empty() {
        bail!("invalid duration '{value}'");
    }
    let mut seconds = 0u64;
    let mut digits = String::new();
    for character in value.chars() {
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
    let normalized = value.trim().to_ascii_lowercase();
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
        "/impacted_targets" | "/impacted_targets_with_distances" => {
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
            let distances = path.ends_with("_with_distances");
            if distances && !state.config.track_deps {
                return respond_json(
                    request,
                    400,
                    &json!({"error": "distances unavailable: server started without --trackDeps"}),
                );
            }
            let timeout = state.config.request_timeout;
            let state_for_compute = Arc::clone(state);
            let (sender, receiver) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let _ = sender.send(compute_query(&state_for_compute, inputs, distances));
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

fn compute_query(state: &Arc<State>, inputs: QueryInputs, distances: bool) -> Result<Value> {
    let started = Instant::now();
    let _guard = state
        .workspace_lock
        .lock()
        .map_err(|_| anyhow!("workspace lock poisoned"))?;
    let resolve_started = Instant::now();
    let (from_sha, to_sha) = resolve_both(state, &inputs.from, &inputs.to)?;
    let resolve_millis = resolve_started.elapsed().as_millis() as u64;
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
    let impacted_value = if distances {
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
            let _ = git(
                state,
                &[
                    String::from("fetch"),
                    String::from("origin"),
                    revision.to_owned(),
                ],
            );
        }
    }
    Ok((resolve_sha(state, from)?, resolve_sha(state, to)?))
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
    git(
        state,
        &[
            String::from("-c"),
            String::from("advice.detachedHead=false"),
            String::from("checkout"),
            String::from("--force"),
            sha.to_owned(),
        ],
    )
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
    use std::collections::HashSet;

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

    #[test]
    fn parses_compound_duration() {
        assert_eq!(parse_duration("1d12h30m").unwrap().as_secs(), 131_400);
        assert!(parse_duration("1hour").is_err());
    }

    #[test]
    fn parses_binary_byte_size() {
        assert_eq!(parse_byte_size("10GB").unwrap(), 10 * 1024u64.pow(3));
        assert_eq!(parse_byte_size("42").unwrap(), 42);
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
}
