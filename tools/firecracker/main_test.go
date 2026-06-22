package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// fakeBazelDiff is a stand-in `bazel-diff` for exercising the local driver end to
// end: it emits a fixed fingerprint and writes the output files each subcommand
// is asked to produce, so runRecord/runConsume run without a real Bazel.
const fakeBazelDiff = `#!/bin/sh
sub="$1"; shift
case "$sub" in
  fingerprint)
    echo '{"fingerprint":"FAKEFP","flags":{}}' ;;
  warmup)
    bh=""; fp=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --base-hashes) bh="$2"; shift 2 ;;
        --fingerprint-output) fp="$2"; shift 2 ;;
        *) shift ;;
      esac
    done
    [ -n "$bh" ] && echo '{}' > "$bh"
    [ -n "$fp" ] && echo '{"fingerprint":"FAKEFP"}' > "$fp" ;;
  generate-hashes)
    out=""
    while [ "$#" -gt 0 ]; do
      case "$1" in -w|-b) shift 2 ;; *) out="$1"; shift ;; esac
    done
    [ -n "$out" ] && echo '{"//x:y":"h"}' > "$out" ;;
  get-impacted-targets)
    out=""
    while [ "$#" -gt 0 ]; do
      case "$1" in -o) out="$2"; shift 2 ;; *) shift ;; esac
    done
    [ -n "$out" ] && printf '//x:y\n//a:b\n' > "$out" ;;
esac
`

func writeFakeBazelDiff(t *testing.T, dir string) string {
	t.Helper()
	p := filepath.Join(dir, "bazel-diff")
	if err := os.WriteFile(p, []byte(fakeBazelDiff), 0o755); err != nil {
		t.Fatal(err)
	}
	return p
}

func TestRunRecordThenConsumeLocalDriver(t *testing.T) {
	ws, shas := makeLinearRepo(t, 3) // base=shas[0], target=shas[2]
	dir := t.TempDir()
	bd := writeFakeBazelDiff(t, dir)
	bazel := writeFakeBazel(t, dir, "Build label: 8.5.1")
	store := filepath.Join(dir, "store")

	if err := runRecord([]string{
		"--workspace", ws, "--base-sha", shas[0], "--store", store,
		"--bazel", bazel, "--bazel-diff", bd,
	}); err != nil {
		t.Fatalf("runRecord: %v", err)
	}
	// record must have produced the store entry with base hashes + metadata.
	e := entry{Dir: entryDir(store, "FAKEFP", shas[0])}
	if _, err := os.Stat(e.baseHashes()); err != nil {
		t.Fatalf("record did not write base hashes: %v", err)
	}

	out := filepath.Join(dir, "impacted.txt")
	if code := runConsume([]string{
		"--workspace", ws, "--target-sha", shas[2], "--store", store,
		"--bazel", bazel, "--bazel-diff", bd, "--out", out,
	}); code != exitOK {
		t.Fatalf("runConsume code=%d, want %d", code, exitOK)
	}
	b, err := os.ReadFile(out)
	if err != nil || !strings.Contains(string(b), "//x:y") {
		t.Fatalf("consume did not write impacted targets: %q err=%v", string(b), err)
	}
}

func TestRunConsumeColdFallback(t *testing.T) {
	ws, shas := makeLinearRepo(t, 2)
	dir := t.TempDir()
	bd := writeFakeBazelDiff(t, dir)
	bazel := writeFakeBazel(t, dir, "Build label: 8.5.1")
	// Empty store => no compatible snapshot => exit 2 (cold fallback).
	code := runConsume([]string{
		"--workspace", ws, "--target-sha", shas[1], "--store", filepath.Join(dir, "store"),
		"--bazel", bazel, "--bazel-diff", bd, "--out", filepath.Join(dir, "o.txt"),
	})
	if code != exitColdFallback {
		t.Fatalf("expected cold-fallback exit %d, got %d", exitColdFallback, code)
	}
}

func TestRunRecordMissingArgs(t *testing.T) {
	if err := runRecord([]string{"--workspace", "/x"}); err == nil {
		t.Fatal("runRecord should require --store and --base-sha")
	}
}

func TestRunConsumeMissingArgs(t *testing.T) {
	if code := runConsume([]string{"--workspace", "/x"}); code != exitError {
		t.Fatalf("runConsume with missing args should be exitError, got %d", code)
	}
}

func TestMakeDriver(t *testing.T) {
	if d, err := (commonFlags{driver: "local"}).makeDriver(); err != nil || d.name() != "local" {
		t.Fatalf("local driver: d=%v err=%v", d, err)
	}
	if d, err := (commonFlags{driver: ""}).makeDriver(); err != nil || d.name() != "local" {
		t.Fatalf("empty driver should default to local: d=%v err=%v", d, err)
	}
	if _, err := (commonFlags{driver: "nope"}).makeDriver(); err == nil {
		t.Fatal("unknown driver should error")
	}
	// firecracker without the required host/config always errors (no kvm/kernel/tap).
	if _, err := (commonFlags{driver: "firecracker"}).makeDriver(); err == nil {
		t.Fatal("firecracker driver without config should error")
	}
}

func TestUsageDoesNotPanic(t *testing.T) {
	usage() // writes help to stderr; just exercise it
}

func TestMultiFlag(t *testing.T) {
	var m multiFlag
	if err := m.Set("a"); err != nil {
		t.Fatal(err)
	}
	_ = m.Set("b")
	if len(m) != 2 || m.String() != "a b" {
		t.Fatalf("multiFlag wrong: %v / %q", m, m.String())
	}
}
