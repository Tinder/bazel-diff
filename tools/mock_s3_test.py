#!/usr/bin/env python3
"""Unit tests for tools/mock_s3.py.

`mock_s3` is pure stdlib and never shells out to bazel or git, so unlike the serve harnesses it is
safe to exercise fully -- including its HTTP surface -- from a `py_test`. That matters because the
mock is load-bearing: if its `aws-chunked` decoder silently mis-parses a PutObject, the consistency
harness records corrupt payloads and reports divergence that is not real (or, worse, records
identical corruption and reports agreement that is not real).
"""

import json
import unittest
import urllib.error
import urllib.request

import mock_s3


def _chunk(payload: bytes, *, signature: str | None = None) -> bytes:
    header = f"{len(payload):x}"
    if signature:
        header += f";chunk-signature={signature}"
    return header.encode() + b"\r\n" + payload + b"\r\n"


def _terminator(*, signature: str | None = None, trailers=()) -> bytes:
    header = "0"
    if signature:
        header += f";chunk-signature={signature}"
    out = header.encode() + b"\r\n"
    for name, value in trailers:
        out += f"{name}:{value}\r\n".encode()
    out += b"\r\n"
    return out


class DecodeAwsChunkedTest(unittest.TestCase):
    def test_unsigned_single_chunk(self):
        framed = _chunk(b'{"a":1}') + _terminator()
        payload, trailers = mock_s3.decode_aws_chunked(framed)
        self.assertEqual(payload, b'{"a":1}')
        self.assertEqual(trailers, {})

    def test_signed_chunks_are_accepted(self):
        sig = "a" * 64
        framed = _chunk(b"hello", signature=sig) + _terminator(signature=sig)
        payload, _ = mock_s3.decode_aws_chunked(framed)
        self.assertEqual(payload, b"hello")

    def test_multiple_chunks_are_concatenated(self):
        framed = _chunk(b"abc") + _chunk(b"defg") + _terminator()
        payload, _ = mock_s3.decode_aws_chunked(framed)
        self.assertEqual(payload, b"abcdefg")

    def test_trailers_are_captured(self):
        framed = _chunk(b"x") + _terminator(trailers=[("x-amz-checksum-crc32", "Ag8Zpw==")])
        payload, trailers = mock_s3.decode_aws_chunked(framed)
        self.assertEqual(payload, b"x")
        self.assertEqual(trailers, {"x-amz-checksum-crc32": "Ag8Zpw=="})

    def test_empty_payload(self):
        payload, _ = mock_s3.decode_aws_chunked(_terminator())
        self.assertEqual(payload, b"")

    def test_declared_length_mismatch_raises(self):
        framed = _chunk(b"abc") + _terminator()
        with self.assertRaises(mock_s3.ChunkDecodeError):
            mock_s3.decode_aws_chunked(framed, expected_len=99)

    def test_declared_length_match_passes(self):
        framed = _chunk(b"abc") + _terminator()
        payload, _ = mock_s3.decode_aws_chunked(framed, expected_len=3)
        self.assertEqual(payload, b"abc")

    def test_truncated_chunk_raises(self):
        with self.assertRaises(mock_s3.ChunkDecodeError):
            mock_s3.decode_aws_chunked(b"10\r\nshort\r\n")

    def test_garbage_size_raises(self):
        with self.assertRaises(mock_s3.ChunkDecodeError):
            mock_s3.decode_aws_chunked(b"zz\r\ndata\r\n")


class ParseObjectPathTest(unittest.TestCase):
    def test_simple(self):
        self.assertEqual(mock_s3.parse_object_path("/bucket/key.json"), ("bucket", "key.json"))

    def test_nested_key_keeps_slashes(self):
        self.assertEqual(
            mock_s3.parse_object_path("/bucket/i0/abc.def.json"), ("bucket", "i0/abc.def.json")
        )

    def test_query_string_is_dropped(self):
        self.assertEqual(
            mock_s3.parse_object_path("/bucket/k.json?x-id=GetObject"), ("bucket", "k.json")
        )

    def test_percent_decoding(self):
        self.assertEqual(mock_s3.parse_object_path("/bucket/a%20b.json"), ("bucket", "a b.json"))

    def test_non_object_paths(self):
        self.assertIsNone(mock_s3.parse_object_path("/"))
        self.assertIsNone(mock_s3.parse_object_path("/bucket"))
        self.assertIsNone(mock_s3.parse_object_path("/bucket/"))
        self.assertIsNone(mock_s3.parse_object_path("relative/key"))


class SplitPrefixTest(unittest.TestCase):
    def test_longest_prefix_wins(self):
        self.assertEqual(mock_s3.split_prefix("i11/k.json", ["i1/", "i11/"]), ("i11/", "k.json"))

    def test_unknown_prefix(self):
        self.assertEqual(mock_s3.split_prefix("zz/k.json", ["i0/"]), (None, "zz/k.json"))


class ErrorXmlTest(unittest.TestCase):
    def test_code_is_present(self):
        body = mock_s3.error_xml("NoSuchKey", "The specified key does not exist.", "i0/k.json")
        self.assertIn(b"<Code>NoSuchKey</Code>", body)
        self.assertIn(b"i0/k.json", body)


# ----------------------------------------------------------------------------------------------
# The live HTTP surface
# ----------------------------------------------------------------------------------------------


