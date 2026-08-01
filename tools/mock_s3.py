#!/usr/bin/env python3
"""A recording, in-memory mock of the small slice of S3 that `bazel-diff serve` uses.

`S3HashCacheStorage` only ever issues GetObject, PutObject and HeadObject against a single bucket
(see cli/src/main/kotlin/com/bazel_diff/server/S3HashCacheStorage.kt), so a few hundred lines of
stdlib `http.server` stand in for MinIO/LocalStack with no container and no third-party dependency
-- and, unlike a real bucket, this one *records every request*. That recording is the point: it is
what lets `serve_consistency.py` compare the hash payloads several `serve` instances independently
produced for the same revision, and prove which instance served which read.

Two read modes, chosen per run:

  * "isolated" -- a GET/HEAD only resolves objects written under the *requesting* instance's own
    key prefix. Every instance therefore generates its own hashes for a revision, giving us N
    independent payloads under one cache key to diff. This is the hash-poisoning detector.
  * "shared"   -- a GET/HEAD resolves against one logical namespace regardless of prefix, i.e. the
    real shared-cache topology. `ObjectRead.served_from_prefix` still records *whose* write
    satisfied the read, which is what turns a `cacheHit: true` in a serve profile into proof that
    the remote tier (rather than the instance's own local disk tier) served it.

Instances always write under their own prefix, so writes stay attributable in both modes.

Wire notes (AWS SDK v2 2.46.7, the version pinned in MODULE.bazel):

  * PutObject arrives **`aws-chunked`-framed**. The SDK's S3 auth-scheme defaults pin
    `PAYLOAD_SIGNING_ENABLED=false` but `CHUNK_ENCODING_ENABLED=true` for PutObject, and the
    default `RequestChecksumCalculation=WHEN_SUPPORTED` appends a CRC32 *trailer* -- which turns
    chunk encoding on. A naive `read(Content-Length)` therefore stores framed bytes, not the JSON,
    and every payload comparison downstream would be garbage. `decode_aws_chunked` handles both
    the unsigned (`<hexlen>\\r\\n`) and signed (`<hexlen>;chunk-signature=<hex>\\r\\n`) spellings.
  * PutObject sends `Expect: 100-continue`; `protocol_version = "HTTP/1.1"` makes
    `BaseHTTPRequestHandler` answer the handshake automatically.
  * A missing key must come back as `404` with an `<Error><Code>NoSuchKey</Code>` body for GET (the
    SDK maps that to `NoSuchKeyException`, the one exception `S3HashCacheStorage.get` treats as a
    plain miss rather than a warning) and as a *bare* `404` for HEAD.
  * Never return 5xx. The SDK retries those, which would double-count writes and silently mask a
    mock bug as a cache miss. Unhandled requests are recorded in `bad_requests()` and answered
    `400`, so a harness can assert the list is empty instead of going green on nothing.

Pure stdlib. `decode_aws_chunked`, `parse_object_path`, `split_prefix` and `error_xml` are free
functions with no I/O so they can be unit-tested directly (see mock_s3_test.py).
"""

from __future__ import annotations

import contextlib
import email.utils
import hashlib
import threading
import time
import urllib.parse
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Iterable, Mapping, Sequence

# ----------------------------------------------------------------------------------------------
# Pure helpers
# ----------------------------------------------------------------------------------------------


class ChunkDecodeError(Exception):
    """Raised when an `aws-chunked` body is malformed or its decoded length is not as declared."""


