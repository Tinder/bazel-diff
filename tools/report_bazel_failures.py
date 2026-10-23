"""Print the logs of failed Bazel actions and tests from a JSON build event file.

`bazel build` and `bazel test` on GitHub Actions scroll the useful diagnostic
past pages of progress output, and the test log a failed test points at is
never printed. Bazel's build event protocol (BEP) records every failed action
with the path of its stderr, and every test attempt with the path of its
`test.log`. Reading those from `--build_event_json_file` takes no guessing
about the shape of Bazel's console output.

CI runs Bazel with `--config=ci`, which .bazelrc maps to
`--build_event_json_file=bazel-bep.json`, and each job ends with an
`if: failure()` step that runs this script on that file. The script renders:

* a text block for the end of the Actions log
* `::error::` annotations, which Actions shows at the top of the failed step
* a markdown section appended to `$GITHUB_STEP_SUMMARY` when that is set

Usage:
    report_bazel_failures.py BUILD_EVENT_JSON_FILE

The process exits 0 whenever it ran at all, including when the file is
missing (the job failed before Bazel ran). A failing report step would only
hide the step that failed.
"""

import json
import os
import sys
import urllib.parse
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass, field


_MAX_LINES = 200
_MAX_CHARS = 20_000
_MAX_LINE = 1_000
_MAX_FAILURES = 10
_MAX_NOTES = 20
_MAX_ANNOTATION = 4_000
_LOG_READ_LIMIT = 4_000_000

# Test attempt and summary statuses that are not failures.
_TEST_OK = {"PASSED", "FLAKY"}

LogReader = Callable[[str], "str | None"]


@dataclass
class Failure:
    """One failed action or test attempt and the logs Bazel kept for it."""

    label: str
    detail: str
    # (name, path-or-uri) pairs, e.g. ("stderr", "/tmp/.../stderr-12") or
    # ("test.log", "/tmp/.../test.log"). Unreadable entries still get listed.
    logs: list[tuple[str, str]] = field(default_factory=list)
    message: str = ""
    # The excerpt is filled in from the log files by `parse_bep`.
    excerpt: str = ""


@dataclass
class Report:
    failures: list[Failure]
    notes: list[str]


def read_text_file(path: str) -> "str | None":
    """Read a log file Bazel named. Missing and unreadable files yield None."""
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            return handle.read(_LOG_READ_LIMIT)
    except OSError:
        return None


def parse_bep(text: str, read_log: "LogReader | None" = None) -> Report:
    """Collect failed actions and tests from newline-delimited BEP JSON.

    `read_log` loads a log path named in an event and returns its contents, or
    None when the file cannot be read. The default reader opens the path.
    """
    reader = read_text_file if read_log is None else read_log
    actions: list[Failure] = []
    tests: dict[str, list[Failure]] = {}
    test_order: list[str] = []
    overall: dict[str, str] = {}
    aborted: list[str] = []
    finished: list[tuple[str, str]] = []
    console_errors: list[str] = []
    malformed = 0

    for raw in text.splitlines():
        if not raw.strip():
            continue
        try:
            event = json.loads(raw)
        except ValueError:
            malformed += 1
            continue
        if not isinstance(event, dict):
            malformed += 1
            continue
        ident = event.get("id") or {}

        action = event.get("action")
        if isinstance(action, dict):
            failure = _action_failure(action, ident)
            if failure is not None:
                actions.append(failure)
            continue

        result = event.get("testResult")
        if isinstance(result, dict):
            label = str((ident.get("testResult") or {}).get("label") or "")
            failure = _test_failure(result, ident, label)
            if failure is not None:
                if label not in tests:
                    tests[label] = []
                    test_order.append(label)
                tests[label].append(failure)
            continue

        summary = event.get("testSummary")
        if isinstance(summary, dict):
            label = str((ident.get("testSummary") or {}).get("label") or "")
            overall[label] = str(summary.get("overallStatus") or "")
            continue

        abort = event.get("aborted")
        if isinstance(abort, dict):
            note = _aborted_note(abort, ident)
            if note:
                aborted.append(note)
            continue

        done = event.get("finished")
        if isinstance(done, dict):
            finished.append(_finished_status(done))
            continue

        progress = event.get("progress")
        if isinstance(progress, dict):
            for line in str(progress.get("stderr") or "").splitlines():
                if line.startswith("ERROR: "):
                    console_errors.append(_shorten_line(line.rstrip()))

    failures = list(actions)
    for label in test_order:
        if overall.get(label, "") in _TEST_OK:
            # Passed on a later attempt. The build did not fail because of it.
            continue
        failures.extend(tests[label])

    for failure in failures:
        failure.excerpt = _excerpt(failure, reader)

    notes: list[str] = []
    if not failures:
        # Analysis and loading errors never produce an action or test event.
        # Bazel's own console errors and the aborted targets are all there is.
        notes.extend(console_errors)
        notes.extend(aborted)
    for name, message in finished:
        # With failures listed, the message only repeats the first of them.
        note = _join("build finished", name)
        notes.append(note if failures else _join(note, message))
        if name == "SUCCESS" and not failures:
            notes.append("The last Bazel invocation succeeded; the job failed in another step.")
    if malformed:
        notes.append(f"{malformed} unreadable line(s) in the build event file were skipped.")
    return Report(failures, _dedupe(notes)[:_MAX_NOTES])


