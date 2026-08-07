use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

pub const RESOURCES: &str = "cli/src/test/resources";

pub struct TestWorkspace {
    temp_dir: TempDir,
    bazel_workspaces: Vec<PathBuf>,
}

impl TestWorkspace {
    fn new(temp_dir: TempDir) -> Self {
        let bazel_workspaces = top_level_bazel_workspaces(temp_dir.path());
        Self {
            temp_dir,
            bazel_workspaces,
        }
    }

    pub fn path(&self) -> &Path {
        self.temp_dir.path()
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let Some(bazel) = find_bazel() else {
            return;
        };
        for workspace in &self.bazel_workspaces {
            let _ = Command::new(&bazel)
                .arg("shutdown")
                .current_dir(workspace)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn top_level_bazel_workspaces(root: &Path) -> Vec<PathBuf> {
    let mut candidates = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            matches!(
                entry.file_name().to_str(),
                Some("MODULE.bazel" | "WORKSPACE" | "WORKSPACE.bazel")
            )
        })
        .filter_map(|entry| entry.path().parent().map(Path::to_path_buf))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| path.components().count());

    let mut workspaces = Vec::<PathBuf>::new();
    for candidate in candidates {
        if !workspaces
            .iter()
            .any(|workspace| candidate.starts_with(workspace))
        {
            workspaces.push(candidate);
        }
    }
    workspaces
}

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn bazel() -> PathBuf {
    find_bazel().expect("set BAZEL or install bazel/bazelisk")
}

fn find_bazel() -> Option<PathBuf> {
    std::env::var_os("BAZEL")
        .map(PathBuf::from)
        .or_else(|| which("bazel"))
        .or_else(|| which("bazelisk"))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join("go/bin/bazelisk"))
                .filter(|path| path.is_file())
        })
}

