#!/usr/bin/env python3
"""Deterministic, hermetic workloads for the Kotlin/Rust performance gate.

``tools/benchmark.py`` measures the two implementations against a *real* Bazel
workspace: it needs a checkout, a Bazel install and Hyperfine, and its numbers
describe that one workspace. That is the right tool for "how much faster is Rust
on our monorepo today", and the wrong tool for a CI gate.

This module builds the inputs for the gate instead, from nothing but the Python
standard library:

* a synthetic ``streamed_proto`` fixture -- a length-delimited stream of
  ``blaze_query.Target`` messages, byte-for-byte the shape Bazel emits for
  ``bazel query //...:all-targets --output=streamed_proto
  --proto:instantiation_stack``;
* the workspace those targets refer to, so source-file and ``.bzl``-seed hashing
  have real bytes to read;
* a replay ``bazel`` shim that answers ``version`` / ``mod`` / ``info`` /
  ``query`` from that fixture, so no Bazel server is involved and the measured
  wall time is bazel-diff's own work;
* hash files (and dependency edges) for the ``get-impacted-targets`` scenarios.

Everything is a pure function of the spec: same spec, same bytes, on any
machine. That is what makes a timing comparison between two binaries meaningful
-- both implementations parse the identical fixture and, if they disagree about
the answer, the gate fails on parity before it ever reports a speedup.

The protobuf encoder below is deliberately tiny: proto2 wire format is varints
and length-delimited fields, and hand-rolling the handful of fields the fixture
needs keeps this module dependency-free (no protobuf runtime, no generated
code, no Bazel action to produce it).
"""

from __future__ import annotations

import hashlib
import json
import shlex
from dataclasses import dataclass
from pathlib import Path

# blaze_query.Target.Discriminator
TARGET_RULE = 1
TARGET_SOURCE_FILE = 2
TARGET_GENERATED_FILE = 3

# blaze_query.Attribute.Discriminator
ATTRIBUTE_STRING = 2
ATTRIBUTE_LABEL = 3
ATTRIBUTE_STRING_LIST = 5
ATTRIBUTE_LABEL_LIST = 6
ATTRIBUTE_BOOLEAN = 14


# --------------------------------------------------------------------------
# Minimal proto2 wire-format encoder
# --------------------------------------------------------------------------


def encode_varint(value: int) -> bytes:
    """Encode ``value`` as a protobuf base-128 varint."""
    if value < 0:
        raise ValueError(f"varint must not be negative: {value}")
    parts = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        parts.append(byte | (0x80 if value else 0))
        if not value:
            return bytes(parts)


def _tag(field_number: int, wire_type: int) -> bytes:
    return encode_varint((field_number << 3) | wire_type)


def encode_bytes_field(field_number: int, payload: bytes) -> bytes:
    """Encode a length-delimited field (wire type 2)."""
    return _tag(field_number, 2) + encode_varint(len(payload)) + payload


def encode_string_field(field_number: int, value: str) -> bytes:
    return encode_bytes_field(field_number, value.encode("utf-8"))


def encode_varint_field(field_number: int, value: int) -> bytes:
    """Encode an int32/enum/bool field (wire type 0)."""
    return _tag(field_number, 0) + encode_varint(value)


def encode_delimited(messages: list[bytes]) -> bytes:
    """Concatenate messages in ``parseDelimitedFrom`` framing.

    Each message is prefixed with its length as a varint -- the framing Bazel's
    ``streamed_proto`` output uses and both implementations decode.
    """
    return b"".join(encode_varint(len(message)) + message for message in messages)


# --------------------------------------------------------------------------
# blaze_query messages
# --------------------------------------------------------------------------


def encode_attribute(
    name: str,
    discriminator: int,
    *,
    string_value: str | None = None,
    string_list_value: tuple[str, ...] = (),
    boolean_value: bool | None = None,
    explicitly_specified: bool = True,
) -> bytes:
    """Encode ``blaze_query.Attribute``.

    Fields are emitted in ascending field-number order, matching what Bazel
    writes. Rule digests re-serialize the parsed attribute, so both
    implementations normalize this anyway -- but a faithful fixture keeps the
    bytes the parsers see identical to production input.
    """
    payload = encode_string_field(1, name) + encode_varint_field(2, discriminator)
    if string_value is not None:
        payload += encode_string_field(5, string_value)
    for value in string_list_value:
        payload += encode_string_field(6, value)
    payload += encode_varint_field(13, int(explicitly_specified))
    if boolean_value is not None:
        payload += encode_varint_field(14, int(boolean_value))
    return payload


