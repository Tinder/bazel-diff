"""Pull build and test failures out of a Bazel log.

`bazel build` and `bazel test` on GitHub Actions scroll the useful diagnostic
past pages of progress lines and compiler warnings. The failing target is one
line near the bottom, and the test log it points at is never printed.

This module finds those targets, reads the test logs Bazel named, and renders
three views of the same report:

* a short text block for the end of the Actions log
* `::error::` annotations, which Actions shows at the top of the failed step
* a markdown section appended to the job summary

Usage:
    report_bazel_failures.py BAZEL_LOG

The process always exits 0 when the log can be read. Callers keep Bazel's own
exit code. A log with no recognizable failure still produces a short tail
excerpt, because this program is only invoked after the Bazel command failed.
"""

import os
import re
import sys
from collections.abc import Callable
from dataclasses import dataclass


_ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
# `@repo//pkg:name`, `@@repo//pkg:name`, `@@//pkg:name`, and `//pkg:name`.
_LABEL = r"(?P<label>(?:@@?(?:[A-Za-z0-9_.+-]*))?//[^\s]+)"
_FAIL_TARGET = re.compile(rf"^(?P<status>FAIL|TIMEOUT):\s+{_LABEL}(?P<rest>.*)$")
_TARGET_LINE = re.compile(
    rf"^{_LABEL}(?:\s+\([^)]*\))*\s+"
    r"(?P<status>FAILED|TIMEOUT|FLAKY|REMOTE_FAILURE|NO STATUS)\b(?P<rest>.*)$"
)
_LOG_LINE = re.compile(r"^\s+(\S*test\.log)\s*$")
_SEE = re.compile(r"\(see\s+(\S+)\)")
_SUMMARY = re.compile(
    r"^(?:Executed \d+ out of \d+|"
    r"INFO: Build completed,|"
    r"ERROR: Build did NOT complete successfully\b|"
    r"FAILED: Build did NOT complete successfully\b)"
)
_LABEL_ANYWHERE = re.compile(
    r"(?:@@?(?:[A-Za-z0-9_.+-]*))?//[A-Za-z0-9_./+-]*:[A-Za-z0-9_./+-]+"
)
_DIAGNOSTIC = re.compile(r"^error(?:\[E\d+\])?: ")
_LOC_PANIC = re.compile(r"panicked at ([^:\s]+):(\d+)")
_LOC_ARROW = re.compile(r"^\s*-->\s+([^:\n]+):(\d+)", re.MULTILINE)
_PROGRESS = re.compile(r"^\[\d")

_MAX_LINES = 80
_MAX_CHARS = 6000
_MAX_LINE = 500
_MAX_FAILURES = 10
_MAX_ANNOTATION = 500
_LOG_READ_LIMIT = 1_000_000

LogReader = Callable[[str], "str | None"]


@dataclass
class Failure:
    """One build error, failed test, or other command failure."""

    label: str
    detail: str
    excerpt: str = ""
    file: str | None = None
    line: int | None = None
    # True for Bazel's "Running //pkg:tool failed" wrapper around a program
    # that already printed its own FAIL line. Dropped when that line was kept.
    wrapper: bool = False


@dataclass
class Report:
    """Failures found in one Bazel invocation, plus Bazel's own summary lines."""

    failures: list[Failure]
    notes: list[str]


@dataclass
class _TestHit:
    label: str
    detail: str = ""
    log_path: str | None = None


def read_text_file(path: str) -> str | None:
    """Read a test log if it exists. Missing files and unreadable paths are skipped."""
    if not path.endswith("test.log") or not os.path.isfile(path):
        return None
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            return handle.read(_LOG_READ_LIMIT)
    except OSError:
        return None