pub fn bazel_version() -> (u32, u32, u32) {
    let output = Command::new(bazel())
        .arg("version")
        .output()
        .expect("run bazel version");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text
        .lines()
        .find_map(|line| line.strip_prefix("Build label: "))
        .expect("Build label");
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
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

pub fn supports_mod_show_repo() -> bool {
    let version = bazel_version();
    version >= (8, 6, 0) && version != (9, 0, 0)
}

pub fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bazel-diff"))
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

pub fn run<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(binary());
    command.args(args);
    inherit_test_environment(&mut command);
    let output = command.output().expect("execute bazel-diff");
    if !output.status.success() {
        eprintln!("stdout:\n{}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    }
    output
}

fn inherit_test_environment(command: &mut Command) {
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "JAVA_HOME",
        "JAVA_TOOL_OPTIONS",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

pub fn generate(workspace: &Path, output: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "generate-hashes".to_owned(),
        "-w".to_owned(),
        workspace.display().to_string(),
        "-b".to_owned(),
        bazel().display().to_string(),
    ];
    args.extend(extra.iter().map(|value| (*value).to_owned()));
    args.push(output.display().to_string());
    run(args)
}

pub fn impacted(workspace: &Path, from: &Path, to: &Path, output: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "get-impacted-targets".to_owned(),
        "-w".to_owned(),
        workspace.display().to_string(),
        "-b".to_owned(),
        bazel().display().to_string(),
        "-sh".to_owned(),
        from.display().to_string(),
        "-fh".to_owned(),
        to.display().to_string(),
        "-o".to_owned(),
        output.display().to_string(),
    ];
    args.extend(extra.iter().map(|value| (*value).to_owned()));
    run(args)
}

pub fn fixture(name: &str) -> PathBuf {
    repo_root().join(RESOURCES).join("fixture").join(name)
}

pub fn workspace_fixture(name: &str) -> PathBuf {
    repo_root().join(RESOURCES).join("workspaces").join(name)
}

pub fn copy_workspace(name: &str) -> TestWorkspace {
    let destination = tempfile::tempdir().expect("temp workspace");
    copy_tree(&workspace_fixture(name), destination.path()).expect("copy workspace");
    TestWorkspace::new(destination)
}

pub fn extract_fixture(name: &str) -> TestWorkspace {
    let destination = tempfile::tempdir().expect("temp fixture");
    let file = fs::File::open(fixture(name)).expect("open zip fixture");
    let mut archive = zip::ZipArchive::new(file).expect("parse zip fixture");
    archive
        .extract(destination.path())
        .expect("extract zip fixture");
    TestWorkspace::new(destination)
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in walkdir::WalkDir::new(source) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source).expect("under source");
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

pub fn read_lines(path: &Path) -> BTreeSet<String> {
    fs::read_to_string(path)
        .expect("read lines")
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

pub fn golden_lines(name: &str) -> BTreeSet<String> {
    read_lines(&fixture(name))
}

pub fn filter_internal(targets: BTreeSet<String>) -> BTreeSet<String> {
    targets
        .into_iter()
        .filter(|target| {
            !target.contains("bazel-diff-integration-test")
                && !target.contains("@@//:BUILD")
                && !target.contains("bazel_diff_maven")
                && !target.contains("rules_jvm_external++maven+maven//:junit_junit")
                && !target.contains("rules_jvm_external++maven+maven//:org_hamcrest_hamcrest_core")
        })
        .map(|target| normalize_rules_jvm_external_label(&target))
        .collect()
}

fn normalize_rules_jvm_external_label(label: &str) -> String {
    let Some(rest) = label.strip_prefix("@@rules_jvm_external") else {
        return label.to_owned();
    };
    let Some((repository, target)) = rest.split_once("//") else {
        return label.to_owned();
    };
    let segments = if let Some(versioned) = repository.strip_prefix('~') {
        versioned
            .split('~')
            .skip(1)
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
    } else {
        repository
            .trim_start_matches('+')
            .split('+')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
    };
    format!("@@rules_jvm_external++{}//{target}", segments.join("+"))
}

pub fn hashes(path: &Path) -> BTreeMap<String, String> {
    let value: Value = serde_json::from_slice(&fs::read(path).expect("read hashes")).expect("json");
    let object = value
        .get("hashes")
        .unwrap_or(&value)
        .as_object()
        .expect("hash object");
    object
        .iter()
        .map(|(label, hash)| {
            (
                label.clone(),
                hash.as_str().expect("hash string").to_owned(),
            )
        })
        .collect()
}

pub fn edit(path: &Path, transform: impl FnOnce(String) -> String) {
    let original = fs::read_to_string(path).expect("read edit target");
    let changed = transform(original.clone());
    assert_ne!(changed, original, "{} was not changed", path.display());
    fs::write(path, changed).expect("write edit target");
}

pub fn git(workspace: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(workspace)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

pub fn init_git(workspace: &Path) -> String {
    git(workspace, &["init", "-q"]);
    git(workspace, &["config", "user.email", "test@example.com"]);
    git(workspace, &["config", "user.name", "test"]);
    git(workspace, &["add", "-A"]);
    git(workspace, &["commit", "-q", "-m", "base"]);
    git(workspace, &["rev-parse", "HEAD"])
}

pub fn commit(workspace: &Path, message: &str) -> String {
    git(workspace, &["add", "-A"]);
    git(workspace, &["commit", "-q", "-m", message]);
    git(workspace, &["rev-parse", "HEAD"])
}

pub fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind port")
        .local_addr()
        .expect("local addr")
        .port()
}

pub struct ServerProcess {
    child: Child,
    pub port: u16,
}

impl ServerProcess {
    pub fn start(workspace: &Path, cache: &Path, extra: &[&str]) -> Self {
        let port = free_port();
        let mut args = vec![
            "serve".to_owned(),
            "-w".to_owned(),
            workspace.display().to_string(),
            "-b".to_owned(),
            bazel().display().to_string(),
            "--cacheDir".to_owned(),
            cache.display().to_string(),
            "--port".to_owned(),
            port.to_string(),
            "--no-initial-fetch".to_owned(),
        ];
        args.extend(extra.iter().map(|value| (*value).to_owned()));
        let mut command = Command::new(binary());
        command.args(args);
        inherit_test_environment(&mut command);
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start serve");
        let server = Self { child, port };
        server.await_healthy();
        server
    }

    fn await_healthy(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if self.get("/health").is_ok_and(|(status, _)| status == 200) {
                return;
            }
            thread::sleep(Duration::from_millis(250));
        }
        panic!("serve did not become healthy");
    }

    pub fn get(&self, path: &str) -> io::Result<(u16, String)> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        stream.set_read_timeout(Some(Duration::from_secs(300)))?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        let status = response
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse().ok())
            .unwrap_or(0);
        let body = response
            .split_once("\r\n\r\n")
            .map_or(String::new(), |(_, body)| body.to_owned());
        Ok((status, body))
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn normalizes_rules_jvm_external_canonical_names_across_bazel_versions() {
    assert_eq!(
        normalize_rules_jvm_external_label(
            "@@rules_jvm_external~6.3~maven~com_google_guava_guava_32_0_0_jre//file:file"
        ),
        "@@rules_jvm_external++maven+com_google_guava_guava_32_0_0_jre//file:file"
    );
    assert_eq!(
        normalize_rules_jvm_external_label(
            "@@rules_jvm_external++maven+com_google_guava_guava_32_0_0_jre//file:file"
        ),
        "@@rules_jvm_external++maven+com_google_guava_guava_32_0_0_jre//file:file"
    );
}
