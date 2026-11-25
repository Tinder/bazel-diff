use std::path::Path;
use std::process::Command;

/// Writes an executable test script from a separate `/bin/sh`, so this process never holds it
/// open for writing.
///
/// Linux refuses to execute a file that any process has open for writing (`ETXTBSY`, "Text file
/// busy"). A test that writes a script itself and then runs it races with other tests: a child
/// they fork while the script is open inherits the write descriptor until that child calls
/// `exec`. Here the only writer is the shell, which has exited before this returns.
pub(crate) fn write_executable(path: &Path, contents: &str) {
    let status = Command::new("/bin/sh")
        .args(["-c", r#"printf '%s' "$2" > "$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .arg(contents)
        .status()
        .expect("spawn /bin/sh to write a test script");
    assert!(status.success(), "writing {} failed", path.display());
}
