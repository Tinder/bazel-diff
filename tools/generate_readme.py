"""Generates README.md by injecting live CLI help and a contributors table."""

import json
import os
import re
import subprocess
import sys
import time
import urllib.request
from collections import defaultdict
from pathlib import Path


REPO = "Tinder/bazel-diff"


# ---------------------------------------------------------------------------
# Runfiles helpers
# ---------------------------------------------------------------------------

def runfile(rel_path: str) -> Path:
    """Resolve a path relative to the Bazel runfiles tree.

    Data files declared in the py_binary's `data` attribute are placed
    alongside the script under `_main/<package>/` in the runfiles tree.
    Since __file__ is already at `…/_main/tools/generate_readme.py`, all
    sibling data files are in the same directory — we just strip the leading
    package prefix from rel_path.
    """
    # Preferred: RUNFILES_DIR is set when run as a data dep of another target.
    runfiles_dir = os.environ.get("RUNFILES_DIR")
    if runfiles_dir:
        return Path(runfiles_dir) / "_main" / rel_path

    # Standard: __file__ is the runfiles copy; data files are siblings.
    script_dir = Path(__file__).parent  # …/_main/tools/
    parts = rel_path.split("/", 1)
    if len(parts) == 2 and parts[0] == "tools":
        return script_dir / parts[1]
    return script_dir / rel_path


# ---------------------------------------------------------------------------
# Sentinel injection
# ---------------------------------------------------------------------------

def inject_section(text: str, marker: str, content: str) -> str:
    """Replace the content between BEGIN/END sentinel comments for *marker*."""
    begin = f"<!-- BEGIN_SECTION: {marker} -->"
    end = f"<!-- END_SECTION: {marker} -->"
    pattern = re.compile(
        rf"({re.escape(begin)}\n).*?({re.escape(end)})",
        re.DOTALL,
    )
    replacement = rf"\g<1>{content}\n\g<2>"
    result, count = pattern.subn(replacement, text)
    if count == 0:
        raise ValueError(f"Sentinel markers for '{marker}' not found in template")
    return result


# ---------------------------------------------------------------------------
# CLI help section
# ---------------------------------------------------------------------------

def build_cli_help_section(help_root: str, help_gen: str, help_get: str) -> str:
    lines = [
        "## CLI Interface",
        "",
        "`bazel-diff` Command",
        "",
        "```terminal",
        help_root.rstrip(),
        "```",
        "",
        "### `generate-hashes` command",
        "",
        "```terminal",
        help_gen.rstrip(),
        "```",
        "",
        "### `get-impacted-targets` command",
        "",
        "```terminal",
        help_get.rstrip(),
        "```",
    ]
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# GitHub user resolution
# ---------------------------------------------------------------------------

def github_headers() -> dict:
    headers = {
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "bazel-diff-readme-gen",
    }
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def fetch_github_email_map(repo: str) -> dict[str, dict]:
    """Page through the GitHub commits API and return email -> {login, avatar_url}."""
    email_map: dict[str, dict] = {}
    page = 1
    while True:
        url = f"https://api.github.com/repos/{repo}/commits?per_page=100&page={page}"
        req = urllib.request.Request(url, headers=github_headers())
        try:
            with urllib.request.urlopen(req, timeout=15) as resp:
                commits = json.load(resp)
        except Exception as exc:
            print(f"Warning: GitHub API error on page {page}: {exc}", file=sys.stderr)
            break

        for commit in commits:
            gh_author = commit.get("author")
            email = (commit.get("commit") or {}).get("author", {}).get("email", "")
            if gh_author and email and email not in email_map:
                email_map[email] = {
                    "login": gh_author["login"],
                    "avatar_url": gh_author["avatar_url"],
                }

        if len(commits) < 100:
            break
        page += 1
        time.sleep(0.1)

    return email_map


def resolve_noreply_username(email: str) -> str | None:
    """Extract a GitHub username from a noreply address as a last-resort fallback."""
    if not email.endswith("@users.noreply.github.com"):
        return None
    local = email.split("@")[0]
    # Strip leading numeric id: "12345+username" -> "username"
    return local.split("+")[-1]


# ---------------------------------------------------------------------------
# Contributors section
# ---------------------------------------------------------------------------

def build_contributors_section(workspace_dir: Path, email_map: dict[str, dict]) -> str:
    # Collect (name, email) pairs with counts from git log.
    result = subprocess.run(
        ["git", "-C", str(workspace_dir), "log", "--format=%aN\t%aE"],
        capture_output=True,
        text=True,
        check=True,
    )

    # Aggregate: per (name, email) pair, then roll up by name keeping
    # the email that has the most commits for that name.
    pair_counts: dict[tuple[str, str], int] = defaultdict(int)
    for line in result.stdout.splitlines():
        if "\t" not in line:
            continue
        name, email = line.split("\t", 1)
        name, email = name.strip(), email.strip()
        if name:
            pair_counts[(name, email)] += 1

    # Roll up by name: sum counts, pick the email with the highest count.
    name_totals: dict[str, int] = defaultdict(int)
    name_best_email: dict[str, str] = {}
    name_best_count: dict[str, int] = defaultdict(int)

    for (name, email), count in pair_counts.items():
        name_totals[name] += count
        if count > name_best_count[name]:
            name_best_count[name] = count
            name_best_email[name] = email

    # Sort by total commit count descending.
    sorted_authors = sorted(name_totals.items(), key=lambda x: x[1], reverse=True)

    rows = ["| | Name | Commits |", "| --- | --- | --- |"]
    for name, total in sorted_authors:
        email = name_best_email[name]
        user = email_map.get(email)

        # Fallback: try to extract username from noreply address.
        if not user:
            login = resolve_noreply_username(email)
            if login:
                user = {
                    "login": login,
                    "avatar_url": f"https://avatars.githubusercontent.com/{login}",
                }

        if user:
            login = user["login"]
            base_avatar = user["avatar_url"].split("?")[0]
            avatar = base_avatar + "?s=40"
            profile = f"https://github.com/{login}"
            rows.append(
                f"| [![{name}]({avatar})]({profile}) | [{name}]({profile}) | {total} |"
            )
        else:
            rows.append(f"| | {name} | {total} |")

    return "\n".join(rows)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    workspace_dir = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    output_path = workspace_dir / "README.md"

    template = runfile("tools/readme_template.md").read_text()
    help_root = runfile("tools/help_root.txt").read_text()
    help_gen = runfile("tools/help_generate_hashes.txt").read_text()
    help_get = runfile("tools/help_get_impacted_targets.txt").read_text()

    print("Fetching GitHub user data...")
    email_map = fetch_github_email_map(REPO)

    print("Building contributors table...")
    contributors_section = build_contributors_section(workspace_dir, email_map)

    cli_help_section = build_cli_help_section(help_root, help_gen, help_get)

    readme = inject_section(template, "cli-help", cli_help_section)
    readme = inject_section(readme, "contributors", contributors_section)

    output_path.write_text(readme)
    print(f"README.md written to {output_path}")


if __name__ == "__main__":
    main()