def decode_aws_chunked(body: bytes, expected_len: int | None = None) -> tuple[bytes, dict]:
    """Strips `aws-chunked` framing, returning (payload, trailing_headers).

    The framing is a sequence of `<hexlen>[;chunk-signature=<hex>]\\r\\n<data>\\r\\n` chunks
    terminated by a zero-length chunk, optionally followed by trailer headers (the SDK sends
    `x-amz-checksum-crc32` there) and a blank line. Signed and unsigned chunk headers differ only
    in the `;`-suffixed parameters, which we discard -- there is nothing to verify against, since
    the mock ignores SigV4 entirely.

    [expected_len], when given, is the `x-amz-decoded-content-length` header; a mismatch means we
    mis-parsed the stream and must fail loudly rather than record a corrupt payload.
    """
    payload = bytearray()
    trailers: dict = {}
    pos = 0
    end = len(body)

    while True:
        eol = body.find(b"\r\n", pos)
        if eol == -1:
            raise ChunkDecodeError(f"unterminated chunk header at offset {pos}")
        size_field = body[pos:eol].split(b";", 1)[0].strip()
        pos = eol + 2
        try:
            size = int(size_field, 16)
        except ValueError as exc:
            raise ChunkDecodeError(f"bad chunk size {size_field!r} at offset {pos}") from exc
        if size == 0:
            break
        if pos + size > end:
            raise ChunkDecodeError(f"chunk of {size} bytes overruns the body at offset {pos}")
        payload += body[pos : pos + size]
        pos += size
        if body[pos : pos + 2] != b"\r\n":
            raise ChunkDecodeError(f"missing CRLF after chunk data at offset {pos}")
        pos += 2

    # Whatever follows the terminating chunk is trailer headers, ending at a blank line.
    while pos < end:
        eol = body.find(b"\r\n", pos)
        line = body[pos:end] if eol == -1 else body[pos:eol]
        pos = end if eol == -1 else eol + 2
        if not line.strip():
            continue
        if b":" in line:
            name, value = line.split(b":", 1)
            trailers[name.decode("utf-8", "replace").strip().lower()] = value.decode(
                "utf-8", "replace"
            ).strip()

    decoded = bytes(payload)
    if expected_len is not None and len(decoded) != expected_len:
        raise ChunkDecodeError(
            f"decoded {len(decoded)} bytes but x-amz-decoded-content-length said {expected_len}"
        )
    return decoded, trailers


def parse_object_path(path: str) -> tuple[str, str] | None:
    """`/bucket/a/b.json?x=1` -> `("bucket", "a/b.json")`, percent-decoded.

    Returns None for anything that is not a path-style object address (the service root, a bucket
    listing, a malformed path). Path style is what `--s3ForcePathStyle` produces, and it is the
    only addressing this mock accepts -- virtual-host style would need wildcard DNS.
    """
    raw = path.split("?", 1)[0]
    if not raw.startswith("/"):
        return None
    parts = raw[1:].split("/", 1)
    if len(parts) != 2 or not parts[0] or not parts[1]:
        return None
    return urllib.parse.unquote(parts[0]), urllib.parse.unquote(parts[1])


def split_prefix(key: str, prefixes: Iterable[str]) -> tuple[str | None, str]:
    """Splits a full object key into (owning_prefix, logical_key).

    Longest prefix wins, so `i1/` and `i11/` cannot be confused. A key under no known prefix comes
    back as `(None, key)` -- the caller records it rather than guessing.
    """
    best: str | None = None
    for prefix in prefixes:
        if key.startswith(prefix) and (best is None or len(prefix) > len(best)):
            best = prefix
    if best is None:
        return None, key
    return best, key[len(best) :]


def error_xml(code: str, message: str, key: str = "") -> bytes:
    """The S3 REST error document. `<Code>` is the field the SDK maps to an exception type."""
    return (
        '<?xml version="1.0" encoding="UTF-8"?>'
        "<Error>"
        f"<Code>{code}</Code>"
        f"<Message>{message}</Message>"
        f"<Key>{key}</Key>"
        "<RequestId>mock-s3</RequestId>"
        "<HostId>mock-s3</HostId>"
        "</Error>"
    ).encode("utf-8")


# ----------------------------------------------------------------------------------------------
# Recorded traffic
# ----------------------------------------------------------------------------------------------


@dataclass(frozen=True)
class ObjectWrite:
    seq: int
    ts: float
    instance: str | None  # instance id that owns `prefix`, None if the prefix is unregistered
    prefix: str
    logical_key: str  # the cache key with the instance prefix stripped: `<sha>.<fingerprint>`
    body: bytes
    sha256: str
    size: int
    chunked: bool  # whether the request arrived aws-chunked (it should)


@dataclass(frozen=True)
class ObjectRead:
    seq: int
    ts: float
    instance: str | None
    prefix: str
    logical_key: str
    op: str  # "GET" | "HEAD"
    hit: bool
    served_from_prefix: str | None  # in shared mode, whose write satisfied this read


# ----------------------------------------------------------------------------------------------
# The store
# ----------------------------------------------------------------------------------------------

ISOLATED = "isolated"
SHARED = "shared"