def encode_rule(
    name: str,
    rule_class: str,
    *,
    location: str,
    attributes: tuple[bytes, ...],
    rule_inputs: tuple[str, ...],
    rule_outputs: tuple[str, ...],
    starlark_environment_hash: str,
    instantiation_stack: tuple[str, ...],
) -> bytes:
    """Encode ``blaze_query.Rule``."""
    payload = encode_string_field(1, name) + encode_string_field(2, rule_class)
    payload += encode_string_field(3, location)
    for attribute in attributes:
        payload += encode_bytes_field(4, attribute)
    for rule_input in rule_inputs:
        payload += encode_string_field(5, rule_input)
    for rule_output in rule_outputs:
        payload += encode_string_field(6, rule_output)
    payload += encode_string_field(12, starlark_environment_hash)
    for frame in instantiation_stack:
        payload += encode_string_field(13, frame)
    return payload


def encode_source_file(name: str, *, location: str) -> bytes:
    """Encode ``blaze_query.SourceFile``."""
    return encode_string_field(1, name) + encode_string_field(2, location)


def encode_generated_file(name: str, *, generating_rule: str, location: str) -> bytes:
    """Encode ``blaze_query.GeneratedFile``."""
    return (
        encode_string_field(1, name)
        + encode_string_field(2, generating_rule)
        + encode_string_field(3, location)
    )


def encode_target(discriminator: int, payload: bytes) -> bytes:
    """Wrap an inner message in ``blaze_query.Target``."""
    field_number = {
        TARGET_RULE: 2,
        TARGET_SOURCE_FILE: 3,
        TARGET_GENERATED_FILE: 4,
    }[discriminator]
    return encode_varint_field(1, discriminator) + encode_bytes_field(
        field_number, payload
    )


# --------------------------------------------------------------------------
# Synthetic build graph
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class GraphSpec:
    """Shape of the synthetic build graph a ``generate-hashes`` scenario hashes.

    The graph is layered: package ``i`` in layer ``k`` depends only on packages
    in layer ``k - 1``. That keeps it acyclic and bounds transitive depth at
    ``layers``, which is what a modular monorepo looks like -- and, unlike one
    long chain, it does not make transitive hashing quadratic in a way that
    would drown out the per-target costs the gate is trying to observe.
    """

    packages: int
    rules_per_package: int = 3
    sources_per_rule: int = 2
    fanin: int = 2
    layers: int = 8
    bzl_files: int = 8
    source_bytes: int = 512

    def __post_init__(self) -> None:
        for field, value in (
            ("packages", self.packages),
            ("rules_per_package", self.rules_per_package),
            ("layers", self.layers),
            ("bzl_files", self.bzl_files),
        ):
            if value < 1:
                raise ValueError(f"{field} must be at least 1, got {value}")
        for field, value in (
            ("sources_per_rule", self.sources_per_rule),
            ("fanin", self.fanin),
            ("source_bytes", self.source_bytes),
        ):
            if value < 0:
                raise ValueError(f"{field} must not be negative, got {value}")

    @property
    def rule_count(self) -> int:
        return self.packages * self.rules_per_package

    @property
    def source_count(self) -> int:
        return self.rule_count * self.sources_per_rule

    @property
    def target_count(self) -> int:
        # Every rule also emits exactly one generated file.
        return self.rule_count * 2 + self.source_count

    def scaled(self, factor: float) -> GraphSpec:
        """Return this spec with the package count scaled by ``factor``."""
        if factor <= 0:
            raise ValueError(f"scale factor must be positive, got {factor}")
        return GraphSpec(
            packages=max(1, round(self.packages * factor)),
            rules_per_package=self.rules_per_package,
            sources_per_rule=self.sources_per_rule,
            fanin=self.fanin,
            layers=self.layers,
            bzl_files=self.bzl_files,
            source_bytes=self.source_bytes,
        )


def package_name(index: int) -> str:
    return f"pkg/p{index:05d}"


def layer_of(index: int, spec: GraphSpec) -> int:
    return (index * spec.layers) // spec.packages


