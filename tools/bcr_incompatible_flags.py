"""Print the incompatible flags BCR presubmit tests for a Bazel version.

The Bazel Central Registry's presubmit rebuilds every `verify_targets` task once
per flag listed in its `incompatible_flags.yml`, for each Bazel version the
entry names, and a registry PR cannot merge until every flag passes. The
BCR consumer workflow (.github/workflows/bcr_consumer.yaml) mirrors that phase
so a change that only breaks under one of those flags fails its own PR instead
of the registry PR for the next release.

The file is a flat mapping, for example:

    incompatible_flags:
      # https://github.com/bazelbuild/bazel/issues/12933
      "--incompatible_config_setting_private_default_visibility":
        - 7.x
        - 8.x

This parses exactly that shape, so the workflow needs no YAML library on the
runner. Entries the registry comments out are skipped like any comment.

Usage:
    bcr_incompatible_flags.py --file incompatible_flags.yml BAZEL_VERSION

BAZEL_VERSION is spelled as the registry spells it ("8.x", "last_green").
Flags print one per line, in file order; no output means the version has
none. The process exits 1 only when the file cannot be read or parsed.
"""

import argparse
import re
import sys

_FLAG_LINE = re.compile(r'^\s+"?(--[^"\s:]+)"?\s*:\s*(?:#.*)?$')
_VERSION_LINE = re.compile(r"^\s+-\s*\"?([^\"\s#]+)\"?\s*(?:#.*)?$")
_ROOT_LINE = re.compile(r"^incompatible_flags\s*:\s*(?:#.*)?$")


def parse_incompatible_flags(text):
    """Map each flag under `incompatible_flags:` to its list of Bazel versions.

    Raises ValueError when the text has no `incompatible_flags:` root or a
    version item appears before any flag.
    """
    flags = {}
    current = None
    seen_root = False
    for number, raw in enumerate(text.splitlines(), start=1):
        line = raw.rstrip()
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if _ROOT_LINE.match(line):
            seen_root = True
            continue
        flag = _FLAG_LINE.match(line)
        if flag:
            current = flag.group(1)
            flags.setdefault(current, [])
            continue
        version = _VERSION_LINE.match(line)
        if version:
            if current is None:
                raise ValueError(f"line {number}: version before any flag: {stripped}")
            flags[current].append(version.group(1))
            continue
        raise ValueError(f"line {number}: unrecognized line: {stripped}")
    if not seen_root:
        raise ValueError("no incompatible_flags: root")
    return flags


def flags_for(bazel_version, flags):
    """Flags whose version list names `bazel_version`, in file order."""
    return [flag for flag, versions in flags.items() if bazel_version in versions]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--file", required=True, help="path to BCR's incompatible_flags.yml")
    parser.add_argument("bazel_version", help='Bazel version as BCR spells it, e.g. "8.x"')
    args = parser.parse_args(argv)
    try:
        with open(args.file, encoding="utf-8") as handle:
            flags = parse_incompatible_flags(handle.read())
    except (OSError, ValueError) as error:
        print(f"bcr_incompatible_flags: {args.file}: {error}", file=sys.stderr)
        return 1
    for flag in flags_for(args.bazel_version, flags):
        print(flag)
    return 0


if __name__ == "__main__":
    sys.exit(main())
