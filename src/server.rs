use crate::bazel::BazelOptions;
use crate::cancel::{self, Cancellation, CANCELLED};
use crate::hash::{generate_hashes_timed, HashOptions};
use crate::model::{
    filter_and_sort_labels, impacted_targets, impacted_targets_with_distances, HashFileData,
};
use crate::module_graph::impacted_with_module_changes;
use crate::partial::{self, Classification, ClassifyInputs, Glob, TreeIndex, MODULE_FILES};
use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::CertificateDer;
use s3::creds::Credentials;
use s3::request::ResponseData;
use s3::{Bucket, Region};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use url::{form_urlencoded, Url};

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub hash_options: HashOptions,
    pub git_path: PathBuf,
    pub port: u16,
    pub request_timeout: Duration,
    pub cache_dir: PathBuf,
    pub track_deps: bool,
    /// Opt-in: guard cache hits with a fingerprint of the workspace's external-dependency state.
    pub dependency_fingerprint: bool,
    /// Opt-in: record hashing recipes in every entry and derive a revision from the request's
    /// cached `from` entry, without a `bazel query`, when the diff between them cannot have
    /// changed the graph (see [`crate::partial`]).
    pub partial_graph: bool,
    /// Paths whose change always forces a full generation in partial-graph mode.
    pub partial_graph_rebuild_patterns: Vec<Glob>,
    pub no_initial_fetch: bool,
    pub warmup_revisions: Vec<String>,
    pub cache_max_age: Option<Duration>,
    pub cache_max_entries: Option<usize>,
    pub cache_max_size: Option<u64>,
    pub cache_prune_interval: Duration,
    /// Byte budget for parsed entries kept in memory, measured by each entry's serialized size.
    /// `0` disables the in-memory tier.
    pub cache_memory_max: u64,
    pub remote_cache: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_prefix: String,
    pub s3_region: Option<String>,
    pub s3_endpoint: Option<String>,
    pub s3_force_path_style: bool,
    pub nexus_url: Option<String>,
    pub nexus_repository: Option<String>,
    pub nexus_prefix: String,
    pub nexus_credentials: Option<NexusCredentials>,
    pub nexus_ca_cert: Option<PathBuf>,
}

/// How long a timed-out request waits for its cancelled query to stop before answering anyway.
/// Killing the subprocesses is immediate; this bounds the CPU-only phases (target hashing) a
/// cancel can only interrupt at their boundaries.
const CANCEL_GRACE: Duration = Duration::from_secs(60);

/// How often a query waiting for the workspace re-checks whether it has been cancelled.
const WORKSPACE_WAIT_POLL: Duration = Duration::from_millis(50);

struct State {
    config: ServerConfig,
    ready: AtomicBool,
    workspace: Workspace,
    /// Partial-graph mode: revisions hashed by derivation, and full generations the classifier
    /// fell back to. `/metrics` reports both.
    partial_derived: AtomicUsize,
    partial_rebuilds: AtomicUsize,
    /// Queries accepted and not yet finished: waiting for the workspace, running, or abandoned by
    /// a timed-out client but still running. `/metrics` reports it as `activeQueries`.
    active_queries: AtomicUsize,
    started: Instant,
    fingerprint: String,
    remote: Option<RemoteCache>,
    /// Parsed entries, in front of the local-disk tier. Guarded by its own lock rather than the
    /// workspace's so `/metrics` can read its size while a query holds the workspace.
    memory: Mutex<MemoryCache>,
}

/// The in-memory tier: entries already parsed, most recently used last.
///
/// Nearly every request shares its `from` revision (the main branch) with the previous one, and
/// reading and parsing a large entry costs seconds, so a hit here is the difference between a
/// query bounded by the compare and one bounded by JSON parsing. Entries are sized by their
/// serialized length, the one cheap proxy for their heap footprint, and the least recently used
/// entry goes first when the budget is exceeded.
struct MemoryCache {
    budget: u64,
    used: u64,
    entries: VecDeque<MemoryEntry>,
}

struct MemoryEntry {
    key: String,
    bytes: u64,
    data: Arc<HashFileData>,
}

impl MemoryCache {
    fn new(budget: u64) -> Self {
        Self {
            budget,
            used: 0,
            entries: VecDeque::new(),
        }
    }

    /// Returns the entry for `key`, marking it most recently used.
    fn get(&mut self, key: &str) -> Option<Arc<HashFileData>> {
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        let entry = self.entries.remove(index)?;
        let data = Arc::clone(&entry.data);
        self.entries.push_back(entry);
        Some(data)
    }

    /// Remembers `data` for `key`, evicting least recently used entries to stay within budget. An
    /// entry larger than the whole budget is not kept.
    fn insert(&mut self, key: &str, bytes: u64, data: &Arc<HashFileData>) {
        if bytes > self.budget {
            return;
        }
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            let previous = self.entries.remove(index).expect("index from position");
            self.used -= previous.bytes;
        }
        while self.used + bytes > self.budget {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            self.used -= evicted.bytes;
        }
        self.used += bytes;
        self.entries.push_back(MemoryEntry {
            key: key.to_owned(),
            bytes,
            data: Arc::clone(data),
        });
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Locks the in-memory tier. A panic while holding it cannot leave the cache inconsistent in a
/// way that matters (it is only a cache), so a poisoned lock is recovered rather than propagated.
fn memory_cache(state: &State) -> std::sync::MutexGuard<'_, MemoryCache> {
    state
        .memory
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl State {
    fn new(config: ServerConfig, remote: Option<RemoteCache>) -> Self {
        let fingerprint = configuration_fingerprint(&config);
        Self {
            memory: Mutex::new(MemoryCache::new(config.cache_memory_max)),
            config,
            ready: AtomicBool::new(false),
            partial_derived: AtomicUsize::new(0),
            partial_rebuilds: AtomicUsize::new(0),
            workspace: Workspace::default(),
            active_queries: AtomicUsize::new(0),
            started: Instant::now(),
            fingerprint,
            remote,
        }
    }

    /// The server's Bazel options bound to the query currently holding the workspace, so the
    /// Bazel subprocesses it spawns die with that query.
    fn bazel_options(&self) -> BazelOptions {
        let mut options = self.config.hash_options.bazel.clone();
        options.cancellation = self.workspace.holder();
        options
    }
}

/// Serialises everything that mutates `--workspacePath` (git checkout, `bazel query`) and
/// remembers which operation holds it. That lets `/metrics` report `workspaceBusy`, lets the
/// subprocesses the holder spawns be killed when its query is cancelled, and lets a query that is
/// cancelled while queued give up instead of taking the workspace for nothing.
#[derive(Default)]
struct Workspace {
    holder: Mutex<Option<Arc<Cancellation>>>,
    released: Condvar,
}

impl Workspace {
    fn acquire(&self, token: &Arc<Cancellation>) -> Result<WorkspaceGuard<'_>> {
        let mut holder = self.holder.lock().map_err(|_| poisoned())?;
        while holder.is_some() {
            if token.is_cancelled() {
                bail!(CANCELLED);
            }
            holder = self
                .released
                .wait_timeout(holder, WORKSPACE_WAIT_POLL)
                .map_err(|_| poisoned())?
                .0;
        }
        *holder = Some(Arc::clone(token));
        Ok(WorkspaceGuard { workspace: self })
    }

    fn holder(&self) -> Option<Arc<Cancellation>> {
        self.holder.lock().ok().and_then(|holder| holder.clone())
    }

    fn is_busy(&self) -> bool {
        self.holder
            .lock()
            .map(|holder| holder.is_some())
            .unwrap_or(true)
    }

    /// Fails once the operation holding the workspace has been cancelled: the caller is about to
    /// do something (write a cache entry) that only a finished query should do.
    fn check_cancelled(&self) -> Result<()> {
        if let Some(token) = self.holder() {
            token.check()?;
        }
        Ok(())
    }
}

fn poisoned() -> anyhow::Error {
    anyhow!("workspace lock poisoned")
}

struct WorkspaceGuard<'a> {
    workspace: &'a Workspace,
}

impl Drop for WorkspaceGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut holder) = self.workspace.holder.lock() {
            *holder = None;
        }
        self.workspace.released.notify_all();
    }
}

/// Keeps `activeQueries` accurate even when the worker panics.
struct ActiveQuery(Arc<State>);

impl ActiveQuery {
    fn start(state: &Arc<State>) -> Self {
        state.active_queries.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(state))
    }
}

impl Drop for ActiveQuery {
    fn drop(&mut self) {
        self.0.active_queries.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The shared cache tier behind local disk. Every backend degrades the same way: a failed read is
/// a miss (the revision is regenerated) and a failed write leaves the entry local-only, so an
/// outage of the shared store costs throughput, never availability.
enum RemoteCache {
    S3(S3Cache),
    Nexus(Box<NexusCache>),
}

impl RemoteCache {
    fn new(config: &ServerConfig) -> Result<Option<Self>> {
        if let Some(bucket_name) = &config.s3_bucket {
            return Ok(Some(Self::S3(S3Cache::new(config, bucket_name)?)));
        }
        if let (Some(url), Some(repository)) = (&config.nexus_url, &config.nexus_repository) {
            return Ok(Some(Self::Nexus(Box::new(NexusCache::new(
                nexus_base_url(url, repository, &config.nexus_prefix)?,
                config.nexus_credentials.clone(),
                config.nexus_ca_cert.as_deref(),
            )?))));
        }
        Ok(None)
    }

    /// Reads the entry for `key`: the compressed object first, then the plain one an older
    /// replica (or this one, before compression) may have written. A read failure is a miss and
    /// does not fall through, so an outage costs one request per lookup, not two.
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        let compressed = format!("{key}{COMPRESSED_SUFFIX}");
        match self.fetch(&compressed) {
            Fetched::Found(bytes) => match gunzip(&bytes) {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    eprintln!(
                        "[Warn] shared cache entry {compressed} is not valid gzip (treating as a miss): {error}"
                    );
                    None
                }
            },
            Fetched::Missing => match self.fetch(&format!("{key}{PLAIN_SUFFIX}")) {
                Fetched::Found(bytes) => Some(bytes),
                Fetched::Missing | Fetched::Failed => None,
            },
            Fetched::Failed => None,
        }
    }

    /// Publishes `data` for `key` as a gzip-compressed object. Hash entries are repetitive label
    /// text and hex, so this is a 2-3x smaller upload and backfill for every replica.
    fn put(&self, key: &str, data: &[u8]) {
        match gzip(data) {
            Ok(compressed) => self.store(
                &format!("{key}{COMPRESSED_SUFFIX}"),
                &compressed,
                "application/gzip",
            ),
            Err(error) => eprintln!(
                "[Warn] failed to compress shared cache entry {key} (entry not shared): {error}"
            ),
        }
    }

    fn fetch(&self, name: &str) -> Fetched {
        match self {
            Self::S3(cache) => cache.get(name),
            Self::Nexus(cache) => cache.get(name),
        }
    }

    fn store(&self, name: &str, data: &[u8], content_type: &str) {
        match self {
            Self::S3(cache) => cache.put(name, data),
            Self::Nexus(cache) => cache.put(name, data, content_type),
        }
    }
}

/// Object name suffixes on the shared tier. New entries are written compressed; the plain suffix
/// is read for entries written before compression existed.
const COMPRESSED_SUFFIX: &str = ".json.gz";
const PLAIN_SUFFIX: &str = ".json";

/// Outcome of one object read on the shared tier. A clean miss and a failure are told apart so a
/// miss on the compressed name can fall back to the plain one while a failure stays a failure.
enum Fetched {
    Found(Vec<u8>),
    Missing,
    Failed,
}

/// Fast-level gzip: the compressed size is within a few percent of the default level on hash
/// entries and the encode is several times quicker, which matters on a cache write that already
/// sits on the critical path of a cold query.
fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(data.len() / 2), Compression::fast());
    encoder.write_all(data)?;
    encoder.finish()
}

fn gunzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(data.len() * 3);
    GzDecoder::new(data).read_to_end(&mut decoded)?;
    Ok(decoded)
}

struct S3Cache {
    bucket: Box<Bucket>,
    prefix: String,
}

impl S3Cache {
    fn new(config: &ServerConfig, bucket_name: &str) -> Result<Self> {
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
        Ok(Self {
            bucket,
            prefix: normalize_s3_prefix(&config.s3_prefix),
        })
    }

