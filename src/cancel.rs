//! Cooperative cancellation for work that shells out to `git` and `bazel`.
//!
//! A [`Cancellation`] token is shared between the thread doing the work and the thread that may
//! abandon it. Every subprocess the work spawns goes through [`output`] or [`output_configured`],
//! which register the child so [`Cancellation::cancel`] can kill it, and which refuse to start a
//! child once the token is cancelled. Phases with no subprocess to kill (CPU-bound hashing, the
//! cache write) call [`Cancellation::check`] at their boundaries instead.
//!
//! Without a token (`None`), both helpers behave exactly like [`Command::output`], so the CLI
//! commands that never cancel anything are unaffected.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The message carried by every error a cancelled token produces.
pub const CANCELLED: &str = "query cancelled";

/// Longest pause between exit polls of a running child. Polling starts at a millisecond and backs
/// off, so a quick `git rev-parse` is noticed almost at once and a long `bazel query` costs at most
/// this much extra latency.
const MAX_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Default)]
pub struct Cancellation {
    cancelled: AtomicBool,
    next_id: AtomicU64,
    children: Mutex<HashMap<u64, Arc<Mutex<Child>>>>,
}

impl fmt::Debug for Cancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Cancellation")
            .field("cancelled", &self.is_cancelled())
            .field("children", &self.running_children())
            .finish()
    }
}

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Fails once the token is cancelled, so a phase boundary can stop work that has no
    /// subprocess to kill.
    pub fn check(&self) -> io::Result<()> {
        if self.is_cancelled() {
            return Err(cancelled_error());
        }
        Ok(())
    }

    /// Number of subprocesses started through this token that have not exited yet.
    pub fn running_children(&self) -> usize {
        lock(&self.children).len()
    }

    /// Marks the token cancelled and kills every registered subprocess. Returns how many kills
    /// were delivered. Later calls to [`output`] with this token fail without spawning anything.
    pub fn cancel(&self) -> usize {
        self.cancelled.store(true, Ordering::SeqCst);
        let children = lock(&self.children).values().cloned().collect::<Vec<_>>();
        children
            .iter()
            .filter(|child| lock(child).kill().is_ok())
            .count()
    }

    /// Runs `command` like [`Command::output`] (stdin closed, stdout and stderr captured), but
    /// kills it on [`cancel`](Self::cancel) and fails with [`CANCELLED`] instead of returning
    /// output the caller no longer wants.
    pub fn output(&self, command: &mut Command) -> io::Result<Output> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.output_configured(command)
    }

    /// Like [`output`](Self::output), but keeps the stdio the caller configured, for commands
    /// whose stdout is redirected to a file. Only piped streams are captured.
    pub fn output_configured(&self, command: &mut Command) -> io::Result<Output> {
        self.check()?;
        let mut child = command.spawn()?;
        // Both pipes are drained on their own threads so the child never blocks on a full pipe
        // while this thread waits for it to exit.
        let stdout = child.stdout.take().map(spawn_reader);
        let stderr = child.stderr.take().map(spawn_reader);
        let registered = self.register(child);
        // A cancel that landed between `check` and `register` saw an empty registry.
        if self.is_cancelled() {
            let _ = lock(&registered.child).kill();
        }
        let status = wait_for_exit(&registered.child)?;
        drop(registered);
        // After a cancel the readers are deliberately not joined: a grandchild the kill did not
        // reach may still hold the pipes open, and nobody wants the output anyway.
        self.check()?;
        Ok(Output {
            status,
            stdout: join_reader(stdout)?,
            stderr: join_reader(stderr)?,
        })
    }

    fn register(&self, child: Child) -> Registered<'_> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let child = Arc::new(Mutex::new(child));
        lock(&self.children).insert(id, Arc::clone(&child));
        Registered {
            token: self,
            id,
            child,
        }
    }
}

/// Runs `command` through `token` when there is one, or exactly like [`Command::output`] when
/// there is not.
pub fn output(token: Option<&Cancellation>, command: &mut Command) -> io::Result<Output> {
    match token {
        Some(token) => token.output(command),
        None => command.output(),
    }
}