def render_text(report: Report) -> str:
    """Plain-text report printed at the end of the Actions log."""
    lines = ["----- bazel failure summary -----"]
    lines.extend(report.notes)
    if report.notes:
        lines.append("")
    shown = _limited(report.failures)
    if not shown:
        lines.append("The build event file names no failed action or test.")
    for failure in shown:
        lines.append(_heading(failure))
        for name, path in failure.logs:
            lines.append(f"  {name}: {path}")
        if failure.excerpt:
            lines.append(_neutralize(failure.excerpt))
        lines.append("")
    lines.append("----- end bazel failure summary -----")
    return "\n".join(lines) + "\n"


def render_markdown(report: Report) -> str:
    """Markdown section for `$GITHUB_STEP_SUMMARY`."""
    out = ["", "## Bazel failures", ""]
    for note in report.notes:
        out.append(f"- {note}")
    if report.notes:
        out.append("")
    shown = _limited(report.failures)
    if not shown:
        out.append("The build event file names no failed action or test.")
        out.append("")
    for failure in shown:
        title = _heading(failure).replace("`", "'")
        out.append(f"### `{title}`")
        out.append("")
        for name, path in failure.logs:
            out.append(f"- {name}: `{path}`")
        if failure.logs:
            out.append("")
        if failure.excerpt:
            out.append(_fence(failure.excerpt))
            out.append("")
    return "\n".join(out)


def render_annotations(report: Report) -> str:
    """GitHub Actions `::error::` commands, one per failure."""
    commands = []
    for failure in _limited(report.failures):
        title = _escape(_heading(failure)[:180], property_value=True)
        message = failure.excerpt or failure.message or _heading(failure)
        if len(message) > _MAX_ANNOTATION:
            message = "…\n" + message[-_MAX_ANNOTATION:]
        commands.append(f"::error title={title}::{_escape(message, property_value=False)}")
    if not commands:
        return ""
    return "\n".join(commands) + "\n"