    fn object_key(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    /// Reads an entry, degrading to a miss on any failure.
    ///
    /// Deliberately a bare `GetObject` with no `HeadObject` probe in front of it: besides costing a
    /// second round trip on every hit, `head_object` cannot report a miss here. S3 answers a HEAD
    /// miss with `404`, `Transfer-Encoding: chunked` and no body, and every HTTP client on this
    /// stack fails reading that absent body -- `attohttpc: Io Error: unexpected end of file`. A GET
    /// miss carries a real `NoSuchKey` body, so it reads back cleanly.
    fn get(&self, name: &str) -> Fetched {
        let failure = match self.bucket.get_object(self.object_key(name)) {
            Ok(response) if response.status_code() == 200 => {
                return Fetched::Found(response.to_vec())
            }
            Ok(response) if response.status_code() == 404 => return Fetched::Missing,
            Ok(response) => http_failure(&response),
            Err(error) => error.to_string(),
        };
        eprintln!(
            "[Warn] S3 cache read of {} failed (treating as a miss): {failure}",
            self.object_key(name)
        );
        Fetched::Failed
    }

    fn put(&self, name: &str, data: &[u8]) {
        let failure = match self.bucket.put_object(self.object_key(name), data) {
            Ok(response) if (200..300).contains(&response.status_code()) => return,
            Ok(response) => http_failure(&response),
            Err(error) => error.to_string(),
        };
        eprintln!(
            "[Warn] S3 cache write of {} failed (entry not shared): {failure}",
            self.object_key(name)
        );
    }
}

/// Renders a non-2xx response for a warning, matching how `S3Error::HttpFailWithBody` reads.
///
/// Statuses are classified here rather than by `rust-s3`'s `fail-on-err` feature, which is
/// deliberately off: it turns every non-2xx into an error *after* reading the body, and the read
/// happens inside `rust-s3`'s `retry!`, so a plain 404 miss cost a duplicate request and a
/// one-second backoff sleep before being recognized as a miss. `retry!` still covers transport
/// errors, which are the ones worth retrying.
fn http_failure(response: &ResponseData) -> String {
    format!(
        "Got HTTP {} with content '{}'",
        response.status_code(),
        response.as_str().unwrap_or("<non-utf8 body>").trim()
    )
}

fn normalize_s3_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

/// Username and password (or a Nexus user token's name and pass codes) for HTTP basic auth.
#[derive(Clone)]
pub struct NexusCredentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for NexusCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NexusCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Cache entries stored as components of a Sonatype Nexus Repository *raw* hosted repository,
/// addressed through Nexus's plain HTTP content API: `GET`/`PUT` on
/// `<nexusUrl>/repository/<repository>/<prefix>/<key>.json.gz`.
struct NexusCache {
    session: attohttpc::Session,
    base: Url,
    credentials: Option<NexusCredentials>,
}

const NEXUS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const NEXUS_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounds how much of an error body (Nexus answers many failures with a full HTML page) is
/// echoed into a warning.
const NEXUS_ERROR_BODY_LIMIT: usize = 200;

impl NexusCache {
    fn new(
        base: Url,
        credentials: Option<NexusCredentials>,
        ca_cert: Option<&Path>,
    ) -> Result<Self> {
        let mut session = attohttpc::Session::new();
        session.connect_timeout(NEXUS_CONNECT_TIMEOUT);
        session.read_timeout(NEXUS_READ_TIMEOUT);
        // A redirect is reported, never followed: following one would replay the basic-auth
        // header against whatever host the `Location` names.
        session.follow_redirects(false);
        if let Some(path) = ca_cert {
            for certificate in read_pem_certificates(path)? {
                session.add_root_certificate(certificate);
            }
        }
        Ok(Self {
            session,
            base,
            credentials,
        })
    }

    fn object_url(&self, name: &str) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("validated by nexus_base_url")
            .pop_if_empty()
            .push(name);
        url
    }

    fn authorized<B>(&self, request: attohttpc::RequestBuilder<B>) -> attohttpc::RequestBuilder<B> {
        match &self.credentials {
            Some(credentials) => {
                request.basic_auth(&credentials.username, Some(&credentials.password))
            }
            None => request,
        }
    }

    /// Reads an entry, degrading to a miss on any failure. Nexus answers a missing component with
    /// a plain `404`.
    fn get(&self, name: &str) -> Fetched {
        let url = self.object_url(name);
        let failure = match self.authorized(self.session.get(url.as_str())).send() {
            Ok(response) if response.status() == attohttpc::StatusCode::OK => {
                match response.bytes() {
                    Ok(bytes) => return Fetched::Found(bytes),
                    Err(error) => error.to_string(),
                }
            }
            Ok(response) if response.status() == attohttpc::StatusCode::NOT_FOUND => {
                return Fetched::Missing
            }
            Ok(response) => nexus_http_failure(response),
            Err(error) => error.to_string(),
        };
        eprintln!("[Warn] Nexus cache read of {url} failed (treating as a miss): {failure}");
        Fetched::Failed
    }

    fn put(&self, name: &str, data: &[u8], content_type: &str) {
        let url = self.object_url(name);
        let failure = match self
            .authorized(self.session.put(url.as_str()))
            .header(attohttpc::header::CONTENT_TYPE, content_type)
            .bytes(data)
            .send()
        {
            Ok(response) if response.is_success() => return,
            Ok(response) if response.status() == attohttpc::StatusCode::BAD_REQUEST => format!(
                "{} (a raw hosted repository answers 400 when its deployment policy forbids \
                 redeploying an existing component; set it to \"Allow redeploy\")",
                nexus_http_failure(response)
            ),
            Ok(response) => nexus_http_failure(response),
            Err(error) => error.to_string(),
        };
        eprintln!("[Warn] Nexus cache write of {url} failed (entry not shared): {failure}");
    }
}

fn nexus_http_failure(response: attohttpc::Response) -> String {
    let status = response.status().as_u16();
    let body = response.text().unwrap_or_default();
    let body = body.trim();
    let shown = match body.char_indices().nth(NEXUS_ERROR_BODY_LIMIT) {
        Some((cut, _)) => format!("{}...", &body[..cut]),
        None => body.to_owned(),
    };
    format!("Got HTTP {status} with content '{shown}'")
}

/// Resolves the directory URL every entry lives under:
/// `<url>/repository/<repository>/<prefix>/`. `url` is the Nexus base URL, including any context
/// path Nexus is served under (`https://host/nexus`). Shared with the CLI, which renders it as the
/// `remote` cache location in `/metrics`.
pub fn nexus_base_url(url: &str, repository: &str, prefix: &str) -> Result<Url> {
    let mut base = Url::parse(url).with_context(|| format!("invalid Nexus URL {url:?}"))?;
    if !matches!(base.scheme(), "http" | "https") {
        bail!("Nexus URL must be http or https: {url}");
    }
    if !base.username().is_empty() || base.password().is_some() {
        bail!(
            "Nexus URL must not embed credentials; use --nexusUsername and \
             BAZEL_DIFF_NEXUS_PASSWORD"
        );
    }
    if repository.is_empty() || repository.contains('/') {
        bail!("Nexus repository must be a single non-empty name: {repository:?}");
    }
    base.set_query(None);
    base.set_fragment(None);
    {
        let mut segments = base
            .path_segments_mut()
            .map_err(|()| anyhow!("invalid Nexus URL {url:?}"))?;
        segments.pop_if_empty().push("repository").push(repository);
        segments.extend(prefix.split('/').filter(|segment| !segment.is_empty()));
        segments.push("");
    }
    Ok(base)
}

fn read_pem_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let pem = fs::read(path)
        .with_context(|| format!("failed to read Nexus CA certificate {}", path.display()))?;
    let certificates = CertificateDer::pem_slice_iter(&pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| anyhow!("invalid PEM in {}: {error:?}", path.display()))?;
    if certificates.is_empty() {
        bail!("no PEM certificates found in {}", path.display());
    }
    Ok(certificates)
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

/// Rejects configurations partial-graph mode cannot honour. The derivation evaluates recipes
/// keyed by bare label from the revision's git tree alone, so it has no configured instances
/// (cquery), no files outside the tree (fine-grained external repositories) and no
/// per-invocation seeds (always-affected tags); each of those would make a derived entry differ
/// from a full generation, which is the one property the mode rests on.
pub fn validate_partial_graph(config: &ServerConfig) -> Result<()> {
    if !config.partial_graph {
        return Ok(());
    }
    let bazel = &config.hash_options.bazel;
    if bazel.use_cquery {
        bail!("--partialGraph does not support --useCquery");
    }
    if !bazel.fine_grained_external_repos.is_empty() {
        bail!("--partialGraph does not support --fineGrainedHashExternalRepos");
    }
    if !config.hash_options.always_affected_tags.is_empty() {
        bail!("--partialGraph does not support --alwaysAffectedTags");
    }
    Ok(())
}

