package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestLocalDriverRecordInvokesWarmup(t *testing.T) {
	ws, shas := makeLinearRepo(t, 2)
	store := t.TempDir()
	e, err := newEntry(store, "fp", shas[0])
	if err != nil {
		t.Fatal(err)
	}

	var calls [][]string
	d := localDriver{runner: func(name string, args ...string) error {
		calls = append(calls, append([]string{name}, args...))
		return nil
	}}
	err = d.record(recordRequest{
		Workspace: ws, BaseSHA: shas[0], Bazel: "bazel", BazelDiff: "bazel-diff",
		Flags: []string{"--useCquery"}, Entry: e,
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(calls) != 1 {
		t.Fatalf("want 1 warmup call, got %d: %v", len(calls), calls)
	}
	got := calls[0]
	assertContains(t, got, "warmup")
	assertContains(t, got, "--base-hashes")
	assertContains(t, got, e.baseHashes())
	assertContains(t, got, "--fingerprint-output")
	assertContains(t, got, "--useCquery")
}

func TestLocalDriverConsumeChain(t *testing.T) {
	ws, shas := makeLinearRepo(t, 2)
	store := t.TempDir()
	e, _ := newEntry(store, "fp", shas[0])
	out := filepath.Join(t.TempDir(), "impacted.txt")

	var calls [][]string
	d := localDriver{runner: func(name string, args ...string) error {
		calls = append(calls, append([]string{name}, args...))
		return nil
	}}
	err := d.consume(consumeRequest{
		Workspace: ws, TargetSHA: shas[1], Bazel: "bazel", BazelDiff: "bazel-diff",
		Entry: e, Out: out,
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(calls) != 2 {
		t.Fatalf("want generate-hashes + get-impacted-targets (2 calls), got %d: %v", len(calls), calls)
	}
	assertContains(t, calls[0], "generate-hashes")
	assertContains(t, calls[1], "get-impacted-targets")
	assertContains(t, calls[1], "-sh")
	assertContains(t, calls[1], e.baseHashes())
	assertContains(t, calls[1], out)
}

func TestComputeFingerprintParsesOutput(t *testing.T) {
	dir := t.TempDir()
	script := filepath.Join(dir, "fake-bazel-diff")
	// A fake bazel-diff that prints a fingerprint JSON regardless of args.
	body := "#!/bin/sh\ncat <<'EOF'\n{\"fingerprint\":\"abc123\",\"flags\":{\"useCquery\":\"false\"}}\nEOF\n"
	if err := os.WriteFile(script, []byte(body), 0o755); err != nil {
		t.Fatal(err)
	}
	fp, err := computeFingerprint(script, dir, "bazel", nil)
	if err != nil {
		t.Fatal(err)
	}
	if fp != "abc123" {
		t.Fatalf("want abc123, got %q", fp)
	}
}

func assertContains(t *testing.T, hay []string, needle string) {
	t.Helper()
	for _, s := range hay {
		if s == needle {
			return
		}
	}
	t.Fatalf("expected %q in %v", needle, hay)
}