def parse_failures(text: str, read_log: LogReader | None = None) -> Report:
    """Extract failed tests and build errors from Bazel stdout/stderr.

    `read_log` loads a test-log path named in the Bazel output. It returns the
    file contents, or None when the path is missing. The default reader opens
    the path on disk and also understands `bazel-testlogs/<pkg>/<target>/test.log`
    when the absolute path Bazel printed is already gone.
    """
    reader = read_text_file if read_log is None else read_log
    lines = _normalize(text)
    hits: dict[str, _TestHit] = {}
    order: list[str] = []
    extras: list[Failure] = []
    notes: list[str] = []

    def hit_for(label: str) -> _TestHit:
        hit = hits.get(label)
        if hit is None:
            hit = _TestHit(label=label)
            hits[label] = hit
            order.append(label)
        return hit

    index = 0
    while index < len(lines):
        line = lines[index]
        if _SUMMARY.match(line):
            notes.append(line)
            index += 1
            continue

        fail = _FAIL_TARGET.match(line)
        if fail:
            item = hit_for(fail.group("label"))
            seen = _SEE.search(line)
            if seen and not item.log_path:
                item.log_path = seen.group(1)
            if not item.detail:
                rest = _SEE.sub("", fail.group("rest") or "").strip()
                status = fail.group("status")
                item.detail = f"{status} {rest}".strip() if rest else status
            index += 1
            continue

        target = _TARGET_LINE.match(line)
        if target:
            item = hit_for(target.group("label"))
            # The summary line (`FAILED in 0.4s`) is more useful than the
            # earlier `FAIL: (Exit 101)` event, so it always wins.
            item.detail = f"{target.group('status')}{target.group('rest') or ''}".strip()
            if index + 1 < len(lines):
                log_match = _LOG_LINE.match(lines[index + 1])
                if log_match:
                    item.log_path = log_match.group(1)
                    index += 1
            index += 1
            continue

        if line.startswith("FAIL: "):
            extras.append(
                Failure(
                    label="",
                    detail=line.strip(),
                    excerpt=_context_before(lines, index, 40),
                )
            )
            index += 1
            continue

        if line.startswith("ERROR: "):
            start = index
            block, index = _error_block(lines, start)
            extras.append(_failure_from_error(lines, start, block))
            continue

        index += 1

    failures: list[Failure] = []
    seen_labels = set(order)
    for label in order:
        item = hits[label]
        excerpt = _excerpt_for_test(item, reader)
        file_path, file_line = _first_location(excerpt)
        failures.append(
            Failure(
                label=label,
                detail=item.detail or "FAILED",
                excerpt=excerpt,
                file=file_path,
                line=file_line,
            )
        )
    for extra in extras:
        if extra.label and extra.label in seen_labels:
            continue
        if extra.file is None:
            extra.file, extra.line = _first_location(extra.excerpt)
        failures.append(extra)
    return Report(_drop_wrappers(failures), _dedupe(notes))


def render_text(report: Report) -> str:
    """Plain-text report printed at the end of the Actions log."""
    lines = ["----- bazel failure summary -----"]
    lines.extend(report.notes)
    shown = _limited(report.failures)
    if not shown:
        lines.append("The command failed, but no failure was found in the log.")
    for failure in shown:
        lines.append(_heading(failure))
        excerpt = _neutralize(failure.excerpt.strip())
        if excerpt and excerpt != _heading(failure) and excerpt != failure.detail:
            lines.append(excerpt)
        lines.append("")
    lines.append("----- end bazel failure summary -----")
    return "\n".join(lines) + "\n"


def render_markdown(report: Report) -> str:
    """Markdown section for `$GITHUB_STEP_SUMMARY`."""
    out = ["", "## Bazel failures", ""]
    for note in report.notes:
        out.append(note)
        out.append("")
    shown = _limited(report.failures)
    if not shown:
        out.append("The command failed, but no failure was found in the log.")
        out.append("")
    for failure in shown:
        title = _heading(failure).replace("`", "'")
        out.append(f"### `{title}`")
        out.append("")
        excerpt = failure.excerpt.strip()
        if excerpt and excerpt != _heading(failure) and excerpt != failure.detail:
            out.append(_fence(excerpt))
            out.append("")
    return "\n".join(out)


def render_annotations(report: Report) -> str:
    """GitHub Actions `::error::` commands, one per failure."""
    commands = []
    for failure in _limited(report.failures):
        props = []
        path = failure.file
        if path and ".." not in path.split("/") and not path.startswith("/"):
            props.append("file=" + _escape(path, property_value=True))
        if failure.line:
            props.append(f"line={failure.line}")
        title = _heading(failure)[:180]
        props.append("title=" + _escape(title, property_value=True))
        message = (failure.excerpt.strip() or failure.detail or title)[:_MAX_ANNOTATION]
        attr = (" " + ",".join(props)) if props else ""
        commands.append(f"::error{attr}::{_escape(message, property_value=False)}")
    if not commands:
        return ""
    return "\n".join(commands) + "\n"


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if len(args) != 1:
        print("usage: report_bazel_failures.py BAZEL_LOG", file=sys.stderr)
        return 2
    path = args[0]
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            text = handle.read()
    except OSError as exc:
        print(f"error: cannot read {path}: {exc}", file=sys.stderr)
        return 2

    report = parse_failures(text)
    if not report.failures:
        report = Report(
            failures=[
                Failure(
                    label="",
                    detail="command failed",
                    excerpt=_fallback_excerpt(text) or "(log was empty)",
                )
            ],
            notes=report.notes,
        )
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