def main(argv: "list[str] | None" = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if len(args) != 1:
        print("usage: report_bazel_failures.py BUILD_EVENT_JSON_FILE", file=sys.stderr)
        return 2
    path = args[0]
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            text = handle.read()
    except OSError as exc:
        print(f"no build event file to report from ({exc}); Bazel may not have run", file=sys.stderr)
        return 0

    report = parse_bep(text)
    sys.stdout.write(render_text(report))
    annotations = render_annotations(report)
    if annotations:
        sys.stdout.write(annotations)
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_path:
        try:
            with open(summary_path, "a", encoding="utf-8") as handle:
                handle.write(render_markdown(report))
        except OSError as exc:
            print(f"warning: could not write job summary: {exc}", file=sys.stderr)
    return 0


def _action_failure(action: dict, ident: dict) -> "Failure | None":
    if action.get("success"):
        return None
    label = str(action.get("label") or (ident.get("actionCompleted") or {}).get("label") or "")
    kind = str(action.get("type") or "action")
    code = action.get("exitCode")
    detail = f"{kind} failed" if code is None else f"{kind} failed (exit {code})"
    logs = []
    for name in ("stderr", "stdout"):
        path = _file_path(action.get(name))
        if path:
            logs.append((name, path))
    return Failure(
        label=label,
        detail=detail,
        logs=logs,
        message=str((action.get("failureDetail") or {}).get("message") or ""),
    )


def _test_failure(result: dict, ident: dict, label: str) -> "Failure | None":
    status = str(result.get("status") or "")
    if status in _TEST_OK:
        return None
    attempt = ident.get("testResult") or {}
    parts = []
    for key, name in (("shard", "shard"), ("run", "run"), ("attempt", "attempt")):
        value = attempt.get(key)
        if value not in (None, 0, 1, "0", "1"):
            parts.append(f"{name} {value}")
    detail = status or "FAILED"
    if parts:
        detail = f"{detail} ({', '.join(parts)})"
    logs = []
    for output in result.get("testActionOutput") or []:
        if not isinstance(output, dict):
            continue
        name = str(output.get("name") or "")
        if name.endswith("test.log"):
            path = _file_path(output)
            if path:
                logs.append((name, path))
    message = ""
    if result.get("failureDetail"):
        message = str(result["failureDetail"].get("message") or "")
    return Failure(label=label, detail=detail, logs=logs, message=message)


def _aborted_note(abort: dict, ident: dict) -> str:
    reason = str(abort.get("reason") or "ABORTED")
    description = " ".join(str(abort.get("description") or "").split())
    label = ""
    for key in ("targetCompleted", "targetConfigured", "configuredLabel", "unconfiguredLabel", "pattern"):
        value = ident.get(key)
        if isinstance(value, dict) and value.get("label"):
            label = str(value["label"])
            break
        if isinstance(value, dict) and value.get("pattern"):
            label = " ".join(str(item) for item in value["pattern"])
            break
    if not description and not label:
        return ""
    head = f"{label} aborted: {reason}" if label else f"aborted: {reason}"
    return _join(head, description)


def _finished_status(done: dict) -> tuple[str, str]:
    name = str((done.get("exitCode") or {}).get("name") or "")
    message = " ".join(str((done.get("failureDetail") or {}).get("message") or "").split())
    return name, message


def _join(head: str, tail: str) -> str:
    return f"{head}: {tail}" if tail else head


def _file_path(entry: object) -> "str | None":
    """Local path for a BEP `File`. Non-file URIs are returned as written."""
    if not isinstance(entry, dict):
        return None
    uri = str(entry.get("uri") or "")
    if not uri:
        return None
    parsed = urllib.parse.urlparse(uri)
    if parsed.scheme != "file":
        return uri
    path = urllib.request.url2pathname(parsed.path)
    if parsed.netloc and parsed.netloc != "localhost":
        # UNC-style file://host/share paths. Not expected from Bazel.
        return f"//{parsed.netloc}{path}"
    return path


def _excerpt(failure: Failure, read_log: LogReader) -> str:
    chunks = []
    for name, path in failure.logs:
        if "://" in path:
            continue
        contents = read_log(path)
        if contents is None:
            continue
        body = _strip_test_header(contents) if name.endswith("test.log") else contents.strip()
        if not body:
            continue
        if len(failure.logs) > 1:
            chunks.append(f"[{name}]\n{body}")
        else:
            chunks.append(body)
    text = "\n\n".join(chunks)
    if not text and failure.message:
        text = failure.message.strip()
    return _tail(text)


def _strip_test_header(text: str) -> str:
    """Drop Bazel's three-line `exec ${PAGER...}` preamble from a test.log."""
    lines = text.splitlines()
    if lines and lines[0].startswith("exec ${PAGER"):
        lines = lines[1:]
        if lines and lines[0].startswith("Executing tests from "):
            lines = lines[1:]
        if lines and lines[0].startswith("-----"):
            lines = lines[1:]
    return "\n".join(lines).strip()


def _tail(text: str) -> str:
    """Keep the end of a long log. That is where Bazel-run tools summarize."""
    lines = [_shorten_line(line) for line in text.splitlines()]
    dropped = 0
    if len(lines) > _MAX_LINES:
        dropped = len(lines) - _MAX_LINES
        lines = lines[-_MAX_LINES:]
    clipped = "\n".join(lines).strip()
    if len(clipped) > _MAX_CHARS:
        clipped = clipped[-_MAX_CHARS:]
        clipped = clipped.split("\n", 1)[-1] if "\n" in clipped else clipped
        dropped = max(dropped, 1)
    if dropped:
        clipped = f"… ({dropped} earlier line(s) omitted)\n{clipped}" if dropped > 1 else f"… (truncated)\n{clipped}"
    return clipped


def _limited(failures: list[Failure]) -> list[Failure]:
    if len(failures) <= _MAX_FAILURES:
        return failures
    hidden = failures[_MAX_FAILURES:]
    names = ", ".join(failure.label or failure.detail for failure in hidden)
    more = Failure(label="", detail=f"{len(hidden)} more failures", excerpt=names)
    return failures[:_MAX_FAILURES] + [more]


def _dedupe(items: list[str]) -> list[str]:
    seen: set[str] = set()
    out = []
    for item in items:
        if item not in seen:
            seen.add(item)
            out.append(item)
    return out


def _heading(failure: Failure) -> str:
    if failure.label and failure.detail:
        return f"{failure.label}: {failure.detail}"
    return failure.label or failure.detail or "command failed"


def _shorten_line(line: str) -> str:
    if len(line) > _MAX_LINE:
        return line[:_MAX_LINE] + "…"
    return line


def _neutralize(text: str) -> str:
    """Keep test output from being parsed as an Actions workflow command."""
    return "\n".join((" " + line) if line.startswith("::") else line for line in text.splitlines())


def _fence(text: str) -> str:
    tick = "```"
    while tick in text:
        tick += "`"
    return f"{tick}text\n{text}\n{tick}"


def _escape(text: str, *, property_value: bool) -> str:
    text = text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    if property_value:
        text = text.replace(",", "%2C").replace(":", "%3A")
    return text


if __name__ == "__main__":
    sys.exit(main())