pub fn serve(config: ServerConfig) -> Result<()> {
    validate_partial_graph(&config)?;
    fs::create_dir_all(&config.cache_dir)?;
    let remote = RemoteCache::new(&config)?;
    let state = Arc::new(State::new(config, remote));
    prune_cache(&state)?;
    start_cache_pruner(&state)?;
    perform_initial_fetch(&state);
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

/// Performs the startup git fetch (unless `--noInitialFetch`) and warmup, then flips `ready`
/// once the clone is known good. On a fetch failure the server is left running but un-ready
/// ("lame duck") so `/health` reports 503 and a load balancer removes the instance rather than
/// us attempting a risky in-place repair; `/metrics` keeps answering with `ready: false`.
fn perform_initial_fetch(state: &Arc<State>) {
    if !state.config.no_initial_fetch {
        let fetch = [
            String::from("fetch"),
            String::from("--all"),
            String::from("--tags"),
        ];
        if let Err(error) = git(state, &fetch) {
            eprintln!(
                "[Error] initial git fetch failed; server is lame-ducked (health will report NOT_READY): {error:#}"
            );
            return;
        }
    }
    for revision in &state.config.warmup_revisions {
        if let Err(error) = warm_revision(state, revision) {
            eprintln!("[Warn] failed to warm revision {revision}: {error:#}");
        }
    }
    state.ready.store(true, Ordering::Release);
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
            let token = Arc::new(Cancellation::new());
            let active = ActiveQuery::start(state);
            let state_for_compute = Arc::clone(state);
            let worker_token = Arc::clone(&token);
            let (sender, receiver) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let result = compute_query(&state_for_compute, inputs, kind, &worker_token);
                // Decrement before answering so a client that polls /metrics right after its
                // response never sees its own finished query still counted.
                drop(active);
                let _ = sender.send(result);
            });
            let computed = if timeout.is_zero() {
                receiver
                    .recv()
                    .map_err(|_| anyhow!("query worker stopped"))?
            } else {
                match receiver.recv_timeout(timeout) {
                    Ok(result) => result,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let stopped = cancel_query(&token, &receiver, timeout);
                        return respond_json(
                            request,
                            504,
                            &json!({
                                "error": format!("request timed out after {}s", timeout.as_secs()),
                                "cancelled": stopped,
                                "workspaceBusy": state.workspace.is_busy(),
                            }),
                        );
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

/// Cancels the query a timed-out request abandoned and waits for its worker to stop, so the 504
/// is sent only once the query has released the workspace and will write no cache entry. Returns
/// whether the worker stopped within [`CANCEL_GRACE`]; when it did not, `/metrics` reports the
/// moment it does through `activeQueries` and `workspaceBusy`.
fn cancel_query(
    token: &Cancellation,
    receiver: &mpsc::Receiver<Result<Value>>,
    timeout: Duration,
) -> bool {
    let killed = token.cancel();
    eprintln!(
        "[Warn] request timed out after {}s; cancelled its query and killed {killed} subprocess(es)",
        timeout.as_secs()
    );
    let cancelled = Instant::now();
    match receiver.recv_timeout(CANCEL_GRACE) {
        Err(mpsc::RecvTimeoutError::Timeout) => {
            eprintln!(
                "[Warn] cancelled query is still running {}s later; /metrics reports when it stops",
                CANCEL_GRACE.as_secs()
            );
            false
        }
        Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            eprintln!(
                "[Info] cancelled query stopped and released the workspace after {}ms",
                elapsed_millis(cancelled)
            );
            true
        }
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

fn compute_query(
    state: &Arc<State>,
    inputs: QueryInputs,
    kind: QueryKind,
    token: &Arc<Cancellation>,
) -> Result<Value> {
    let started = Instant::now();
    let _guard = state.workspace.acquire(token)?;
    let resolve_started = Instant::now();
    let (from_sha, to_sha) = resolve_both(state, &inputs.from, &inputs.to)?;
    let resolve_millis = resolve_started.elapsed().as_millis() as u64;
    if matches!(kind, QueryKind::DepEdges) {
        // Same graph generate-hashes writes to --depEdgesFile: the end revision's
        // full (unscoped) dependency map. modifiedFilepaths must not shrink it —
        // PRI/CRI BFS-walk this file from the app target and need every edge.
        let to = get_hashes_locked(state, &to_sha, &BTreeSet::new(), None)?.data;
        prune_cache(state)?;
        return Ok(serde_json::to_value(&to.dep_edges)?);
    }
    let from_started = Instant::now();
    let from = get_hashes_locked(state, &from_sha, &inputs.modified_filepaths, None)?;
    let from_profile = retrieval_profile(&from_sha, &from, from_started);
    let from = from.data;
    let to_started = Instant::now();
    // The `from` entry is the base a partial-graph derivation of `to` starts from.
    let to = get_hashes_locked(
        state,
        &to_sha,
        &inputs.modified_filepaths,
        Some((&from_sha, &from)),
    )?;
    let to_profile = retrieval_profile(&to_sha, &to, to_started);
    let to = to.data;
    let diff_started = Instant::now();
    checkout(state, &to_sha)?;
    let bazel = state.bazel_options();
    let exclude_external = bazel.is_bzlmod_enabled();
    let impacted = impacted_with_module_changes(&from, &to, Some(&bazel))?;
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
                "hashRetrievals": [from_profile, to_profile],
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
    let _guard = state.workspace.acquire(&Arc::new(Cancellation::new()))?;
    let sha = resolve_sha(state, revision)?;
    get_hashes_locked(state, &sha, &BTreeSet::new(), None)?;
    Ok(())
}

/// Wall-clock breakdown of one cache-miss hash generation, in pipeline order: the `git
/// checkout`, then the phases of [`generate_hashes_timed`], then serialising the result and
/// writing it to the cache tiers. `targetCount` sizes the graph the phases operated on. Field
/// names match the profile the Kotlin service emitted, which tools/serve_harness.py asserts on.
///
/// In partial-graph mode the `partial` fields say how the entry was produced: `mode` is
/// `derived` (no checkout, no query: the base's recipes were re-evaluated) or `full`, with
/// `partialFallbackReason` naming what ruled a derivation out.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationBreakdown {
    checkout_millis: u64,
    bazel_query_millis: u64,
    target_hash_millis: u64,
    module_graph_millis: u64,
    cache_write_millis: u64,
    target_count: usize,
    #[serde(flatten)]
    partial: PartialProfile,
}

/// How partial-graph mode handled one generation; empty outside the mode.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct PartialProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    derived_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    partial_fallback_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_sources: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unmatched_paths: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    classify_millis: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    derive_millis: Option<u64>,
}

/// Outcome of one cache lookup: the data, whether it was served from a cache tier, and where the
/// time went. A hit carries only `cache_read_millis` (reading and deserialising the entry is the
/// whole cost of a hit); a miss carries only `generation`.
struct Retrieval {
    data: Arc<HashFileData>,
    cache_hit: bool,
    cache_read_millis: Option<u64>,
    generation: Option<GenerationBreakdown>,
}

impl Retrieval {
    fn hit(data: Arc<HashFileData>, read_started: Instant) -> Self {
        Self {
            data,
            cache_hit: true,
            cache_read_millis: Some(elapsed_millis(read_started)),
            generation: None,
        }
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// One `profile.hashRetrievals[]` entry. Optional phases are omitted rather than nulled so a hit
/// never shows a `generation` key and a miss never shows `cacheReadMillis`.
fn retrieval_profile(sha: &str, retrieval: &Retrieval, started: Instant) -> Value {
    let mut entry = serde_json::Map::from_iter([
        ("sha".to_owned(), Value::String(sha.to_owned())),
        ("cacheHit".to_owned(), Value::Bool(retrieval.cache_hit)),
        (
            "durationMillis".to_owned(),
            Value::from(elapsed_millis(started)),
        ),
    ]);
    if let Some(millis) = retrieval.cache_read_millis {
        entry.insert("cacheReadMillis".to_owned(), Value::from(millis));
    }
    if let Some(generation) = &retrieval.generation {
        entry.insert(
            "generation".to_owned(),
            serde_json::to_value(generation).unwrap_or(Value::Null),
        );
    }
    Value::Object(entry)
}

/// Retrieves the hashes of `sha` from the cache tiers, or generates them.
///
/// `base` is the request's other revision when it has already been retrieved: in partial-graph
/// mode a full-content miss first tries to derive `sha` from it (see [`crate::partial`]).
fn get_hashes_locked(
    state: &Arc<State>,
    sha: &str,
    modified: &BTreeSet<PathBuf>,
    base: Option<(&str, &Arc<HashFileData>)>,
) -> Result<Retrieval> {
    // Keyed by the commit's tree, not the commit itself: a rebase, an amended message, or a merge
    // commit that changes no file all point at a tree that was already hashed.
    let tree = resolve_tree(state, sha)?;
    let key = cache_key(state, &tree, modified);
    let path = state.config.cache_dir.join(format!("{key}.json"));
    let mut current_dependency_fingerprint = None;
    // Bound first so the tier's lock is released before the (possibly slow) servability check.
    let remembered = memory_cache(state).get(&key);
    if let Some(data) = remembered {
        let read_started = Instant::now();
        if cache_entry_is_servable(
            state,
            sha,
            "memory",
            &key,
            &data,
            &mut current_dependency_fingerprint,
        )? {
            // Still counts as a use of the on-disk entry, so the pruner does not expire it out
            // from under an entry that is hot in memory.
            touch(&path);
            return Ok(Retrieval::hit(data, read_started));
        }
    }
    if path.is_file() {
        let read_started = Instant::now();
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read hash file {}", path.display()))?;
        let data = Arc::new(HashFileData::from_slice(&bytes)?);
        if cache_entry_is_servable(
            state,
            sha,
            "local",
            &key,
            &data,
            &mut current_dependency_fingerprint,
        )? {
            touch(&path);
            memory_cache(state).insert(&key, bytes.len() as u64, &data);
            return Ok(Retrieval::hit(data, read_started));
        }
    }
    if let Some(remote) = &state.remote {
        let read_started = Instant::now();
        if let Some(bytes) = remote.get(&key) {
            let data = Arc::new(HashFileData::from_slice(&bytes)?);
            if cache_entry_is_servable(
                state,
                sha,
                "remote",
                &key,
                &data,
                &mut current_dependency_fingerprint,
            )? {
                fs::write(&path, &bytes)?;
                memory_cache(state).insert(&key, bytes.len() as u64, &data);
                return Ok(Retrieval::hit(data, read_started));
            }
        }
    }
    if state.config.dependency_fingerprint {
        eprintln!("[BD-DBG][cache-recompute] sha={sha} key={key}");
    }
    let mut options = state.config.hash_options.clone();
    options.bazel.cancellation = state.workspace.holder();
    options.modified_filepaths = modified.clone();
    options.track_deps = state.config.track_deps;
    let mut tree = None;
    if modified.is_empty() && options.content_hashes.is_none() {
        // Git already holds a content hash for every tracked file, so seed the source hasher from
        // one `ls-tree` instead of reading the whole working tree. The scoped `modifiedFilepaths`
        // path keeps its own disk-read semantics, and files absent from the tree (untracked, or in
        // a fine-grained external repository) still fall back to a disk read.
        let listed_started = Instant::now();
        let index = git_tree_index(state, sha)?;
        eprintln!(
            "[Info] listed {} tracked blobs for {sha} in {}ms",
            index.len(),
            elapsed_millis(listed_started)
        );
        options.content_hashes = Some(index.content_hashes());
        tree = Some(Arc::new(index));
    }
    let mut partial_profile = PartialProfile::default();
    let mut derived = None;
    if let (true, Some(tree)) = (state.config.partial_graph, &tree) {
        let classify_started = Instant::now();
        let outcome = match base {
            Some((base_sha, base_data)) if base_data.partial.is_some() => {
                classify_revisions(state, base_sha, sha, base_data, tree)
                    .map(|classification| (classification, base_sha.to_owned()))
            }
            Some((base_sha, _)) => Ok((
                Classification::Rebuild {
                    reason: format!("base entry {base_sha} carries no partial-graph recipes"),
                },
                base_sha.to_owned(),
            )),
            None => Ok((
                Classification::Rebuild {
                    reason: "no base revision to derive from".to_owned(),
                },
                String::new(),
            )),
        };
        partial_profile.classify_millis = Some(elapsed_millis(classify_started));
        match outcome {
            Ok((
                Classification::Derivable {
                    changed_sources,
                    unmatched_paths,
                },
                base_sha,
            )) => {
                let derive_started = Instant::now();
                let (_, base_data) = base.expect("a derivable classification has a base");
                match partial::derive(base_data, tree) {
                    Ok(data) => {
                        eprintln!(
                            "[Info] partial graph: derived {sha} from {base_sha}: {} changed source targets, {} changed paths outside the graph, {} targets",
                            changed_sources.len(),
                            unmatched_paths.len(),
                            data.hashes.len()
                        );
                        partial_profile.mode = Some("derived");
                        partial_profile.derived_from = Some(base_sha);
                        partial_profile.changed_sources = Some(changed_sources.len());
                        partial_profile.unmatched_paths = Some(unmatched_paths.len());
                        partial_profile.derive_millis = Some(elapsed_millis(derive_started));
                        derived = Some(data);
                    }
                    Err(error) => {
                        partial_profile.partial_fallback_reason =
                            Some(format!("derivation failed: {error:#}"));
                    }
                }
            }
            Ok((Classification::Rebuild { reason }, _)) => {
                partial_profile.partial_fallback_reason = Some(reason);
            }
            Err(error) => {
                partial_profile.partial_fallback_reason =
                    Some(format!("classification failed: {error:#}"));
            }
        }
        if derived.is_none() {
            partial_profile.mode = Some("full");
            eprintln!(
                "[Info] partial graph: full generation for {sha}: {}",
                partial_profile
                    .partial_fallback_reason
                    .as_deref()
                    .unwrap_or("unknown reason")
            );
            state.partial_rebuilds.fetch_add(1, Ordering::SeqCst);
        } else {
            state.partial_derived.fetch_add(1, Ordering::SeqCst);
        }
    }
    let (mut data, timings, checkout_millis) = match derived {
        Some(data) => (data, Default::default(), 0),
        None => {
            let checkout_started = Instant::now();
            checkout(state, sha)?;
            let checkout_millis = elapsed_millis(checkout_started);
            if state.config.partial_graph {
                options.partial_tree = tree.clone();
            }
            let (data, timings) = generate_hashes_timed(&options)?;
            (data, timings, checkout_millis)
        }
    };
    // A generation the client stopped waiting for is not cached: the subprocess kill above may
    // have left the working tree mid-checkout, and the caller may already be reusing it.
    state.workspace.check_cancelled()?;
    // A derived entry already carries its base's fingerprint: the module files are unchanged.
    if state.config.dependency_fingerprint && data.dependency_fingerprint.is_none() {
        data.dependency_fingerprint = current_dependency_fingerprint.take().or_else(|| {
            match dependency_fingerprint_for_workspace(state) {
                Ok(value) => Some(value),
                Err(error) => {
                    eprintln!("[Warn] {error:#}");
                    None
                }
            }
        });
        match data.dependency_fingerprint.as_deref() {
            Some(fingerprint) => eprintln!(
                "[BD-DBG][cache-write-fingerprint] sha={sha} key={key} fp={}",
                short_fingerprint(fingerprint)
            ),
            None => eprintln!("[BD-DBG][cache-write-fingerprint] sha={sha} key={key} fp=NONE"),
        }
    }
    let write_started = Instant::now();
    let bytes = serde_json::to_vec(&data.serialized(true, state.config.track_deps))?;
    let temporary = state.config.cache_dir.join(format!("{key}.tmp"));
    fs::write(&temporary, &bytes)?;
    fs::rename(temporary, path)?;
    if let Some(remote) = &state.remote {
        remote.put(&key, &bytes);
    }
    let data = Arc::new(data);
    memory_cache(state).insert(&key, bytes.len() as u64, &data);
    let generation = GenerationBreakdown {
        checkout_millis,
        bazel_query_millis: timings.bazel_query_millis,
        target_hash_millis: timings.target_hash_millis,
        module_graph_millis: timings.module_graph_millis,
        cache_write_millis: elapsed_millis(write_started),
        target_count: data.hashes.len(),
        partial: partial_profile,
    };
    // Always logged, not just for profile=true requests, so a slow generation is attributable to
    // a phase straight from the server log.
    eprintln!(
        "[Info] generated hashes for {sha}: checkout={}ms bazelQuery={}ms targetHash={}ms moduleGraph={}ms cacheWrite={}ms targets={}{}",
        generation.checkout_millis,
        generation.bazel_query_millis,
        generation.target_hash_millis,
        generation.module_graph_millis,
        generation.cache_write_millis,
        generation.target_count,
        generation
            .partial
            .mode
            .map(|mode| format!(" partialGraph={mode}"))
            .unwrap_or_default()
    );
    Ok(Retrieval {
        data,
        cache_hit: false,
        cache_read_millis: None,
        generation: Some(generation),
    })
}

/// Decides whether a cached entry may be served for `sha`.
///
/// Without `--dependencyFingerprint` every entry is trusted, as it was before the guard existed:
/// the cache key already covers the SHA and the hash-affecting configuration. With the flag on,
/// the entry must carry a dependency fingerprint that matches the workspace's current
/// external-dependency state, which costs a checkout plus `bazel mod` calls per lookup.
fn cache_entry_is_servable(
    state: &Arc<State>,
    sha: &str,
    scope: &str,
    key: &str,
    data: &HashFileData,
    current_dependency_fingerprint: &mut Option<String>,
) -> Result<bool> {
    if !state.config.dependency_fingerprint {
        return Ok(true);
    }
    let matched = cache_entry_matches_dependency_fingerprint(
        state,
        sha,
        scope,
        data,
        current_dependency_fingerprint,
    )?;
    let outcome = if matched {
        "cache-hit"
    } else {
        "cache-guard-miss"
    };
    eprintln!("[BD-DBG][{outcome}] scope={scope} sha={sha} key={key}");
    Ok(matched)
}

fn cache_entry_matches_dependency_fingerprint(
    state: &Arc<State>,
    sha: &str,
    scope: &str,
    data: &HashFileData,
    current_dependency_fingerprint: &mut Option<String>,
) -> Result<bool> {
    let Some(cached_fingerprint) = data.dependency_fingerprint.as_deref() else {
        eprintln!("[BD-DBG][cache-guard-no-fingerprint] scope={scope} sha={sha} policy=recompute");
        return Ok(false);
    };
    if current_dependency_fingerprint.is_none() {
        checkout(state, sha)?;
        *current_dependency_fingerprint = match dependency_fingerprint_for_workspace(state) {
            Ok(value) => Some(value),
            Err(error) => {
                eprintln!("[Warn] {error:#}");
                None
            }
        };
    }
    Ok(match current_dependency_fingerprint.as_deref() {
        Some(current) => {
            let matched = current == cached_fingerprint;
            eprintln!(
                "[BD-DBG][cache-guard-check] scope={scope} sha={sha} cachedFp={} currentFp={} matched={matched}",
                short_fingerprint(cached_fingerprint),
                short_fingerprint(current)
            );
            matched
        }
        None => {
            eprintln!(
                "[BD-DBG][cache-guard-check] scope={scope} sha={sha} cachedFp={} currentFp=NONE matched=false",
                short_fingerprint(cached_fingerprint)
            );
            false
        }
    })
}

fn short_fingerprint(value: &str) -> &str {
    &value[..value.len().min(12)]
}

fn dependency_fingerprint_for_workspace(state: &State) -> Result<String> {
    state
        .bazel_options()
        .dependency_fingerprint()
        .map_err(|error| {
            anyhow!("failed to compute dependency fingerprint for cache guard: {error:#}")
        })
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
            .bazel
            .label_only_module_extensions
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
        // Full-content entries hash git blob ids rather than file bytes; keep them apart from
        // entries a build without this seeding wrote under the same package version.
        String::from("sourceContent=git-blob"),
    ] {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    if config.partial_graph {
        // Partial-graph entries hash under a different formula and carry recipes; they are never
        // compared with, or served in place of, an entry of the default mode. Only folded in when
        // enabled so existing keys do not move.
        hasher.update(format!("partialGraph={}", partial::RECIPE_VERSION).as_bytes());
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
        bail!("revision '{revision}' is missing from the local clone");
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if sha.len() != 40 {
        bail!("revision '{revision}' resolved to invalid SHA '{sha}'");
    }
    Ok(sha)
}

/// Resolves the tree object a commit points at; distinct commits with identical content share it.
fn resolve_tree(state: &State, sha: &str) -> Result<String> {
    let output = git_output(
        state,
        &[
            String::from("rev-parse"),
            String::from("--verify"),
            format!("{sha}^{{tree}}"),
        ],
    )?;
    if !output.status.success() {
        bail!("tree for revision '{sha}' is missing from the local clone");
    }
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if tree.len() != 40 {
        bail!("revision '{sha}' resolved to invalid tree '{tree}'");
    }
    Ok(tree)
}

/// Every tracked path at `sha` with its object id and mode, from one `git ls-tree`.
/// [`TreeIndex::content_hashes`] is the `{path: blob}` map `--contentHashPath` accepts.
fn git_tree_index(state: &State, sha: &str) -> Result<TreeIndex> {
    let output = git_success(
        state,
        &[
            String::from("ls-tree"),
            String::from("-r"),
            String::from("-z"),
            sha.to_owned(),
        ],
    )?;
    Ok(TreeIndex::parse_ls_tree(&output))
}

/// Decides whether `sha` can be derived from the cached `base_sha` entry: the raw diff between
/// the two, the module and workspace files' label references at `sha`, and the server's
/// rebuild patterns and seed files go through [`partial::classify`].
fn classify_revisions(
    state: &State,
    base_sha: &str,
    sha: &str,
    base: &HashFileData,
    tree: &TreeIndex,
) -> Result<Classification> {
    let diff = git_success(
        state,
        &[
            String::from("diff"),
            String::from("--raw"),
            String::from("--no-renames"),
            String::from("-z"),
            base_sha.to_owned(),
            sha.to_owned(),
        ],
    )?;
    let diff = partial::parse_diff_raw(&diff)?;
    let mut module_referenced = BTreeSet::new();
    for file in MODULE_FILES {
        if tree.get(file).is_none() {
            continue;
        }
        let text = git_success(state, &[String::from("show"), format!("{sha}:{file}")])?;
        module_referenced.extend(partial::referenced_main_repo_paths(
            &String::from_utf8_lossy(&text),
        ));
    }
    Ok(partial::classify(&ClassifyInputs {
        diff: &diff,
        tree,
        base,
        seed_filepaths: &state.config.hash_options.seed_filepaths,
        rebuild_patterns: &state.config.partial_graph_rebuild_patterns,
        module_referenced: &module_referenced,
    }))
}

/// Runs git and returns its stdout, failing on a non-zero exit with the command and stderr.
fn git_success(state: &State, args: &[String]) -> Result<Vec<u8>> {
    let output = git_output(state, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

fn checkout(state: &State, sha: &str) -> Result<()> {
    if let Ok(output) = git_output(state, &[String::from("rev-parse"), String::from("HEAD")]) {
        if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == sha {
            return Ok(());
        }
    }
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
    eprintln!(
        "[Warn] cleared stale git index.lock in {}; retrying checkout of {sha}",
        state.config.hash_options.bazel.workspace.display()
    );
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

/// Runs git in the workspace. While a query holds the workspace the subprocess belongs to it and
/// is killed if that query is cancelled; outside the lock (startup fetch) it runs plainly.
fn git_output(state: &State, args: &[String]) -> Result<std::process::Output> {
    let mut command = Command::new(&state.config.git_path);
    command
        .current_dir(&state.config.hash_options.bazel.workspace)
        .args(args)
        .stdin(Stdio::null());
    cancel::output(state.workspace.holder().as_deref(), &mut command)
        .with_context(|| format!("failed to execute {}", state.config.git_path.display()))
}

fn metrics(state: &State) -> Value {
    let entries = cache_entries(&state.config.cache_dir);
    let total = entries.iter().map(|entry| entry.size).sum::<u64>();
    let (used, max) = memory_usage();
    let (memory_entries, memory_used, memory_max) = {
        let memory = memory_cache(state);
        (memory.len(), memory.used, memory.budget)
    };
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptimeSeconds": state.started.elapsed().as_secs(),
        "ready": state.ready.load(Ordering::Acquire),
        "activeQueries": state.active_queries.load(Ordering::SeqCst),
        "workspaceBusy": state.workspace.is_busy(),
        "gitEngine": "subprocess",
        "trackDeps": state.config.track_deps,
        "dependencyFingerprint": state.config.dependency_fingerprint,
        "partialGraph": {
            "enabled": state.config.partial_graph,
            "derived": state.partial_derived.load(Ordering::SeqCst),
            "fullGenerations": state.partial_rebuilds.load(Ordering::SeqCst),
        },
        "cache": {
            "directory": state.config.cache_dir.to_string_lossy(),
            "remote": state.config.remote_cache,
            "entries": entries.len(),
            "sizeBytes": total,
            "sizeHuman": human_bytes(total),
            "memory": {
                "entries": memory_entries,
                "sizeBytes": memory_used,
                "maxBytes": memory_max,
            },
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
    // Refreshes the modification time pruning reads as last use. Opening for write without
    // truncating leaves the bytes alone; rewriting them cost a full copy of the entry per hit.
    if let Ok(file) = fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
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
                    label_only_module_extensions: BTreeSet::new(),
                    exclude_external_targets: false,
                    exclude_targets_query: None,
                    no_bazelrc: false,
                    verbose: false,
                    cancellation: None,
                },
                content_hashes: None,
                ignored_attributes: HashSet::new(),
                seed_filepaths: BTreeSet::new(),
                modified_filepaths: BTreeSet::new(),
                track_deps: false,
                always_affected_tags: HashSet::new(),
                partial_tree: None,
            },
            git_path: PathBuf::from("git"),
            port: 0,
            request_timeout: Duration::ZERO,
            cache_dir: PathBuf::from("."),
            track_deps: false,
            dependency_fingerprint: false,
            partial_graph: false,
            partial_graph_rebuild_patterns: Vec::new(),
            no_initial_fetch: true,
            warmup_revisions: Vec::new(),
            cache_max_age: None,
            cache_max_entries: None,
            cache_max_size: None,
            cache_prune_interval: Duration::from_secs(60),
            cache_memory_max: 64 * 1024 * 1024,
            remote_cache: None,
            s3_bucket: None,
            s3_prefix: String::new(),
            s3_region: None,
            s3_endpoint: None,
            s3_force_path_style: false,
            nexus_url: None,
            nexus_repository: None,
            nexus_prefix: String::new(),
            nexus_credentials: None,
            nexus_ca_cert: None,
        }
    }

    fn hash(kind: &str, value: &str, direct: &str) -> TargetHash {
        TargetHash {
            kind: kind.to_owned(),
            hash: value.into(),
            direct_hash: direct.into(),
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
        config.dependency_fingerprint = true;
        ready_state(config, None)
    }

    fn ready_state(config: ServerConfig, remote: Option<RemoteCache>) -> Arc<State> {
        let state = State::new(config, remote);
        state.ready.store(true, Ordering::Release);
        Arc::new(state)
    }

    /// The full-content cache key `get_hashes_locked` uses for a commit: its tree, not its SHA.
    fn cache_key_for_commit(state: &State, sha: &str) -> String {
        cache_key(state, &resolve_tree(state, sha).unwrap(), &BTreeSet::new())
    }

    #[test]
    fn cache_key_is_shared_by_commits_with_identical_trees() {
        let (repo, first, second) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), false);
        git_command(
            repo.path(),
            &["commit", "-q", "--allow-empty", "-m", "no file changes"],
        );
        let third = git_command(repo.path(), &["rev-parse", "HEAD"]);
        assert_ne!(second, third);
        assert_eq!(
            cache_key_for_commit(&state, &second),
            cache_key_for_commit(&state, &third)
        );
        assert_ne!(
            cache_key_for_commit(&state, &first),
            cache_key_for_commit(&state, &second)
        );
        assert!(cache_key_for_commit(&state, &second)
            .starts_with(&git_command(repo.path(), &["rev-parse", "HEAD^{tree}"])));
        assert!(resolve_tree(&state, "0000000000000000000000000000000000000000").is_err());
    }

    #[test]
    fn git_content_hashes_list_tracked_blobs_and_skip_submodules() {
        let (repo, first, second) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), false);
        fs::create_dir_all(repo.path().join("nested dir")).unwrap();
        fs::write(repo.path().join("nested dir/with space.txt"), "three").unwrap();
        git_command(repo.path(), &["add", "."]);
        git_command(repo.path(), &["commit", "-q", "-m", "three"]);
        let third = git_command(repo.path(), &["rev-parse", "HEAD"]);

        let hashes = git_tree_index(&state, &third).unwrap().content_hashes();
        assert_eq!(hashes.len(), 2);
        assert_eq!(
            hashes["file.txt"],
            git_command(repo.path(), &["rev-parse", &format!("{third}:file.txt")])
        );
        assert_eq!(
            hashes["nested dir/with space.txt"],
            git_command(
                repo.path(),
                &["rev-parse", &format!("{third}:nested dir/with space.txt")]
            )
        );
        // Same path, different content: the blob id moves with it.
        assert_ne!(
            git_tree_index(&state, &first).unwrap().content_hashes()["file.txt"],
            git_tree_index(&state, &second).unwrap().content_hashes()["file.txt"]
        );
        assert!(git_tree_index(&state, "not-a-revision").is_err());
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
            RemoteCache::S3(S3Cache {
                bucket,
                prefix: normalize_s3_prefix(prefix),
            }),
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

        let mut label_only = base.clone();
        label_only
            .hash_options
            .bazel
            .label_only_module_extensions
            .insert("@rules_rs//rs:extensions.bzl%crate".to_owned());
        assert_ne!(configuration_fingerprint(&label_only), base_fingerprint);

        let mut always_affected = base;
        always_affected
            .hash_options
            .always_affected_tags
            .insert("external".to_owned());
        assert_ne!(
            configuration_fingerprint(&always_affected),
            base_fingerprint
        );

        // Partial-graph entries hash under their own formula; the default mode's keys stay put.
        let mut partial = test_config();
        assert_eq!(configuration_fingerprint(&partial), base_fingerprint);
        partial.partial_graph = true;
        assert_ne!(configuration_fingerprint(&partial), base_fingerprint);
        partial.partial_graph_rebuild_patterns = vec![Glob::new("*.lock").unwrap()];
        assert_eq!(
            configuration_fingerprint(&partial),
            configuration_fingerprint(&ServerConfig {
                partial_graph_rebuild_patterns: Vec::new(),
                ..partial.clone()
            }),
            "rebuild patterns only steer the classifier, never the hashes"
        );
    }

    #[test]
    fn partial_graph_rejects_configurations_it_cannot_derive() {
        let mut config = test_config();
        assert!(validate_partial_graph(&config).is_ok());
        config.partial_graph = true;
        assert!(validate_partial_graph(&config).is_ok());

        let mut cquery = config.clone();
        cquery.hash_options.bazel.use_cquery = true;
        assert!(validate_partial_graph(&cquery)
            .unwrap_err()
            .to_string()
            .contains("--useCquery"));

        let mut fine_grained = config.clone();
        fine_grained
            .hash_options
            .bazel
            .fine_grained_external_repos
            .insert("@crates".into());
        assert!(validate_partial_graph(&fine_grained)
            .unwrap_err()
            .to_string()
            .contains("--fineGrainedHashExternalRepos"));

        let mut always_affected = config;
        always_affected
            .hash_options
            .always_affected_tags
            .insert("flaky".into());
        assert!(validate_partial_graph(&always_affected)
            .unwrap_err()
            .to_string()
            .contains("--alwaysAffectedTags"));
    }

    /// A stand-in `bazel` whose query answers with the `targets.pb` checked out in the
    /// workspace, so the graph a revision reports is whatever that revision committed.
    #[cfg(unix)]
    fn fake_bazel(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = directory.join("fake-bazel.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
case "$1 $2" in
  "mod dump_repo_mapping") printf '%s\n' '{"tool":"local_config_tool"}' ;;
  "mod graph") printf '%s\n' '{"key":"<root>","name":"root","dependencies":[]}' ;;
  "version ") printf '%s\n' 'Build label: 8.7.0' ;;
  "info output_base") printf '%s\n' "$PWD" ;;
  "query "*)
    while [ "$1" != --output_file ]; do shift; done
    cp "$PWD/targets.pb" "$2"
    ;;
  *) exit 80 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Two source files, a rule reading one, a rule reading the other plus the first rule, and
    /// that rule's output: enough shape for a source edit to propagate direct and transitively.
    fn encoded_targets() -> Vec<u8> {
        use crate::proto::blaze_query::{target, GeneratedFile, Rule, SourceFile, Target};
        use buffa::Message;
        let source = |name: &str| {
            let mut target = Target::default();
            target.r#type = target::Discriminator::SourceFile;
            target.source_file = buffa::MessageField::some(SourceFile {
                name: name.to_owned(),
                subinclude: vec!["//tools:defs.bzl".to_owned()],
                ..Default::default()
            });
            target
        };
        let rule = |name: &str, inputs: &[&str]| {
            let mut target = Target::default();
            target.r#type = target::Discriminator::Rule;
            target.rule = buffa::MessageField::some(Rule {
                name: name.to_owned(),
                rule_class: "genrule".to_owned(),
                rule_input: inputs.iter().map(|input| (*input).to_owned()).collect(),
                ..Default::default()
            });
            target
        };
        let generated = |name: &str, generating_rule: &str| {
            let mut target = Target::default();
            target.r#type = target::Discriminator::GeneratedFile;
            target.generated_file = buffa::MessageField::some(GeneratedFile {
                name: name.to_owned(),
                generating_rule: generating_rule.to_owned(),
                ..Default::default()
            });
            target
        };
        let mut encoded = Vec::new();
        for target in [
            source("//pkg:a.txt"),
            source("//pkg:b.txt"),
            rule("//pkg:lib", &["//pkg:a.txt"]),
            rule("//pkg:bin", &["//pkg:lib", "//pkg:b.txt"]),
            generated("//pkg:bin.out", "//pkg:bin"),
        ] {
            target.encode_length_delimited(&mut encoded);
        }
        encoded
    }

    /// A repository with the fixed graph above and four commits: `base`; a source and a README
    /// edit; a `BUILD` edit on top of that; and, branching from the first edit, another source
    /// edit.
    #[cfg(unix)]
    fn partial_graph_repo() -> (tempfile::TempDir, PathBuf, [String; 4]) {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(repo.join("pkg")).unwrap();
        fs::create_dir_all(repo.join("tools")).unwrap();
        let bazel = fake_bazel(root.path());
        git_command(&repo, &["init", "-q"]);
        git_command(&repo, &["config", "user.email", "test@example.com"]);
        git_command(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("pkg/a.txt"), "one").unwrap();
        fs::write(repo.join("pkg/b.txt"), "bee").unwrap();
        fs::write(repo.join("pkg/BUILD.bazel"), "# build").unwrap();
        fs::write(repo.join("tools/defs.bzl"), "def macro(): pass").unwrap();
        fs::write(repo.join("README.md"), "readme").unwrap();
        fs::write(repo.join("targets.pb"), encoded_targets()).unwrap();
        let commit = |message: &str| {
            git_command(&repo, &["add", "-A"]);
            git_command(&repo, &["commit", "-q", "-m", message]);
            git_command(&repo, &["rev-parse", "HEAD"])
        };
        let base = commit("base");
        fs::write(repo.join("pkg/a.txt"), "two").unwrap();
        fs::write(repo.join("README.md"), "readme two").unwrap();
        let edited = commit("source edit");
        fs::write(repo.join("pkg/a.txt"), "three").unwrap();
        fs::write(repo.join("pkg/BUILD.bazel"), "# build two").unwrap();
        let rebuilt = commit("build edit");
        git_command(&repo, &["checkout", "-q", &edited]);
        fs::write(repo.join("pkg/a.txt"), "four").unwrap();
        let chained = commit("second source edit");
        (root, bazel, [base, edited, rebuilt, chained])
    }

    fn partial_graph_state(repo: &Path, cache: &Path, bazel: &Path, partial: bool) -> Arc<State> {
        let mut config = test_config();
        config.hash_options.bazel.workspace = repo.to_path_buf();
        config.hash_options.bazel.bazel = bazel.to_path_buf();
        config.hash_options.bazel.no_bazelrc = false;
        config.hash_options.track_deps = true;
        config.track_deps = true;
        config.cache_dir = cache.to_path_buf();
        config.partial_graph = partial;
        ready_state(config, None)
    }

    #[cfg(unix)]
    #[test]
    fn partial_graph_derives_source_edits_identically_to_a_full_generation() {
        let (root, bazel, [base, edited, rebuilt, chained]) = partial_graph_repo();
        let repo = root.path().join("repo");
        let cache = root.path().join("cache");
        let verify_cache = root.path().join("verify-cache");
        let plain_cache = root.path().join("plain-cache");
        for directory in [&cache, &verify_cache, &plain_cache] {
            fs::create_dir_all(directory).unwrap();
        }
        let state = partial_graph_state(&repo, &cache, &bazel, true);

        // The first revision has nothing to derive from: a full generation that records recipes.
        let from = get_hashes_locked(&state, &base, &BTreeSet::new(), None).unwrap();
        let generation = from.generation.as_ref().unwrap();
        assert_eq!(generation.partial.mode, Some("full"));
        assert!(generation
            .partial
            .partial_fallback_reason
            .as_deref()
            .unwrap()
            .contains("no base revision"));
        let recipes = from.data.partial.as_ref().unwrap();
        assert_eq!(recipes.rules.len(), 2);
        assert_eq!(recipes.generated["//pkg:bin.out"], "//pkg:bin");
        assert_eq!(recipes.rules["//pkg:bin"].sources(), ["//pkg:b.txt"]);
        assert_eq!(recipes.rules["//pkg:bin"].deps(), ["//pkg:lib"]);
        assert_eq!(recipes.package_subincludes["//pkg"], ["//tools:defs.bzl"]);

        // A source edit plus a README edit: derived, no checkout, no query.
        let to = get_hashes_locked(&state, &edited, &BTreeSet::new(), Some((&base, &from.data)))
            .unwrap();
        let generation = to.generation.as_ref().unwrap();
        assert_eq!(generation.partial.mode, Some("derived"));
        assert_eq!(
            generation.partial.derived_from.as_deref(),
            Some(base.as_str())
        );
        assert_eq!(generation.partial.changed_sources, Some(1));
        assert_eq!(generation.partial.unmatched_paths, Some(1));
        assert_eq!(generation.bazel_query_millis, 0);
        assert_eq!(generation.checkout_millis, 0);
        assert!(generation.partial.derive_millis.is_some());
        assert_eq!(
            git_command(&repo, &["rev-parse", "HEAD"]),
            base,
            "a derivation leaves the working tree where it was"
        );
        let impacted = impacted_targets(&from.data.hashes, &to.data.hashes, None, false)
            .unwrap()
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            impacted,
            ["//pkg:a.txt", "//pkg:lib", "//pkg:bin", "//pkg:bin.out"]
                .map(str::to_owned)
                .into_iter()
                .collect()
        );
        assert!(
            to.data.hashes["//pkg:lib"].direct_hash != from.data.hashes["//pkg:lib"].direct_hash
        );
        assert!(
            to.data.hashes["//pkg:bin"].direct_hash == from.data.hashes["//pkg:bin"].direct_hash
        );

        // A full generation of the same revision, in a fresh cache, writes the identical entry.
        let verify = partial_graph_state(&repo, &verify_cache, &bazel, true);
        let full = get_hashes_locked(&verify, &edited, &BTreeSet::new(), None).unwrap();
        assert_eq!(full.generation.as_ref().unwrap().partial.mode, Some("full"));
        assert_eq!(full.data.hashes, to.data.hashes);
        assert_eq!(full.data.dep_edges, to.data.dep_edges);
        assert_eq!(full.data.partial, to.data.partial);
        let key = cache_key_for_commit(&state, &edited);
        assert_eq!(
            fs::read(cache.join(format!("{key}.json"))).unwrap(),
            fs::read(verify_cache.join(format!("{key}.json"))).unwrap(),
            "derived and generated entries must be byte-identical"
        );

        // A BUILD edit forces a full generation.
        let rebuilt_entry = get_hashes_locked(
            &state,
            &rebuilt,
            &BTreeSet::new(),
            Some((&edited, &to.data)),
        )
        .unwrap();
        let generation = rebuilt_entry.generation.as_ref().unwrap();
        assert_eq!(generation.partial.mode, Some("full"));
        assert!(generation
            .partial
            .partial_fallback_reason
            .as_deref()
            .unwrap()
            .contains("graph-defining file pkg/BUILD.bazel"));
        assert!(generation.partial.classify_millis.is_some());

        // A derived entry is itself a base for the next derivation.
        let chained_entry = get_hashes_locked(
            &state,
            &chained,
            &BTreeSet::new(),
            Some((&edited, &to.data)),
        )
        .unwrap();
        assert_eq!(
            chained_entry.generation.as_ref().unwrap().partial.mode,
            Some("derived")
        );
        let chained_full = get_hashes_locked(&verify, &chained, &BTreeSet::new(), None).unwrap();
        assert_eq!(chained_full.data.hashes, chained_entry.data.hashes);

        // A base without recipes (an entry the default mode wrote) cannot be derived from.
        let plain = partial_graph_state(&repo, &plain_cache, &bazel, false);
        let plain_entry = get_hashes_locked(&plain, &base, &BTreeSet::new(), None).unwrap();
        assert!(plain_entry.data.partial.is_none());
        assert!(plain_entry
            .generation
            .as_ref()
            .unwrap()
            .partial
            .mode
            .is_none());
        assert_ne!(
            plain_entry.data.hashes["//pkg:lib"].hash, from.data.hashes["//pkg:lib"].hash,
            "the two modes hash under different formulas"
        );
        let unrecorded = get_hashes_locked(
            &verify,
            &base,
            &BTreeSet::new(),
            Some((&edited, &plain_entry.data)),
        )
        .unwrap();
        assert!(unrecorded
            .generation
            .as_ref()
            .unwrap()
            .partial
            .partial_fallback_reason
            .as_deref()
            .unwrap()
            .contains("carries no partial-graph recipes"));

        let snapshot = metrics(&state);
        assert_eq!(snapshot["partialGraph"]["enabled"], true);
        assert_eq!(snapshot["partialGraph"]["derived"], 2);
        assert_eq!(snapshot["partialGraph"]["fullGenerations"], 2);
        assert_eq!(metrics(&plain)["partialGraph"]["enabled"], false);
    }

    #[test]
    fn normalizes_remote_cache_configuration() {
        assert_eq!(normalize_s3_prefix(""), "");
        assert_eq!(normalize_s3_prefix("/"), "");
        assert_eq!(normalize_s3_prefix("/team/cache/"), "team/cache/");
        assert!(RemoteCache::new(&test_config()).unwrap().is_none());
    }

    #[test]
    fn remote_cache_operations_use_normalized_key_and_succeed() {
        let (remote, requests, handle) = remote_cache_with_responses(
            "/team/repo/",
            vec![(200, gzip(b"hello").unwrap()), (200, Vec::new())],
        );
        let RemoteCache::S3(s3) = &remote else {
            panic!("expected the S3 backend");
        };
        assert_eq!(s3.object_key("sha.fp.json.gz"), "team/repo/sha.fp.json.gz");
        assert_eq!(remote.get("sha.fp"), Some(b"hello".to_vec()));
        remote.put("sha.fp", b"data");
        let captured = (0..2)
            .map(|_| requests.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect::<Vec<_>>();
        handle.join().unwrap();
        assert!(captured
            .iter()
            .all(|request| request.url.contains("/bucket/team/repo/sha.fp.json.gz")));
    }

    #[test]
    fn remote_cache_falls_back_to_plain_entries_and_rejects_corrupt_compressed_ones() {
        // A replica that predates compression wrote `<key>.json`: the compressed name misses
        // cleanly and the plain one is read. A compressed object that does not decode is a miss,
        // with no fallback (the plain name would be a stale duplicate at best).
        let (remote, requests, handle) = remote_cache_with_responses(
            "",
            vec![
                (404, b"<Error><Code>NoSuchKey</Code></Error>".to_vec()),
                (200, b"plain".to_vec()),
                (200, b"not gzip".to_vec()),
            ],
        );
        assert_eq!(remote.get("legacy"), Some(b"plain".to_vec()));
        assert_eq!(remote.get("corrupt"), None);
        let captured = (0..3)
            .map(|_| requests.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect::<Vec<_>>();
        handle.join().unwrap();
        assert!(captured[0].url.ends_with("/legacy.json.gz"));
        assert!(captured[1].url.ends_with("/legacy.json"));
        assert!(captured[2].url.ends_with("/corrupt.json.gz"));
        assert!(requests.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn gzip_round_trips_and_shrinks_hash_entries() {
        let entry = (0..2000)
            .map(|index| {
                format!(
                    "\"//pkg/{}:target_{index}\":\"Rule#{:064x}~{:064x}\"",
                    index / 8,
                    index,
                    index * 7
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let entry = format!("{{{entry}}}").into_bytes();
        let compressed = gzip(&entry).unwrap();
        assert!(compressed.len() * 2 < entry.len());
        assert_eq!(gunzip(&compressed).unwrap(), entry);
        assert!(gunzip(b"not gzip").is_err());
    }

    #[test]
    fn remote_cache_misses_and_errors_degrade_without_failing() {
        let (remote, requests, handle) = remote_cache_with_responses(
            "",
            vec![
                (404, b"<Error><Code>NoSuchKey</Code></Error>".to_vec()),
                (404, b"<Error><Code>NoSuchKey</Code></Error>".to_vec()),
                (500, b"failure".to_vec()),
                (500, Vec::new()),
            ],
        );
        assert_eq!(remote.get("missing"), None);
        assert_eq!(remote.get("failure"), None);
        remote.put("failure", b"data");
        // A clean miss probes the compressed and then the plain name; a failure is one request
        // and no fallback. `rust-s3`'s `fail-on-err` feature is off precisely so a non-2xx stays
        // a response: as an error it would enter the crate's `retry!`, and every miss would cost
        // a second request and a one-second backoff sleep.
        for _ in 0..4 {
            requests.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert!(requests.recv_timeout(Duration::from_millis(200)).is_err());
        handle.join().unwrap();
    }

    #[test]
    fn remote_cache_reports_unexpected_statuses_with_their_body() {
        let response = ResponseData::new("denied".into(), 403, std::collections::HashMap::new());
        assert_eq!(
            http_failure(&response),
            "Got HTTP 403 with content 'denied'"
        );
    }

    const TEST_CA_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBkTCCATegAwIBAgIUaqMU9czNCvWG1R457FBnv8BzZfQwCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwSYmF6ZWwtZGlmZi10ZXN0LWNhMCAXDTI2MTAwNTE0NTUwOVoY
DzIxMjYwOTExMTQ1NTA5WjAdMRswGQYDVQQDDBJiYXplbC1kaWZmLXRlc3QtY2Ew
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASRkDRNQ1MfFy+8eM1z9tY6FseE+Bqs
p3hQcwR/2L3wzZKwFWhaDmfn5WFKAINVfcGxOAKWKcXrhK+xWOz9KmYoo1MwUTAd
BgNVHQ4EFgQUzsQ/MEApeEP6b/jW5ecBMceQbJcwHwYDVR0jBBgwFoAUzsQ/MEAp
eEP6b/jW5ecBMceQbJcwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNIADBF
AiEAixF88BBeQUzoM0fYxa/ivGx5U4kHdLW82V9LIFiOV14CIG/00V8ZKfomL0v0
nQBeQ4AIl47nDOrFeVnvse8R9IHJ
-----END CERTIFICATE-----
";

    /// A canned Nexus reply: status, extra headers, body.
    type NexusReply = (u16, Vec<(&'static str, &'static str)>, Vec<u8>);

    struct CapturedNexusRequest {
        method: Method,
        url: String,
        authorization: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    fn nexus_cache_with_responses(
        prefix: &str,
        credentials: Option<NexusCredentials>,
        responses: Vec<NexusReply>,
    ) -> (
        RemoteCache,
        std_mpsc::Receiver<CapturedNexusRequest>,
        thread::JoinHandle<()>,
    ) {
        let server = Server::http("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/nexus", server.server_addr().to_ip().unwrap());
        let (sender, receiver) = std_mpsc::channel();
        let handle = thread::spawn(move || {
            for (status, headers, body) in responses {
                let mut request = server.recv().unwrap();
                let header = |name: &'static str| {
                    request
                        .headers()
                        .iter()
                        .find(|header| header.field.equiv(name))
                        .map(|header| header.value.as_str().to_owned())
                };
                let authorization = header("Authorization");
                let content_type = header("Content-Type");
                let mut received = Vec::new();
                request.as_reader().read_to_end(&mut received).unwrap();
                sender
                    .send(CapturedNexusRequest {
                        method: request.method().clone(),
                        url: request.url().to_owned(),
                        authorization,
                        content_type,
                        body: received,
                    })
                    .unwrap();
                let mut response = Response::from_data(body).with_status_code(StatusCode(status));
                for (name, value) in headers {
                    response.add_header(Header::from_bytes(name, value).unwrap());
                }
                request.respond(response).unwrap();
            }
        });
        let base = nexus_base_url(&endpoint, "bazel-diff", prefix).unwrap();
        (
            RemoteCache::Nexus(Box::new(NexusCache::new(base, credentials, None).unwrap())),
            receiver,
            handle,
        )
    }

    #[test]
    fn nexus_base_url_composes_repository_and_prefix() {
        let url = |base: &str, repository: &str, prefix: &str| {
            nexus_base_url(base, repository, prefix).map(|url| url.to_string())
        };
        assert_eq!(
            url("https://nexus.example.com", "cache", "").unwrap(),
            "https://nexus.example.com/repository/cache/"
        );
        assert_eq!(
            url(
                "https://nexus.example.com/nexus/?x=1#y",
                "cache",
                "/team//repo/"
            )
            .unwrap(),
            "https://nexus.example.com/nexus/repository/cache/team/repo/"
        );
        assert_eq!(
            url("http://host:8081", "cache", "a b").unwrap(),
            "http://host:8081/repository/cache/a%20b/"
        );
        for (base, repository) in [
            ("not a url", "cache"),
            ("ftp://host", "cache"),
            ("mailto:ops@example.com", "cache"),
            ("https://user:pass@host", "cache"),
            ("https://user@host", "cache"),
            ("https://host", ""),
            ("https://host", "a/b"),
        ] {
            assert!(url(base, repository, "").is_err(), "{base} {repository}");
        }
    }

    #[test]
    fn nexus_cache_is_selected_from_configuration() {
        let mut config = test_config();
        config.nexus_url = Some("https://nexus.example.com".to_owned());
        config.nexus_repository = Some("cache".to_owned());
        config.nexus_prefix = "team".to_owned();
        let Some(RemoteCache::Nexus(cache)) = RemoteCache::new(&config).unwrap() else {
            panic!("expected the Nexus backend");
        };
        assert_eq!(
            cache.object_url("sha.fp.json.gz").as_str(),
            "https://nexus.example.com/repository/cache/team/sha.fp.json.gz"
        );
        config.nexus_repository = Some("a/b".to_owned());
        assert!(RemoteCache::new(&config).is_err());
    }

    #[test]
    fn nexus_cache_reads_and_writes_with_basic_auth() {
        let (remote, requests, handle) = nexus_cache_with_responses(
            "/team/repo/",
            Some(NexusCredentials {
                username: "svc".to_owned(),
                password: "s3cret".to_owned(),
            }),
            vec![
                (200, Vec::new(), gzip(b"hello").unwrap()),
                (404, Vec::new(), Vec::new()),
                (404, Vec::new(), Vec::new()),
                (201, Vec::new(), Vec::new()),
            ],
        );
        assert_eq!(remote.get("sha.fp"), Some(b"hello".to_vec()));
        assert_eq!(remote.get("missing"), None);
        remote.put("sha.fp", b"data");
        handle.join().unwrap();
        let captured = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(captured.len(), 4);
        let methods = captured
            .iter()
            .map(|r| r.method.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            [Method::Get, Method::Get, Method::Get, Method::Put]
        );
        assert_eq!(
            captured[0].url,
            "/nexus/repository/bazel-diff/team/repo/sha.fp.json.gz"
        );
        assert_eq!(
            captured[1].url,
            "/nexus/repository/bazel-diff/team/repo/missing.json.gz"
        );
        assert_eq!(
            captured[2].url,
            "/nexus/repository/bazel-diff/team/repo/missing.json"
        );
        assert_eq!(captured[3].url, captured[0].url);
        for request in &captured {
            // base64("svc:s3cret")
            assert_eq!(
                request.authorization.as_deref(),
                Some("Basic c3ZjOnMzY3JldA==")
            );
        }
        assert_eq!(
            captured[3].content_type.as_deref(),
            Some("application/gzip")
        );
        assert_eq!(gunzip(&captured[3].body).unwrap(), b"data");
    }

    #[test]
    fn nexus_cache_failures_degrade_without_following_redirects() {
        let (remote, requests, handle) = nexus_cache_with_responses(
            "",
            None,
            vec![
                (500, Vec::new(), "x".repeat(1000).into_bytes()),
                (401, Vec::new(), b"unauthorized".to_vec()),
                (
                    302,
                    vec![("Location", "http://127.0.0.1:9/elsewhere")],
                    Vec::new(),
                ),
                (
                    400,
                    Vec::new(),
                    b"Repository does not allow updating assets".to_vec(),
                ),
            ],
        );
        assert_eq!(remote.get("failure"), None);
        assert_eq!(remote.get("denied"), None);
        assert_eq!(remote.get("moved"), None);
        remote.put("immutable", b"data");
        handle.join().unwrap();
        let captured = requests.try_iter().collect::<Vec<_>>();
        // Exactly one request per call: nothing is retried and the redirect is not followed.
        assert_eq!(captured.len(), 4);
        assert!(captured
            .iter()
            .all(|request| request.authorization.is_none()));
    }

    #[test]
    fn nexus_cache_transport_errors_degrade_without_failing() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let base = nexus_base_url(&endpoint, "cache", "").unwrap();
        let remote = RemoteCache::Nexus(Box::new(NexusCache::new(base, None, None).unwrap()));
        assert_eq!(remote.get("unreachable"), None);
        remote.put("unreachable", b"data");
    }

    #[test]
    fn nexus_http_failure_truncates_long_bodies() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let handle = thread::spawn(move || {
            let request = server.recv().unwrap();
            request
                .respond(
                    Response::from_data(format!("  {}  ", "é".repeat(300)))
                        .with_status_code(StatusCode(503)),
                )
                .unwrap();
        });
        let response = attohttpc::get(format!("http://{address}/")).send().unwrap();
        let failure = nexus_http_failure(response);
        handle.join().unwrap();
        assert_eq!(
            failure,
            format!("Got HTTP 503 with content '{}...'", "é".repeat(200))
        );
    }

    #[test]
    fn nexus_credentials_debug_redacts_password() {
        let credentials = NexusCredentials {
            username: "svc".to_owned(),
            password: "s3cret".to_owned(),
        };
        let rendered = format!("{credentials:?}");
        assert!(rendered.contains("svc"));
        assert!(!rendered.contains("s3cret"));
    }

    #[test]
    fn nexus_ca_certificates_load_from_pem_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("ca.pem");
        fs::write(&bundle, format!("{TEST_CA_PEM}{TEST_CA_PEM}")).unwrap();
        assert_eq!(read_pem_certificates(&bundle).unwrap().len(), 2);
        let base = nexus_base_url("https://nexus.example.com", "cache", "").unwrap();
        assert!(NexusCache::new(base.clone(), None, Some(&bundle)).is_ok());

        let empty = directory.path().join("empty.pem");
        fs::write(&empty, "not a certificate").unwrap();
        assert!(read_pem_certificates(&empty).is_err());
        let corrupt = directory.path().join("corrupt.pem");
        fs::write(
            &corrupt,
            "-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        assert!(read_pem_certificates(&corrupt).is_err());
        assert!(read_pem_certificates(&directory.path().join("missing.pem")).is_err());
        assert!(NexusCache::new(base, None, Some(&empty)).is_err());
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
    fn initial_fetch_failure_lame_ducks_instead_of_exiting() {
        let (repo, _first, _second) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        // A remote nobody listens on: the startup fetch must fail, and the instance must stay
        // un-ready (503 health, 503 queries, /metrics still served) rather than crash.
        git_command(
            repo.path(),
            &["remote", "add", "origin", "git://127.0.0.1:1/nothing.git"],
        );
        let unready_state = |no_initial_fetch: bool| {
            let mut config = test_config();
            config.no_initial_fetch = no_initial_fetch;
            config.hash_options.bazel.workspace = repo.path().to_path_buf();
            config.git_path = PathBuf::from("git");
            config.cache_dir = cache.path().to_path_buf();
            Arc::new(State::new(config, None))
        };

        let lame_duck = unready_state(false);
        perform_initial_fetch(&lame_duck);
        assert!(!lame_duck.ready.load(Ordering::Acquire));
        assert!(
            request(&lame_duck, "GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
                .starts_with("HTTP/1.1 503")
        );
        assert!(request(
            &lame_duck,
            "GET /impacted_targets?from=a&to=b HTTP/1.1\r\nHost: x\r\n\r\n"
        )
        .starts_with("HTTP/1.1 503"));
        let metrics = request(&lame_duck, "GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(metrics.starts_with("HTTP/1.1 200"));
        assert!(metrics.contains("\"ready\":false"));

        // With the fetch skipped the same clone comes up ready.
        let skipped = unready_state(true);
        perform_initial_fetch(&skipped);
        assert!(skipped.ready.load(Ordering::Acquire));
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
        let missing = resolve_sha(&state, "missing").unwrap_err().to_string();
        assert_eq!(
            missing,
            "revision 'missing' is missing from the local clone"
        );
        assert!(checkout(&state, "missing").is_err());
        assert!(git(&state, &["not-a-command".into()]).is_err());

        let output = git_output(&state, &["rev-parse".into(), "HEAD".into()]).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), second);

        let mut bad_config = state.config.clone();
        bad_config.git_path = PathBuf::from("/definitely/missing/git");
        let bad = State::new(bad_config, None);
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
        let bad_state = State::new(bad_config, None);
        assert!(!fetch_revision(&bad_state, "HEAD"));
    }

    #[test]
    fn local_hash_cache_hits_and_touches_entries() {
        let (repo, _, sha) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), true);
        let dependency_fingerprint = state
            .config
            .hash_options
            .bazel
            .dependency_fingerprint()
            .unwrap();
        let data = HashFileData {
            hashes: std::collections::BTreeMap::from([(
                "//app:lib".into(),
                hash("Rule", "overall", "direct"),
            )]),
            dep_edges: std::collections::BTreeMap::from([(
                "//app:lib".into(),
                vec!["//dep:lib".into()],
            )]),
            dependency_fingerprint: Some(dependency_fingerprint),
            ..Default::default()
        };
        let key = cache_key_for_commit(&state, &sha);
        let path = cache.path().join(format!("{key}.json"));
        write_hashes(&path, &data, true);
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let retrieval = get_hashes_locked(&state, &sha, &BTreeSet::new(), None).unwrap();
        assert!(retrieval.cache_hit);
        assert!(retrieval.cache_read_millis.is_some());
        assert!(retrieval.generation.is_none());
        let loaded = retrieval.data;
        assert_eq!(loaded.hashes["//app:lib"].hash, "overall");
        assert_eq!(loaded.dep_edges["//app:lib"], ["//dep:lib"]);
        assert!(fs::metadata(path).unwrap().modified().unwrap() >= before);
    }

    #[test]
    fn memory_cache_evicts_least_recently_used_within_budget() {
        let entry = |label: &str| {
            Arc::new(HashFileData {
                hashes: std::collections::BTreeMap::from([(
                    label.to_owned(),
                    hash("Rule", "a", "b"),
                )]),
                ..Default::default()
            })
        };
        let mut cache = MemoryCache::new(100);
        cache.insert("a", 40, &entry("//a"));
        cache.insert("b", 40, &entry("//b"));
        assert_eq!(cache.len(), 2);
        // Reading `a` makes `b` the least recently used, so `c` evicts `b`.
        assert!(cache.get("a").unwrap().hashes.contains_key("//a"));
        cache.insert("c", 40, &entry("//c"));
        assert_eq!(cache.len(), 2);
        assert!(cache.get("b").is_none());
        assert!(cache.get("a").is_some());
        assert!(cache.get("c").is_some());
        assert_eq!(cache.used, 80);
        // Re-inserting a key replaces it instead of double counting.
        cache.insert("c", 50, &entry("//c2"));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.used, 90);
        assert!(cache.get("c").unwrap().hashes.contains_key("//c2"));
        // An entry over the whole budget is never kept; one that needs the whole budget evicts
        // everything else.
        cache.insert("huge", 101, &entry("//huge"));
        assert!(cache.get("huge").is_none());
        cache.insert("whole", 100, &entry("//whole"));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.used, 100);
        // A zero budget disables the tier.
        let mut disabled = MemoryCache::new(0);
        disabled.insert("a", 1, &entry("//a"));
        assert_eq!(disabled.len(), 0);
        assert!(disabled.get("a").is_none());
    }

    #[test]
    fn parsed_entries_are_served_from_memory_after_the_first_read() {
        let (repo, _, sha) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), false);
        let data = HashFileData {
            hashes: std::collections::BTreeMap::from([(
                "//app:lib".into(),
                hash("Rule", "overall", "direct"),
            )]),
            dependency_fingerprint: state
                .config
                .hash_options
                .bazel
                .dependency_fingerprint()
                .ok(),
            ..Default::default()
        };
        let key = cache_key_for_commit(&state, &sha);
        let path = cache.path().join(format!("{key}.json"));
        write_hashes(&path, &data, false);
        assert_eq!(metrics(&state)["cache"]["memory"]["entries"], 0);

        let first = get_hashes_locked(&state, &sha, &BTreeSet::new(), None).unwrap();
        assert!(first.cache_hit);
        let memory = metrics(&state)["cache"]["memory"].clone();
        assert_eq!(memory["entries"], 1);
        assert_eq!(memory["sizeBytes"], fs::metadata(&path).unwrap().len());
        assert_eq!(memory["maxBytes"], state.config.cache_memory_max);

        // The on-disk entry is gone (pruned, say); the parsed one still serves, and both
        // retrievals share the same allocation.
        fs::remove_file(&path).unwrap();
        let second = get_hashes_locked(&state, &sha, &BTreeSet::new(), None).unwrap();
        assert!(second.cache_hit);
        assert!(Arc::ptr_eq(&first.data, &second.data));
        assert_eq!(second.data.hashes["//app:lib"].hash, "overall");
    }

    #[test]
    fn remote_cache_hit_is_backfilled_and_next_read_is_local() {
        let (repo, _, sha) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.hash_options.bazel.workspace = repo.path().to_path_buf();
        config.git_path = PathBuf::from("git");
        config.cache_dir = cache.path().to_path_buf();
        config.dependency_fingerprint = true;
        let dependency_fingerprint = config.hash_options.bazel.dependency_fingerprint().unwrap();
        let data = HashFileData {
            hashes: std::collections::BTreeMap::from([(
                "//app:lib".into(),
                hash("Rule", "overall", "direct"),
            )]),
            dependency_fingerprint: Some(dependency_fingerprint),
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&data.serialized(true, false)).unwrap();
        let (remote, requests, handle) =
            remote_cache_with_responses("", vec![(200, gzip(&bytes).unwrap())]);
        let state = ready_state(config, Some(remote));

        let first = get_hashes_locked(&state, &sha, &BTreeSet::new(), None).unwrap();
        assert!(first.cache_hit);
        assert!(first.cache_read_millis.is_some());
        assert_eq!(first.data.hashes["//app:lib"].hash, "overall");
        let key = cache_key_for_commit(&state, &sha);
        assert!(cache.path().join(format!("{key}.json")).is_file());
        let second = get_hashes_locked(&state, &sha, &BTreeSet::new(), None).unwrap();
        assert!(second.cache_hit);
        assert_eq!(second.data.hashes["//app:lib"].hash, "overall");

        requests.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn compute_query_uses_cached_revisions_and_profiles() {
        let (repo, first, second) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), true);
        let dependency_fingerprint = state
            .config
            .hash_options
            .bazel
            .dependency_fingerprint()
            .unwrap();
        let from = HashFileData {
            hashes: std::collections::BTreeMap::from([
                ("//app:source".into(), hash("SourceFile", "old", "old")),
                ("//app:rule".into(), hash("Rule", "old-rule", "same-direct")),
            ]),
            dependency_fingerprint: Some(dependency_fingerprint.clone()),
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
            dependency_fingerprint: Some(dependency_fingerprint),
            ..Default::default()
        };
        for (sha, data) in [(&first, &from), (&second, &to)] {
            let key = cache_key_for_commit(&state, sha);
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
        let result = compute_query(
            &state,
            inputs,
            QueryKind::Distances,
            &Arc::new(Cancellation::new()),
        )
        .unwrap();
        assert_eq!(result["to"], second);
        assert_eq!(result["impactedTargets"].as_array().unwrap().len(), 2);
        let retrievals = result["profile"]["hashRetrievals"].as_array().unwrap();
        assert_eq!(retrievals.len(), 2);
        for retrieval in retrievals {
            assert_eq!(retrieval["cacheHit"], true);
            assert!(retrieval["cacheReadMillis"].as_u64().is_some());
            assert!(retrieval["durationMillis"].as_u64().is_some());
            assert!(retrieval.get("generation").is_none());
        }
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
    fn cache_entries_without_dependency_fingerprint_are_rejected() {
        let repo = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), false);
        let data = HashFileData {
            hashes: std::collections::BTreeMap::from([(
                "//app:lib".into(),
                hash("Rule", "overall", "direct"),
            )]),
            ..Default::default()
        };
        let mut current_dependency_fingerprint = None;

        assert!(!cache_entry_matches_dependency_fingerprint(
            &state,
            "abc",
            "local",
            &data,
            &mut current_dependency_fingerprint,
        )
        .unwrap());
    }

    #[test]
    fn dependency_fingerprint_guard_is_off_unless_opted_in() {
        let (repo, _, sha) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let mut state = state_for_repo(repo.path(), cache.path(), false);
        Arc::get_mut(&mut state)
            .unwrap()
            .config
            .dependency_fingerprint = false;
        // Neither a missing fingerprint nor a stale one may block a hit when the guard is off.
        for cached_fingerprint in [None, Some("stale".to_owned())] {
            let data = HashFileData {
                hashes: std::collections::BTreeMap::from([(
                    "//app:lib".into(),
                    hash("Rule", "overall", "direct"),
                )]),
                dependency_fingerprint: cached_fingerprint,
                ..Default::default()
            };
            let key = cache_key_for_commit(&state, &sha);
            write_hashes(&cache.path().join(format!("{key}.json")), &data, false);
            let retrieval = get_hashes_locked(&state, &sha, &BTreeSet::new(), None).unwrap();
            assert!(retrieval.cache_hit);
            assert_eq!(retrieval.data.hashes["//app:lib"].hash, "overall");
        }

        // The remote tier is trusted the same way: a fingerprint-less entry is a hit.
        let remote_cache = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.hash_options.bazel.workspace = repo.path().to_path_buf();
        config.git_path = PathBuf::from("git");
        config.cache_dir = remote_cache.path().to_path_buf();
        let data = HashFileData {
            hashes: std::collections::BTreeMap::from([(
                "//app:lib".into(),
                hash("Rule", "overall", "direct"),
            )]),
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&data.serialized(true, false)).unwrap();
        // Written by a replica from before compression: only the plain object exists.
        let (remote, requests, handle) = remote_cache_with_responses(
            "",
            vec![
                (404, b"<Error><Code>NoSuchKey</Code></Error>".to_vec()),
                (200, bytes),
            ],
        );
        let state = ready_state(config, Some(remote));
        let retrieval = get_hashes_locked(&state, &sha, &BTreeSet::new(), None).unwrap();
        assert!(retrieval.cache_hit);
        assert_eq!(retrieval.data.hashes["//app:lib"].hash, "overall");
        requests.recv_timeout(Duration::from_secs(5)).unwrap();
        requests.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.join().unwrap();

        // With the guard on, the same fingerprint-less entry is rejected, and since bazel is
        // `/bin/false` here the recompute fails rather than silently serving stale hashes.
        let mut guarded = state_for_repo(repo.path(), cache.path(), false);
        Arc::get_mut(&mut guarded)
            .unwrap()
            .config
            .dependency_fingerprint = true;
        let data = HashFileData {
            hashes: std::collections::BTreeMap::from([(
                "//app:lib".into(),
                hash("Rule", "overall", "direct"),
            )]),
            ..Default::default()
        };
        let key = cache_key_for_commit(&guarded, &sha);
        write_hashes(&cache.path().join(format!("{key}.json")), &data, false);
        assert!(get_hashes_locked(&guarded, &sha, &BTreeSet::new(), None).is_err());
    }

    #[test]
    fn miss_retrievals_serialize_the_generation_breakdown() {
        let retrieval = Retrieval {
            data: Arc::new(HashFileData::default()),
            cache_hit: false,
            cache_read_millis: None,
            generation: Some(GenerationBreakdown {
                checkout_millis: 1,
                bazel_query_millis: 2,
                target_hash_millis: 3,
                module_graph_millis: 4,
                cache_write_millis: 5,
                target_count: 6,
                partial: PartialProfile {
                    mode: Some("derived"),
                    derived_from: Some("base".into()),
                    changed_sources: Some(2),
                    ..Default::default()
                },
            }),
        };
        let value = retrieval_profile("abc", &retrieval, Instant::now());
        assert_eq!(value["sha"], "abc");
        assert_eq!(value["cacheHit"], false);
        assert!(value["durationMillis"].as_u64().is_some());
        assert!(value.get("cacheReadMillis").is_none());
        assert_eq!(
            value["generation"],
            json!({
                "checkoutMillis": 1,
                "bazelQueryMillis": 2,
                "targetHashMillis": 3,
                "moduleGraphMillis": 4,
                "cacheWriteMillis": 5,
                "targetCount": 6,
                "mode": "derived",
                "derivedFrom": "base",
                "changedSources": 2
            })
        );
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
        assert_eq!(value["dependencyFingerprint"], true);
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

        // The query endpoints take GET and POST only, and the method is checked before the
        // parameters are, so a well-formed query under the wrong method is still refused.
        for path in [
            "/impacted_targets",
            "/impacted_targets_with_distances",
            "/dependency_edges",
        ] {
            let response = request(
                &state,
                &format!(
                    "PUT {path}?from=a&to=b HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            );
            assert!(response.starts_with("HTTP/1.1 405"), "{path}: {response}");
            assert!(
                response.contains("method not allowed, use GET or POST"),
                "{path}: {response}"
            );
        }

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
        let dependency_fingerprint = state
            .config
            .hash_options
            .bazel
            .dependency_fingerprint()
            .unwrap();
        let from = HashFileData {
            hashes: std::collections::BTreeMap::from([
                ("//app:source".into(), hash("SourceFile", "old", "old")),
                ("//app:rule".into(), hash("Rule", "old-rule", "same-direct")),
            ]),
            dependency_fingerprint: Some(dependency_fingerprint.clone()),
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
            dependency_fingerprint: Some(dependency_fingerprint),
            ..Default::default()
        };
        for (sha, data) in [(&first, &from), (&second, &to)] {
            let key = cache_key_for_commit(&state, sha);
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
        // Another operation holds the workspace for the whole test, so the query never gets it.
        let _guard = state
            .workspace
            .acquire(&Arc::new(Cancellation::new()))
            .unwrap();
        let response = request(
            &state,
            "GET /impacted_targets?from=a&to=b HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 504"), "{response}");
        assert!(response.contains("request timed out"));
        // The abandoned query stopped waiting (so it is no longer counted) and the workspace is
        // reported busy because of the other holder, not because of it.
        assert!(response.contains("\"cancelled\":true"), "{response}");
        assert!(response.contains("\"workspaceBusy\":true"), "{response}");
        assert_eq!(state.active_queries.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn metrics_report_active_queries_and_workspace_busy() {
        let repo = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), false);
        let idle = metrics(&state);
        assert_eq!(idle["activeQueries"], 0);
        assert_eq!(idle["workspaceBusy"], false);
        assert!(state.workspace.holder().is_none());
        assert!(state.bazel_options().cancellation.is_none());
        state.workspace.check_cancelled().unwrap();

        let token = Arc::new(Cancellation::new());
        let guard = state.workspace.acquire(&token).unwrap();
        assert_eq!(metrics(&state)["workspaceBusy"], true);
        assert!(Arc::ptr_eq(&state.workspace.holder().unwrap(), &token));
        // Bazel options handed out while the query holds the workspace carry its token, so the
        // subprocesses they spawn are killed with it.
        let bazel = state.bazel_options();
        assert!(Arc::ptr_eq(bazel.cancellation.as_ref().unwrap(), &token));
        state.workspace.check_cancelled().unwrap();
        token.cancel();
        assert_eq!(
            state.workspace.check_cancelled().unwrap_err().to_string(),
            CANCELLED
        );
        assert_eq!(bazel.check_cancelled().unwrap_err().to_string(), CANCELLED);
        drop(guard);
        assert_eq!(metrics(&state)["workspaceBusy"], false);
        assert!(state.workspace.holder().is_none());

        let counted = ActiveQuery::start(&state);
        assert_eq!(metrics(&state)["activeQueries"], 1);
        drop(counted);
        assert_eq!(metrics(&state)["activeQueries"], 0);
    }

    #[test]
    fn queued_query_gives_up_once_cancelled() {
        let repo = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let state = state_for_repo(repo.path(), cache.path(), false);
        let _held = state
            .workspace
            .acquire(&Arc::new(Cancellation::new()))
            .unwrap();

        // Cancelled before it ever waits: fails on the first check.
        let cancelled = Arc::new(Cancellation::new());
        cancelled.cancel();
        assert_eq!(
            state
                .workspace
                .acquire(&cancelled)
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            CANCELLED
        );

        // Cancelled while waiting: notices within one poll interval instead of blocking until
        // the holder is done.
        let waiting = Arc::new(Cancellation::new());
        let waiter_state = Arc::clone(&state);
        let waiter_token = Arc::clone(&waiting);
        let waiter = thread::spawn(move || {
            waiter_state
                .workspace
                .acquire(&waiter_token)
                .map(|_| ())
                .unwrap_err()
                .to_string()
        });
        thread::sleep(WORKSPACE_WAIT_POLL);
        waiting.cancel();
        assert_eq!(waiter.join().unwrap(), CANCELLED);
        assert!(state.workspace.is_busy());
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_query_kills_its_subprocess_and_releases_the_workspace() {
        use std::os::unix::fs::PermissionsExt;

        let (repo, first, second) = initialize_git_repo();
        let cache = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        // A git that never returns on its own: the query's first subprocess hangs in it, so only
        // the kill can end the request in time.
        let slow_git = tools.path().join("slow-git");
        fs::write(&slow_git, "#!/bin/sh\nexec sleep 30\n").unwrap();
        fs::set_permissions(&slow_git, fs::Permissions::from_mode(0o755)).unwrap();
        let mut state = state_for_repo(repo.path(), cache.path(), false);
        {
            let config = &mut Arc::get_mut(&mut state).unwrap().config;
            config.request_timeout = Duration::from_millis(100);
            config.git_path = slow_git;
        }

        let started = Instant::now();
        let response = request(
            &state,
            &format!(
                "GET /impacted_targets?from={first}&to={second} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(response.starts_with("HTTP/1.1 504"), "{response}");
        assert!(
            response.contains("request timed out after 0s"),
            "{response}"
        );
        assert!(response.contains("\"cancelled\":true"), "{response}");
        assert!(response.contains("\"workspaceBusy\":false"), "{response}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the hanging git was not killed; request took {:?}",
            started.elapsed()
        );
        // The 504 was only sent once the query had released the workspace and stopped counting.
        assert!(!state.workspace.is_busy());
        assert_eq!(state.active_queries.load(Ordering::SeqCst), 0);
        // Nothing was cached for the generation the client walked away from.
        assert!(cache_entries(cache.path()).is_empty());
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