def upstream_packages(index: int, spec: GraphSpec) -> list[int]:
    """Deterministically pick the packages that package ``index`` depends on.

    Multiplicative hashing over the previous layer's index range: cheap,
    reproducible, and spread out enough that the dependency graph is not a
    handful of hot nodes.
    """
    layer = layer_of(index, spec)
    if layer == 0 or spec.fanin == 0:
        return []
    low = ((layer - 1) * spec.packages) // spec.layers
    high = (layer * spec.packages) // spec.layers
    span = high - low
    if span <= 0:
        return []
    return sorted({low + (index * 2654435761 + step * 40503) % span for step in range(spec.fanin)})


def _bzl_path(index: int, spec: GraphSpec) -> str:
    return f"defs/macro_{index % spec.bzl_files}.bzl"


def _source_text(package: int, rule: int, source: int, spec: GraphSpec) -> str:
    header = f"// package {package} rule {rule} source {source}\n"
    filler = f"{package}:{rule}:{source}:"
    body = (filler * (spec.source_bytes // len(filler) + 1))[: spec.source_bytes]
    return header + body + "\n"


def _bzl_text(index: int) -> str:
    return (
        f'"""Synthetic macro file {index}."""\n\n'
        f"def macro_{index}(name, srcs, deps = []):\n"
        f'    native.genrule(\n'
        f"        name = name,\n"
        f"        srcs = srcs + deps,\n"
        f'        outs = [name + ".out"],\n'
        f'        cmd = "cat $(SRCS) > $@",\n'
        f"    )\n"
    )


def encode_graph(spec: GraphSpec) -> bytes:
    """Render ``spec`` as a length-delimited ``streamed_proto`` target stream."""
    messages: list[bytes] = []
    for package in range(spec.packages):
        package_label = f"//{package_name(package)}"
        build_file = f"{package_name(package)}/BUILD"
        upstream = upstream_packages(package, spec)
        for rule in range(spec.rules_per_package):
            rule_label = f"{package_label}:t{rule}"
            source_labels = tuple(
                f"{package_label}:src_{rule}_{source}.txt"
                for source in range(spec.sources_per_rule)
            )
            for position, source_label in enumerate(source_labels):
                messages.append(
                    encode_target(
                        TARGET_SOURCE_FILE,
                        encode_source_file(
                            source_label,
                            location=f"{build_file}:{rule * 10 + position + 1}:1",
                        ),
                    )
                )
            dependency_labels = tuple(
                f"//{package_name(other)}:t{rule % spec.rules_per_package}"
                for other in upstream
            )
            output_label = f"{rule_label}.out"
            bzl = _bzl_path(package, spec)
            messages.append(
                encode_target(
                    TARGET_RULE,
                    encode_rule(
                        rule_label,
                        "genrule",
                        location=f"{build_file}:{rule * 10 + 1}:1",
                        attributes=(
                            encode_attribute(
                                "name", ATTRIBUTE_STRING, string_value=f"t{rule}"
                            ),
                            encode_attribute(
                                "srcs",
                                ATTRIBUTE_LABEL_LIST,
                                string_list_value=source_labels + dependency_labels,
                            ),
                            encode_attribute(
                                "outs",
                                ATTRIBUTE_LABEL_LIST,
                                string_list_value=(output_label,),
                            ),
                            encode_attribute(
                                "cmd",
                                ATTRIBUTE_STRING,
                                string_value="cat $(SRCS) > $@",
                            ),
                            encode_attribute(
                                "tags",
                                ATTRIBUTE_STRING_LIST,
                                string_list_value=("synthetic", f"layer{layer_of(package, spec)}"),
                            ),
                            encode_attribute(
                                "visibility",
                                ATTRIBUTE_LABEL_LIST,
                                string_list_value=("//visibility:public",),
                            ),
                            encode_attribute(
                                "testonly", ATTRIBUTE_BOOLEAN, boolean_value=False
                            ),
                            encode_attribute(
                                "generator_function",
                                ATTRIBUTE_STRING,
                                string_value=f"macro_{package % spec.bzl_files}",
                            ),
                        ),
                        rule_inputs=source_labels + dependency_labels,
                        rule_outputs=(output_label,),
                        starlark_environment_hash=hashlib.sha256(
                            f"{package}:{rule}".encode()
                        ).hexdigest(),
                        instantiation_stack=(
                            f"{build_file}:{rule * 10 + 1}:1: in <toplevel>",
                            f"{bzl}:{rule + 3}:12: in macro_{package % spec.bzl_files}",
                        ),
                    ),
                )
            )
            messages.append(
                encode_target(
                    TARGET_GENERATED_FILE,
                    encode_generated_file(
                        output_label,
                        generating_rule=rule_label,
                        location=f"{build_file}:{rule * 10 + 1}:1",
                    ),
                )
            )
    return encode_delimited(messages)


def write_workspace(root: Path, spec: GraphSpec) -> Path:
    """Materialize the workspace the encoded graph refers to.

    Source files and ``.bzl`` files must exist on disk: both implementations
    read them to hash source targets and to seed rule digests from the macro
    that instantiated them. Missing files would still "work" (they hash as
    absent) but would skip exactly the I/O the gate wants to measure.
    """
    root.mkdir(parents=True, exist_ok=True)
    (root / "WORKSPACE").write_text('workspace(name = "perf_gate")\n')
    (root / "BUILD").write_text("# synthetic root package\n")
    definitions = root / "defs"
    definitions.mkdir(exist_ok=True)
    (definitions / "BUILD").write_text("# macro definitions\n")
    for index in range(spec.bzl_files):
        (definitions / f"macro_{index}.bzl").write_text(_bzl_text(index))
    for package in range(spec.packages):
        package_dir = root / package_name(package)
        package_dir.mkdir(parents=True, exist_ok=True)
        (package_dir / "BUILD").write_text(
            f'load("//defs:macro_{package % spec.bzl_files}.bzl", '
            f'"macro_{package % spec.bzl_files}")\n'
        )
        for rule in range(spec.rules_per_package):
            for source in range(spec.sources_per_rule):
                (package_dir / f"src_{rule}_{source}.txt").write_text(
                    _source_text(package, rule, source, spec)
                )
    return root


def write_graph_fixture(path: Path, spec: GraphSpec) -> Path:
    """Write the encoded target stream to ``path``."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(encode_graph(spec))
    return path


# --------------------------------------------------------------------------
# Replay Bazel shim
# --------------------------------------------------------------------------

# Kept in step with the shim in tools/benchmark.py: bazel-diff probes the Bazel
# version and picks its query spelling from it, so the reported version decides
# which code path is exercised. 9.2.0 selects the `--output_file` path used by
# every currently supported Bazel.
REPLAY_BAZEL_VERSION = "9.2.0"


def write_replay_bazel(path: Path, fixture: Path, output_base: Path) -> Path:
    """Write a ``bazel`` stand-in that replays ``fixture`` for every query.

    Answers exactly the four subcommands bazel-diff issues while generating
    hashes:

    * ``version`` -- a fixed ``Build label:`` line;
    * ``mod``     -- non-zero, i.e. "this workspace is not bzlmod", which keeps
      the module-graph code path (and its extra queries) out of the measurement;
    * ``info``    -- the fake output base;
    * ``query``   -- the fixture, written to ``--output_file`` when the caller
      asked for one and to stdout otherwise.

    Replaying instead of running Bazel is what makes the gate hermetic: no
    server start-up, no analysis, no cache state -- just the bytes both
    implementations have to parse.
    """
    output_base.mkdir(parents=True, exist_ok=True)
    script = f"""#!/usr/bin/env bash
set -euo pipefail
fixture={shlex.quote(str(fixture))}
output_base={shlex.quote(str(output_base))}
args=("$@")
command=
command_index=-1
for index in "${{!args[@]}}"; do
  case "${{args[$index]}}" in
    version|mod|info|query)
      command="${{args[$index]}}"
      command_index=$index
      break
      ;;
  esac
done
case "$command" in
  version)
    echo 'Build label: {REPLAY_BAZEL_VERSION}'
    ;;
  mod)
    exit 1
    ;;
  info)
    echo "$output_base"
    ;;
  query)
    output_file=
    for ((index = command_index + 1; index < ${{#args[@]}}; index++)); do
      case "${{args[$index]}}" in
        --output_file)
          ((index += 1))
          output_file="${{args[$index]}}"
          ;;
        --output_file=*)
          output_file="${{args[$index]#--output_file=}}"
          ;;
      esac
    done
    if [[ -n "$output_file" ]]; then
      cp "$fixture" "$output_file"
    else
      cat "$fixture"
    fi
    ;;
  *)
    echo "unsupported replay invocation: $*" >&2
    exit 2
    ;;
esac
"""
    path.write_text(script)
    path.chmod(0o755)
    return path


# --------------------------------------------------------------------------
# Hash files for get-impacted-targets
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class HashFileSpec:
    """Shape of the hash-file pair a ``get-impacted-targets`` scenario diffs."""

    targets: int
    changed_ratio: float = 0.05
    added_ratio: float = 0.01
    removed_ratio: float = 0.01
    deps_per_target: int = 4

    def __post_init__(self) -> None:
        if self.targets < 1:
            raise ValueError(f"targets must be at least 1, got {self.targets}")
        if self.deps_per_target < 0:
            raise ValueError(
                f"deps_per_target must not be negative, got {self.deps_per_target}"
            )
        for field, value in (
            ("changed_ratio", self.changed_ratio),
            ("added_ratio", self.added_ratio),
            ("removed_ratio", self.removed_ratio),
        ):
            if not 0 <= value <= 1:
                raise ValueError(f"{field} must be within [0, 1], got {value}")

    def scaled(self, factor: float) -> HashFileSpec:
        if factor <= 0:
            raise ValueError(f"scale factor must be positive, got {factor}")
        return HashFileSpec(
            targets=max(1, round(self.targets * factor)),
            changed_ratio=self.changed_ratio,
            added_ratio=self.added_ratio,
            removed_ratio=self.removed_ratio,
            deps_per_target=self.deps_per_target,
        )


def hash_label(index: int) -> str:
    return f"//{package_name(index // 8)}:target_{index % 8}"


def _digest(label: str, revision: str) -> str:
    """Render one hash-file value.

    bazel-diff writes ``<overall>~<direct>`` (optionally prefixed with
    ``<kind>#``) and rejects anything else, so the fixture has to speak the same
    grammar -- a bare digest is not a valid hash file.
    """
    overall = hashlib.sha256(f"{label}@{revision}".encode()).hexdigest()
    direct = hashlib.sha256(f"{label}@{revision}@direct".encode()).hexdigest()
    return f"{overall}~{direct}"


def build_hash_files(spec: HashFileSpec) -> tuple[dict[str, str], dict[str, str]]:
    """Build the ``(starting, final)`` hash maps for a diff scenario.

    The final revision changes a slice of the digests, drops a slice of the
    labels and adds new ones, so the diff exercises modification, deletion and
    addition rather than a single uniform path.
    """
    changed = round(spec.targets * spec.changed_ratio)
    removed = round(spec.targets * spec.removed_ratio)
    added = round(spec.targets * spec.added_ratio)
    starting = {
        hash_label(index): _digest(hash_label(index), "base")
        for index in range(spec.targets)
    }
    final: dict[str, str] = {}
    for index in range(spec.targets):
        label = hash_label(index)
        if index < removed:
            continue
        revision = "head" if index % spec.targets < changed + removed else "base"
        final[label] = _digest(label, revision)
    for index in range(spec.targets, spec.targets + added):
        label = hash_label(index)
        final[label] = _digest(label, "head")
    return starting, final


def build_dep_edges(labels: list[str], spec: HashFileSpec) -> dict[str, list[str]]:
    """Build a dependency-edge map over ``labels`` for distance metrics."""
    edges: dict[str, list[str]] = {}
    count = len(labels)
    for index, label in enumerate(labels):
        edges[label] = [
            labels[(index + step * 7 + 1) % count] for step in range(spec.deps_per_target)
        ]
    return edges


def write_hash_files(
    directory: Path, spec: HashFileSpec, *, with_dep_edges: bool = False
) -> tuple[Path, Path, Path | None]:
    """Write the starting/final hash files (and optional dep edges) under ``directory``.

    Returns ``(starting_path, final_path, dep_edges_path)``.
    """
    directory.mkdir(parents=True, exist_ok=True)
    starting, final = build_hash_files(spec)
    starting_path = directory / "starting-hashes.json"
    final_path = directory / "final-hashes.json"
    starting_path.write_text(json.dumps(starting, sort_keys=True))
    final_path.write_text(json.dumps(final, sort_keys=True))
    dep_edges_path: Path | None = None
    if with_dep_edges:
        labels = sorted(set(starting) | set(final))
        dep_edges_path = directory / "dep-edges.json"
        dep_edges_path.write_text(json.dumps(build_dep_edges(labels, spec), sort_keys=True))
    return starting_path, final_path, dep_edges_path