def _normalize(text: str) -> list[str]:
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    text = _ANSI.sub("", text)
    return text.splitlines()


def _error_block(lines: list[str], start: int) -> tuple[list[str], int]:
    block = [lines[start]]
    index = start + 1
    while index < len(lines):
        nxt = lines[index]
        if (
            nxt.startswith("ERROR: ")
            or nxt.startswith("INFO: ")
            or nxt.startswith("FAILED: ")
            or nxt.startswith("Use --verbose_failures")
            or _TARGET_LINE.match(nxt)
            or _FAIL_TARGET.match(nxt)
        ):
            break
        block.append(nxt)
        index += 1
    return block, index


def _failure_from_error(lines: list[str], start: int, block: list[str]) -> Failure:
    first = _shorten_error_line(block[0])
    body = [_shorten_line(line) for line in block[1:] if not _is_command_line(line)]
    parts = _preceding_diagnostics(lines, start)
    parts.append(first)
    parts.extend(body)
    excerpt = _cap("\n".join(parts))
    label = _target_from_error(block[0]) or ""
    wrapper = bool(re.search(r"\bRunning \S+ failed:", block[0])) and not _has_diagnostic(excerpt)
    file_path, file_line = _first_location(excerpt)
    return Failure(
        label=label,
        detail="build failed",
        excerpt=excerpt,
        file=file_path,
        line=file_line,
        wrapper=wrapper,
    )


def _preceding_diagnostics(lines: list[str], index: int) -> list[str]:
    """Compiler diagnostics Bazel printed just before the `ERROR:` summary."""
    cursor = index - 1
    while cursor >= 0 and lines[cursor].strip() == "":
        cursor -= 1
    collected_rev: list[str] = []
    while cursor >= 0 and len(collected_rev) < 200:
        line = lines[cursor]
        if (
            _PROGRESS.match(line)
            or line.startswith("INFO: ")
            or line.startswith("ERROR: ")
            or line.startswith("FAILED: ")
            or line.startswith("FAIL: ")
            or _TARGET_LINE.match(line)
        ):
            break
        collected_rev.append(line)
        cursor -= 1
    collected = list(reversed(collected_rev))
    for start, line in enumerate(collected):
        if _DIAGNOSTIC.match(line):
            return [_shorten_line(item) for item in collected[start:]]
    return []


def _excerpt_for_test(item: _TestHit, read_log: LogReader) -> str:
    candidates: list[str] = []
    if item.log_path:
        candidates.append(item.log_path)
    fallback = _label_to_test_log(item.label)
    if fallback and fallback not in candidates:
        candidates.append(fallback)
    for path in candidates:
        contents = read_log(path)
        if contents:
            return _extract_test_excerpt(contents)
    if item.log_path:
        return f"test log: {item.log_path}"
    return ""


def _extract_test_excerpt(text: str) -> str:
    body = _strip_test_header(text)
    patterns = (
        r"(?m)^failures:\n+---- ",
        r"(?m)^thread '.+' panicked at ",
        r"(?m)^={10,}\n(?:FAIL|ERROR): ",
        r"(?m)^Traceback \(most recent call last\):",
        r"(?m)^--- FAIL: ",
        r"(?m)^error(?:\[E\d+\])?: ",
    )
    for pattern in patterns:
        match = re.search(pattern, body)
        if match:
            return _cap(body[match.start() :])
    lines = body.splitlines()
    if len(lines) > _MAX_LINES:
        return _cap("\n".join(lines[-_MAX_LINES:]))
    return _cap(body)


def _strip_test_header(text: str) -> str:
    lines = text.splitlines()
    for index, line in enumerate(lines[:30]):
        if re.match(r"^-{5,}\s*$", line):
            return "\n".join(lines[index + 1 :]).strip()
    if lines and lines[0].startswith("exec ${PAGER"):
        lines = lines[1:]
        if lines and lines[0].startswith("Executing tests from "):
            lines = lines[1:]
        return "\n".join(lines).strip()
    return text.strip()


def _label_to_test_log(label: str) -> str | None:
    bare = label
    if bare.startswith("@@//"):
        bare = bare[2:]
    elif bare.startswith("@"):
        return None
    if not bare.startswith("//") or ":" not in bare:
        return None
    package, name = bare[2:].split(":", 1)
    if not name:
        return None
    parts = ["bazel-testlogs"]
    if package:
        parts.append(package)
    parts.extend([name, "test.log"])
    return "/".join(parts)


