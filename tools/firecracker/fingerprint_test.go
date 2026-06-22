package main

import (
	"os"
	"path/filepath"
	"testing"
)

func writeScript(t *testing.T, dir, body string) string {
	t.Helper()
	p := filepath.Join(dir, "bazel-diff")
	if err := os.WriteFile(p, []byte("#!/bin/sh\n"+body+"\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	return p
}

func TestComputeFingerprintNonZeroExit(t *testing.T) {
	dir := t.TempDir()
	bd := writeScript(t, dir, "echo boom >&2; exit 1")
	if _, err := computeFingerprint(bd, dir, "bazel", nil); err == nil {
		t.Fatal("expected error on non-zero exit")
	}
}

func TestComputeFingerprintBadJSON(t *testing.T) {
	dir := t.TempDir()
	bd := writeScript(t, dir, "echo 'not json'")
	if _, err := computeFingerprint(bd, dir, "bazel", nil); err == nil {
		t.Fatal("expected error on unparseable JSON")
	}
}

func TestComputeFingerprintEmptyFingerprint(t *testing.T) {
	dir := t.TempDir()
	bd := writeScript(t, dir, `echo '{"fingerprint":""}'`)
	if _, err := computeFingerprint(bd, dir, "bazel", nil); err == nil {
		t.Fatal("expected error when fingerprint is empty")
	}
}