class MockS3Store:
    """Thread-safe object store plus request log. Shared by every handler thread."""

    def __init__(
        self,
        bucket: str,
        prefixes: Mapping[str, str] | None = None,
        mode: str = ISOLATED,
    ) -> None:
        if mode not in (ISOLATED, SHARED):
            raise ValueError(f"unknown mode {mode!r}")
        self.bucket = bucket
        self._mode = mode
        self._lock = threading.Lock()
        self._prefix_to_instance: dict = {}
        self._objects: dict = {}  # full key -> bytes
        self._writes: list = []
        self._reads: list = []
        self._bad: list = []
        self._seq = 0
        # Traffic from prior, already-reset generations, so a caller that resets between phases can
        # still report what the whole run did.
        self._retired = {"puts": 0, "gets": 0, "heads": 0, "hits": 0, "bad_requests": 0}
        for iid, prefix in (prefixes or {}).items():
            self.register(iid, prefix)

    # -- configuration ---------------------------------------------------------------------

    def register(self, instance: str, prefix: str) -> None:
        """Declares that [prefix] belongs to [instance], so its traffic is attributable."""
        with self._lock:
            self._prefix_to_instance[prefix] = instance

    @property
    def mode(self) -> str:
        return self._mode

    def set_mode(self, mode: str) -> None:
        if mode not in (ISOLATED, SHARED):
            raise ValueError(f"unknown mode {mode!r}")
        with self._lock:
            self._mode = mode

    def reset(self) -> None:
        """Drops every object and every recorded request. Cases call this at their boundary so one
        case's writes cannot make the next case's "the follower read from i0" assertion vacuous.

        The request *counts* survive in a retired tally so `total_counts()` still describes the
        whole run; only the objects and the per-request log are cleared.
        """
        with self._lock:
            current = self._counts_locked()
            for key in self._retired:
                self._retired[key] += current[key]
            self._objects.clear()
            self._writes.clear()
            self._reads.clear()
            self._bad.clear()
            self._seq = 0

    # -- operations ------------------------------------------------------------------------

    def _next_seq(self) -> int:
        self._seq += 1
        return self._seq

    def _attribute(self, full_key: str) -> tuple[str | None, str, str]:
        prefix, logical = split_prefix(full_key, self._prefix_to_instance)
        if prefix is None:
            return None, "", full_key
        return self._prefix_to_instance[prefix], prefix, logical

    def _resolve(self, full_key: str) -> tuple[bytes | None, str | None]:
        """Finds the bytes for [full_key], honouring the read mode. Returns (body, served_from)."""
        own = self._objects.get(full_key)
        if own is not None:
            _, prefix, _ = self._attribute(full_key)
            return own, prefix
        if self._mode != SHARED:
            return None, None
        _, _, logical = self._attribute(full_key)
        # Earliest writer wins, so a follower's read is attributed to whoever actually published.
        for write in self._writes:
            if write.logical_key == logical:
                stored = self._objects.get(write.prefix + logical)
                if stored is not None:
                    return stored, write.prefix
        return None, None

    def put(self, full_key: str, body: bytes, chunked: bool) -> None:
        with self._lock:
            instance, prefix, logical = self._attribute(full_key)
            self._objects[full_key] = body
            self._writes.append(
                ObjectWrite(
                    seq=self._next_seq(),
                    ts=time.time(),
                    instance=instance,
                    prefix=prefix,
                    logical_key=logical,
                    body=body,
                    sha256=hashlib.sha256(body).hexdigest(),
                    size=len(body),
                    chunked=chunked,
                )
            )

    def get(self, full_key: str) -> bytes | None:
        with self._lock:
            body, served_from = self._resolve(full_key)
            self._record_read(full_key, "GET", body is not None, served_from)
            return body

    def head(self, full_key: str) -> int | None:
        with self._lock:
            body, served_from = self._resolve(full_key)
            self._record_read(full_key, "HEAD", body is not None, served_from)
            return None if body is None else len(body)

    def _record_read(self, full_key, op, hit, served_from) -> None:
        """Caller must hold the lock."""
        instance, prefix, logical = self._attribute(full_key)
        self._reads.append(
            ObjectRead(
                seq=self._next_seq(),
                ts=time.time(),
                instance=instance,
                prefix=prefix,
                logical_key=logical,
                op=op,
                hit=hit,
                served_from_prefix=served_from,
            )
        )

    def record_bad_request(self, method: str, path: str, detail: str = "") -> None:
        with self._lock:
            self._bad.append((method, path, detail))

    # -- inspection ------------------------------------------------------------------------

    def writes(self, *, instance: str | None = None, logical_key: str | None = None) -> list:
        with self._lock:
            return [
                w
                for w in self._writes
                if (instance is None or w.instance == instance)
                and (logical_key is None or w.logical_key == logical_key)
            ]

    def reads(
        self,
        *,
        instance: str | None = None,
        op: str | None = None,
        logical_key: str | None = None,
    ) -> list:
        with self._lock:
            return [
                r
                for r in self._reads
                if (instance is None or r.instance == instance)
                and (op is None or r.op == op)
                and (logical_key is None or r.logical_key == logical_key)
            ]

    def logical_keys(self) -> list:
        """Every cache key observed, in first-write order."""
        with self._lock:
            seen: list = []
            for w in self._writes:
                if w.logical_key not in seen:
                    seen.append(w.logical_key)
            return seen

    def bodies_for(self, logical_key: str) -> dict:
        """instance id -> the payload it wrote for [logical_key] (its last write, if it wrote more
        than once). Exactly the input `serve_consistency.compare_payloads` expects."""
        with self._lock:
            out: dict = {}
            for w in self._writes:
                if w.logical_key == logical_key and w.instance is not None:
                    out[w.instance] = w.body
            return out

    def bad_requests(self) -> list:
        with self._lock:
            return list(self._bad)

    def _counts_locked(self) -> dict:
        return {
            "puts": len(self._writes),
            "gets": sum(1 for r in self._reads if r.op == "GET"),
            "heads": sum(1 for r in self._reads if r.op == "HEAD"),
            "hits": sum(1 for r in self._reads if r.hit),
            "objects": len(self._objects),
            "bad_requests": len(self._bad),
        }

    def counts(self) -> dict:
        """Traffic since the last `reset()`."""
        with self._lock:
            return self._counts_locked()

    def total_counts(self) -> dict:
        """Traffic across the whole run, including generations already cleared by `reset()`."""
        with self._lock:
            out = self._counts_locked()
            for key, value in self._retired.items():
                out[key] += value
            return out

    def counts_by_instance(self) -> dict:
        with self._lock:
            out: dict = {}
            for iid in set(self._prefix_to_instance.values()):
                out[iid] = {"puts": 0, "gets": 0, "heads": 0, "hits": 0}
            for w in self._writes:
                if w.instance in out:
                    out[w.instance]["puts"] += 1
            for r in self._reads:
                if r.instance in out:
                    out[r.instance]["gets" if r.op == "GET" else "heads"] += 1
                    if r.hit:
                        out[r.instance]["hits"] += 1
            return out