/// [`output`] for a command whose stdio the caller configured; see
/// [`Cancellation::output_configured`].
pub fn output_configured(
    token: Option<&Cancellation>,
    command: &mut Command,
) -> io::Result<Output> {
    match token {
        Some(token) => token.output_configured(command),
        None => command.output(),
    }
}

/// A child that is registered with its token until it has been waited on, including when the
/// caller unwinds past the wait.
struct Registered<'a> {
    token: &'a Cancellation,
    id: u64,
    child: Arc<Mutex<Child>>,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        lock(&self.token.children).remove(&self.id);
    }
}

fn spawn_reader(mut pipe: impl Read + Send + 'static) -> JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn join_reader(reader: Option<JoinHandle<io::Result<Vec<u8>>>>) -> io::Result<Vec<u8>> {
    match reader {
        Some(reader) => reader
            .join()
            .map_err(|_| io::Error::other("output reader panicked"))?,
        None => Ok(Vec::new()),
    }
}

/// Waits for the child without holding its lock across the wait, so `cancel` can take the lock
/// to kill it.
fn wait_for_exit(child: &Mutex<Child>) -> io::Result<ExitStatus> {
    let mut interval = Duration::from_millis(1);
    loop {
        if let Some(status) = lock(child).try_wait()? {
            return Ok(status);
        }
        thread::sleep(interval);
        interval = (interval * 2).min(MAX_POLL_INTERVAL);
    }
}

fn cancelled_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, CANCELLED)
}

/// A poisoned lock here only means another thread panicked mid-bookkeeping; the registry and the
/// child handles are still usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn fresh_token_is_not_cancelled() {
        let token = Cancellation::new();
        assert!(!token.is_cancelled());
        token.check().unwrap();
        assert_eq!(token.running_children(), 0);
        assert_eq!(token.cancel(), 0);
        assert!(token.is_cancelled());
        let error = token.check().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(error.to_string(), CANCELLED);
        assert!(format!("{token:?}").contains("cancelled: true"));
    }

    #[cfg(unix)]
    #[test]
    fn output_captures_streams_and_status_like_command_output() {
        let token = Cancellation::new();
        let captured = token
            .output(&mut shell("echo out; echo err >&2; exit 3"))
            .unwrap();
        assert_eq!(captured.status.code(), Some(3));
        assert_eq!(captured.stdout, b"out\n");
        assert_eq!(captured.stderr, b"err\n");
        assert_eq!(token.running_children(), 0);

        // Without a token the helpers defer to Command::output.
        let plain = output_configured(None, &mut shell("echo plain")).unwrap();
        assert_eq!(plain.stdout, b"plain\n");
        let plain = output(None, &mut shell("exit 2")).unwrap();
        assert_eq!(plain.status.code(), Some(2));
    }

    #[cfg(unix)]
    #[test]
    fn output_configured_keeps_redirected_stdout() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stdout.txt");
        let token = Cancellation::new();
        let mut command = shell("echo to-file; echo err >&2");
        command
            .stdout(std::fs::File::create(&path).unwrap())
            .stderr(Stdio::piped());
        let result = output_configured(Some(&token), &mut command).unwrap();
        assert!(result.status.success());
        assert!(result.stdout.is_empty());
        assert_eq!(result.stderr, b"err\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "to-file\n");
    }

    #[cfg(unix)]
    #[test]
    fn cancel_kills_running_child_and_refuses_new_ones() {
        let token = Arc::new(Cancellation::new());
        let worker_token = Arc::clone(&token);
        let worker = thread::spawn(move || worker_token.output(&mut shell("exec sleep 30")));
        let started = Instant::now();
        while token.running_children() == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "child never started"
            );
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(token.cancel(), 1);
        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the sleeping child was not killed"
        );
        assert_eq!(token.running_children(), 0);

        let refused = output(Some(&token), &mut shell("echo never")).unwrap_err();
        assert_eq!(refused.to_string(), CANCELLED);
        // Cancelling again is harmless and kills nothing.
        assert_eq!(token.cancel(), 0);
    }

    #[test]
    fn spawn_failure_surfaces_and_leaves_no_registration() {
        let token = Cancellation::new();
        let error = token
            .output(&mut Command::new("/definitely/missing/binary"))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(token.running_children(), 0);
    }
}