def _request(url, method, data=None, headers=None):
    req = urllib.request.Request(url, method=method, data=data, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


class ServerTest(unittest.TestCase):
    BUCKET = "test-bucket"

    def setUp(self):
        self.prefixes = mock_s3.instance_prefixes(["i0", "i1"])
        self.ctx = mock_s3.mock_s3(self.BUCKET, self.prefixes)
        self.store, self.server = self.ctx.__enter__()
        self.addCleanup(self.ctx.__exit__, None, None, None)

    def url(self, key):
        return f"{self.server.url}/{self.BUCKET}/{key}"

    def test_put_then_get_roundtrip(self):
        body = json.dumps({"//:core": "Rule#abc~def"}).encode()
        code, _ = _request(self.url("i0/sha.fp.json"), "PUT", body)
        self.assertEqual(code, 200)
        code, got = _request(self.url("i0/sha.fp.json"), "GET")
        self.assertEqual(code, 200)
        self.assertEqual(got, body)

    def test_chunked_put_is_decoded(self):
        payload = b'{"//:core":"Rule#a~b"}'
        framed = _chunk(payload) + _terminator(trailers=[("x-amz-checksum-crc32", "AAAAAA==")])
        code, _ = _request(
            self.url("i0/k.json"),
            "PUT",
            framed,
            headers={
                "Content-Encoding": "aws-chunked",
                "x-amz-decoded-content-length": str(len(payload)),
            },
        )
        self.assertEqual(code, 200)
        writes = self.store.writes(instance="i0")
        self.assertEqual(len(writes), 1)
        self.assertEqual(writes[0].body, payload)
        self.assertTrue(writes[0].chunked)

    def test_undecodable_chunked_put_is_a_400_and_recorded(self):
        code, _ = _request(
            self.url("i0/k.json"),
            "PUT",
            b"not chunked at all",
            headers={"Content-Encoding": "aws-chunked", "x-amz-decoded-content-length": "5"},
        )
        # Deliberately a 4xx: a 5xx would be retried by the SDK and look like a cache miss.
        self.assertEqual(code, 400)
        self.assertTrue(self.store.bad_requests())

    def test_get_miss_is_no_such_key(self):
        code, body = _request(self.url("i0/absent.json"), "GET")
        self.assertEqual(code, 404)
        self.assertIn(b"<Code>NoSuchKey</Code>", body)

    def test_head_hit_and_miss(self):
        _request(self.url("i0/k.json"), "PUT", b"12345")
        code, body = _request(self.url("i0/k.json"), "HEAD")
        self.assertEqual(code, 200)
        self.assertEqual(body, b"")
        code, body = _request(self.url("i0/absent.json"), "HEAD")
        self.assertEqual(code, 404)
        self.assertEqual(body, b"")

    def test_isolated_mode_hides_other_instances_writes(self):
        _request(self.url("i0/k.json"), "PUT", b"payload")
        code, _ = _request(self.url("i1/k.json"), "GET")
        self.assertEqual(code, 404)
        reads = self.store.reads(instance="i1")
        self.assertEqual(len(reads), 1)
        self.assertFalse(reads[0].hit)

    def test_shared_mode_exposes_and_attributes_the_write(self):
        _request(self.url("i0/k.json"), "PUT", b"payload")
        self.store.set_mode(mock_s3.SHARED)
        code, got = _request(self.url("i1/k.json"), "GET")
        self.assertEqual(code, 200)
        self.assertEqual(got, b"payload")
        read = self.store.reads(instance="i1")[-1]
        self.assertTrue(read.hit)
        self.assertEqual(read.served_from_prefix, "i0/")

    def test_bodies_for_groups_writes_by_instance(self):
        _request(self.url("i0/k.json"), "PUT", b"from-i0")
        _request(self.url("i1/k.json"), "PUT", b"from-i1")
        self.assertEqual(self.store.logical_keys(), ["k.json"])
        self.assertEqual(
            self.store.bodies_for("k.json"), {"i0": b"from-i0", "i1": b"from-i1"}
        )

    def test_unknown_bucket_is_recorded_as_bad(self):
        code, _ = _request(f"{self.server.url}/other-bucket/k.json", "GET")
        self.assertEqual(code, 400)
        self.assertTrue(self.store.bad_requests())

    def test_counts_and_reset(self):
        _request(self.url("i0/k.json"), "PUT", b"x")
        _request(self.url("i0/k.json"), "GET")
        counts = self.store.counts()
        self.assertEqual(counts["puts"], 1)
        self.assertEqual(counts["gets"], 1)
        self.assertEqual(self.store.counts_by_instance()["i0"]["hits"], 1)
        self.store.reset()
        self.assertEqual(self.store.counts()["puts"], 0)
        self.assertEqual(self.store.logical_keys(), [])

    def test_reset_retires_counts_into_the_run_total(self):
        # The harness resets between cases; without this the reported traffic would describe only
        # whichever case happened to run last.
        _request(self.url("i0/k.json"), "PUT", b"x")
        _request(self.url("i0/k.json"), "GET")
        self.store.reset()
        _request(self.url("i1/k.json"), "PUT", b"y")
        self.assertEqual(self.store.counts()["puts"], 1)
        total = self.store.total_counts()
        self.assertEqual(total["puts"], 2)
        self.assertEqual(total["gets"], 1)


class InstancePrefixesTest(unittest.TestCase):
    def test_layout(self):
        self.assertEqual(mock_s3.instance_prefixes(["i0", "i1"]), {"i0": "i0/", "i1": "i1/"})


if __name__ == "__main__":
    unittest.main()