# ----------------------------------------------------------------------------------------------
# HTTP surface
# ----------------------------------------------------------------------------------------------


class _Handler(BaseHTTPRequestHandler):
    # HTTP/1.1 both enables keep-alive (the SDK reuses connections) and makes the base class
    # answer the `Expect: 100-continue` PutObject sends.
    protocol_version = "HTTP/1.1"
    server_version = "MockS3/1.0"

    # Injected by MockS3Server.
    store: MockS3Store

    def log_message(self, fmt, *args) -> None:  # noqa: A003 - stdlib signature
        """Silenced; the store is the log."""

    # -- helpers ---------------------------------------------------------------------------

    def _object_key(self) -> str | None:
        parsed = parse_object_path(self.path)
        if parsed is None:
            return None
        bucket, key = parsed
        if bucket != self.store.bucket:
            return None
        return key

    def _send(self, code: int, body: bytes = b"", ctype: str | None = None, extra=None) -> None:
        self.send_response(code)
        if ctype:
            self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("x-amz-request-id", "mock-s3")
        for name, value in (extra or {}).items():
            self.send_header(name, value)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _send_headers_only(self, code: int, content_length: int, extra=None) -> None:
        """A HEAD response: the same headers a GET would carry, but never a body."""
        self.send_response(code)
        self.send_header("Content-Length", str(content_length))
        self.send_header("x-amz-request-id", "mock-s3")
        for name, value in (extra or {}).items():
            self.send_header(name, value)
        self.end_headers()

    def _no_such_key(self, key: str, head: bool) -> None:
        if head:
            # HeadObject reports a missing key as a bare 404 with no error body; that is exactly
            # the shape S3HashCacheStorage.contains() special-cases.
            self._send_headers_only(404, 0)
        else:
            self._send(404, error_xml("NoSuchKey", "The specified key does not exist.", key),
                       "application/xml")

    def _read_body(self) -> tuple[bytes, bool]:
        """Reads the request body, undoing `aws-chunked` framing. Returns (payload, was_chunked)."""
        encoding = (self.headers.get("Content-Encoding") or "").lower()
        decoded_len = self.headers.get("x-amz-decoded-content-length")
        if (self.headers.get("Transfer-Encoding") or "").lower() == "chunked":
            raw = self._read_http_chunked()
        else:
            raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        if "aws-chunked" in encoding or decoded_len is not None:
            expected = int(decoded_len) if decoded_len is not None else None
            return decode_aws_chunked(raw, expected)[0], True
        return raw, False

    def _read_http_chunked(self) -> bytes:
        """Transport-level `Transfer-Encoding: chunked`, which the SDK does not currently use for
        PutObject but which costs little to tolerate."""
        out = bytearray()
        while True:
            line = self.rfile.readline().split(b";", 1)[0].strip()
            size = int(line, 16)
            if size == 0:
                # Consume trailers up to the blank line.
                while self.rfile.readline().strip():
                    pass
                break
            out += self.rfile.read(size)
            self.rfile.read(2)
        return bytes(out)

    # -- verbs -----------------------------------------------------------------------------

    def do_PUT(self) -> None:  # noqa: N802 - stdlib naming
        key = self._object_key()
        if key is None:
            self.store.record_bad_request("PUT", self.path, "unroutable object path")
            self._send(400, error_xml("InvalidRequest", "unroutable path"), "application/xml")
            return
        try:
            body, chunked = self._read_body()
        except (ChunkDecodeError, ValueError) as exc:
            # Deliberately a 4xx: a 5xx would be retried by the SDK and the resulting cache miss
            # would look like "no divergence found" instead of "the mock is broken".
            self.store.record_bad_request("PUT", self.path, f"body decode failed: {exc}")
            self._send(400, error_xml("InvalidRequest", str(exc), key), "application/xml")
            return
        self.store.put(key, body, chunked)
        self._send(200, b"", extra={"ETag": '"%s"' % hashlib.md5(body).hexdigest()})

    def do_GET(self) -> None:  # noqa: N802
        key = self._object_key()
        if key is None:
            self.store.record_bad_request("GET", self.path, "unroutable object path")
            self._send(400, error_xml("InvalidRequest", "unroutable path"), "application/xml")
            return
        body = self.store.get(key)
        if body is None:
            self._no_such_key(key, head=False)
            return
        self._send(
            200,
            body,
            "application/json",
            extra={
                "ETag": '"%s"' % hashlib.md5(body).hexdigest(),
                "Last-Modified": email.utils.formatdate(usegmt=True),
            },
        )

    def do_HEAD(self) -> None:  # noqa: N802
        key = self._object_key()
        if key is None:
            self.store.record_bad_request("HEAD", self.path, "unroutable object path")
            self._send_headers_only(400, 0)
            return
        size = self.store.head(key)
        if size is None:
            self._no_such_key(key, head=True)
            return
        self._send_headers_only(200, size, extra={"Content-Type": "application/json"})

    def do_DELETE(self) -> None:  # noqa: N802
        # serve never deletes; answer politely and record it so an unexpected op is visible.
        self.store.record_bad_request("DELETE", self.path, "unexpected operation")
        self._send(204)

    def do_POST(self) -> None:  # noqa: N802
        self.store.record_bad_request("POST", self.path, "unexpected operation")
        self._send(400, error_xml("NotImplemented", "unsupported operation"), "application/xml")