def _context_before(lines: list[str], index: int, limit: int) -> str:
    chunk: list[str] = []
    cursor = index - 1
    while cursor >= 0 and len(chunk) < limit:
        line = lines[cursor]
        if _PROGRESS.match(line) or line.startswith("INFO: Elapsed") or line.startswith("ERROR: "):
            break
        chunk.append(_shorten_line(line))
        cursor -= 1
    chunk.reverse()
    chunk.append(lines[index])
    return _cap("\n".join(chunk))


def _target_from_error(line: str) -> str | None:
    from_target = re.search(r"\(from target ([^\s)]+)\)", line)
    if from_target:
        return from_target.group(1)
    found = _LABEL_ANYWHERE.findall(line)
    if not found:
        return None
    return found[0]


def _first_location(excerpt: str) -> tuple[str | None, int | None]:
    for match in _LOC_PANIC.finditer(excerpt):
        rel = _relativize(match.group(1))
        if rel:
            return rel, int(match.group(2))
    for match in _LOC_ARROW.finditer(excerpt):
        rel = _relativize(match.group(1))
        if rel:
            return rel, int(match.group(2))
    return None, None


def _relativize(path: str) -> str | None:
    path = path.replace("\\", "/")
    workspace = os.environ.get("GITHUB_WORKSPACE", "").replace("\\", "/")
    if workspace and path.startswith(workspace.rstrip("/") + "/"):
        path = path[len(workspace.rstrip("/")) + 1 :]
    else:
        marker = "/execroot/_main/"
        if marker in path:
            path = path.split(marker, 1)[1]
        elif path.startswith("/") or re.match(r"^[A-Za-z]:/", path):
            return None
    if (
        not path
        or path.startswith("/")
        or path.startswith("external/")
        or path.startswith("bazel-out/")
        or path.startswith("../")
        or ".." in path.split("/")
    ):
        return None
    return path


def _fallback_excerpt(text: str) -> str:
    kept = []
    for line in _normalize(text):
        if _PROGRESS.match(line) or line.startswith("INFO: From ") or not line.strip():
            continue
        kept.append(_shorten_line(line))
    if len(kept) > 40:
        kept = kept[-40:]
    return "\n".join(kept)


def _drop_wrappers(failures: list[Failure]) -> list[Failure]:
    specifics = [failure for failure in failures if not failure.wrapper]
    if specifics:
        return specifics
    return failures


def _limited(failures: list[Failure]) -> list[Failure]:
    if len(failures) <= _MAX_FAILURES:
        return failures
    hidden = failures[_MAX_FAILURES:]
    names = ", ".join(failure.label or failure.detail for failure in hidden)
    more = Failure(
        label="",
        detail=f"{len(hidden)} more failures",
        excerpt=names,
    )
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
        return f"{failure.label} {failure.detail}".strip()
    return (failure.label or failure.detail or "command failed").strip()


def _has_diagnostic(text: str) -> bool:
    return (
        re.search(
            r"(?m)^error(?:\[E\d+\])?: |^thread '.+' panicked at |^--- FAIL: |^Traceback ",
            text,
        )
        is not None
    )


def _is_command_line(line: str) -> bool:
    stripped = line.strip()
    if stripped.startswith("(cd ") or stripped.startswith("bazel-out/"):
        return True
    if "process_wrapper" in stripped:
        return True
    return len(stripped) > 300 and "bazel-out/" in stripped


def _shorten_error_line(line: str) -> str:
    marker = " failed:"
    idx = line.find(marker)
    if idx == -1:
        return _shorten_line(line)
    rest = line[idx:]
    match = re.match(r" failed: \([^)]*\)", rest)
    if match:
        return line[: idx + match.end()]
    return line[: idx + len(marker)]


def _shorten_line(line: str) -> str:
    if len(line) > _MAX_LINE:
        return line[:_MAX_LINE] + "…"
    return line


def _cap(text: str) -> str:
    lines = [_shorten_line(line) for line in text.splitlines()]
    if len(lines) > _MAX_LINES:
        lines = lines[:_MAX_LINES]
        lines.append("… (truncated)")
    clipped = "\n".join(lines).strip()
    if len(clipped) > _MAX_CHARS:
        clipped = clipped[:_MAX_CHARS].rstrip() + "\n… (truncated)"
    return clipped


def _neutralize(text: str) -> str:
    """Keep test output from being parsed as an Actions workflow command."""
    neutralized = []
    for line in text.splitlines():
        if line.startswith("::"):
            line = " " + line
        neutralized.append(line)
    return "\n".join(neutralized)


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