class MockS3Server:
    """Runs [store] on a loopback HTTP port. Use as a context manager or call start()/stop()."""

    def __init__(self, store: MockS3Store, host: str = "127.0.0.1", port: int = 0) -> None:
        self.store = store
        handler = type("_BoundHandler", (_Handler,), {"store": store})
        self._httpd = ThreadingHTTPServer((host, port), handler)
        self._httpd.daemon_threads = True
        self._thread: threading.Thread | None = None
        self.host = host
        # Read the port back off the live socket rather than pre-allocating one, so there is no
        # window between choosing a port and binding it.
        self.port = self._httpd.server_address[1]

    @property
    def url(self) -> str:
        return f"http://{self.host}:{self.port}"

    def start(self) -> None:
        self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        with contextlib.suppress(Exception):
            self._httpd.shutdown()
        with contextlib.suppress(Exception):
            self._httpd.server_close()
        if self._thread is not None:
            self._thread.join(timeout=5)

    def __enter__(self) -> "MockS3Server":
        self.start()
        return self

    def __exit__(self, *exc) -> None:
        self.stop()


@contextlib.contextmanager
def mock_s3(bucket: str, prefixes: Mapping[str, str] | None = None, mode: str = ISOLATED):
    """Yields a started (MockS3Store, MockS3Server) pair and shuts the server down on exit."""
    store = MockS3Store(bucket, prefixes, mode)
    server = MockS3Server(store)
    server.start()
    try:
        yield store, server
    finally:
        server.stop()


def instance_prefixes(instance_ids: Sequence[str]) -> dict:
    """`["i0", "i1"]` -> `{"i0": "i0/", "i1": "i1/"}` -- the prefix layout the harness uses.

    Note that `--s3Prefix` is deliberately absent from `ServeCommand.computeConfigFingerprint()`,
    so giving each instance its own prefix keeps writes attributable *without* changing the cache
    key the instances compute. That is what makes cross-instance comparison possible at all.
    """
    return {iid: f"{iid}/" for iid in instance_ids}
