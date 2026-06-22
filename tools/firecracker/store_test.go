package main

import (
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"
)

// makeLinearRepo creates a git repo with n commits and returns workspace + SHAs
// in chronological order (shas[0] is oldest).
func makeLinearRepo(t *testing.T, n int) (string, []string) {
	t.Helper()
	ws := t.TempDir()
	run := func(args ...string) string {
		cmd := exec.Command("git", append([]string{"-C", ws}, args...)...)
		out, err := cmd.CombinedOutput()
		if err != nil {
			t.Fatalf("git %v: %v\n%s", args, err, out)
		}
		return string(out)
	}
	run("init", "-q")
	run("config", "user.email", "t@t.local")
	run("config", "user.name", "t")
	var shas []string
	for i := 0; i < n; i++ {
		f := filepath.Join(ws, "f.txt")
		if err := os.WriteFile(f, []byte{byte('0' + i)}, 0o644); err != nil {
			t.Fatal(err)
		}
		run("add", "-A")
		run("commit", "-q", "-m", "c")
		sha := run("rev-parse", "HEAD")
		shas = append(shas, trim(sha))
	}
	return ws, shas
}

func trim(s string) string {
	for len(s) > 0 && (s[len(s)-1] == '\n' || s[len(s)-1] == ' ' || s[len(s)-1] == '\r') {
		s = s[:len(s)-1]
	}
	return s
}

// writeSnapshot creates a complete store entry (with base_hashes.json marker).
func writeSnapshot(t *testing.T, store, fp, baseSHA string) {
	t.Helper()
	dir := entryDir(store, fp, baseSHA)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, baseHashesName), []byte("{}"), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestNewEntryAndPaths(t *testing.T) {
	store := t.TempDir()
	e, err := newEntry(store, "fp1", "sha1")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(e.Dir); err != nil {
		t.Fatalf("newEntry should create the dir: %v", err)
	}
	checks := map[string]string{
		e.memFile():    memFileName,
		e.vmstate():    vmstateName,
		e.rootfs():     rootfsName,
		e.baseHashes(): baseHashesName,
		e.metadata():   metadataName,
	}
	for got, suffix := range checks {
		if filepath.Base(got) != suffix {
			t.Fatalf("path %q should end in %q", got, suffix)
		}
		if filepath.Dir(got) != e.Dir {
			t.Fatalf("path %q should live under the entry dir %q", got, e.Dir)
		}
	}
}

func TestWriteMetadata(t *testing.T) {
	store := t.TempDir()
	e, _ := newEntry(store, "fp1", "sha1")
	now := time.Unix(1700000000, 0)
	if err := writeMetadata(e, metadata{Fingerprint: "fp1", BazelVersion: "8.5.1"}, now); err != nil {
		t.Fatal(err)
	}
	b, err := os.ReadFile(e.metadata())
	if err != nil {
		t.Fatal(err)
	}
	var m metadata
	if err := json.Unmarshal(b, &m); err != nil {
		t.Fatal(err)
	}
	if m.Fingerprint != "fp1" || m.BaseSHA != "sha1" || m.BazelVersion != "8.5.1" {
		t.Fatalf("metadata fields wrong: %+v", m)
	}
	if m.CreatedAtUnix != 1700000000 || m.CreatedAtString == "" {
		t.Fatalf("metadata timestamps wrong: %+v", m)
	}
}

func TestMustAbs(t *testing.T) {
	abs, err := mustAbs("relative/path")
	if err != nil {
		t.Fatal(err)
	}
	if !filepath.IsAbs(abs) {
		t.Fatalf("mustAbs should return an absolute path, got %q", abs)
	}
	already := "/already/abs"
	if got, _ := mustAbs(already); got != already {
		t.Fatalf("mustAbs of an absolute path should be unchanged, got %q", got)
	}
}

func TestResolveNearestAncestor(t *testing.T) {
	ws, shas := makeLinearRepo(t, 5) // shas[0..4], 4 is newest
	store := t.TempDir()
	fp := "fp1"
	git := gitClient{workspace: ws}

	// Snapshots at commit 0 and commit 2; target is commit 4.
	writeSnapshot(t, store, fp, shas[0])
	writeSnapshot(t, store, fp, shas[2])

	e, err := resolve(store, fp, shas[4], git)
	if err != nil {
		t.Fatal(err)
	}
	if e == nil {
		t.Fatal("expected a snapshot, got nil")
	}
	if e.BaseSHA != shas[2] {
		t.Fatalf("want nearest ancestor %s, got %s", shas[2], e.BaseSHA)
	}
}

func TestResolveNoCompatibleFingerprint(t *testing.T) {
	ws, shas := makeLinearRepo(t, 3)
	store := t.TempDir()
	git := gitClient{workspace: ws}
	writeSnapshot(t, store, "fpA", shas[0])

	// Different fingerprint => no candidates => cold fallback (nil, nil).
	e, err := resolve(store, "fpB", shas[2], git)
	if err != nil {
		t.Fatal(err)
	}
	if e != nil {
		t.Fatalf("expected nil (cold fallback), got %+v", e)
	}
}

func TestResolveIgnoresNonAncestor(t *testing.T) {
	ws, shas := makeLinearRepo(t, 4)
	store := t.TempDir()
	fp := "fp1"
	git := gitClient{workspace: ws}

	// Snapshot at the NEWEST commit; target is an OLDER commit -> not an ancestor.
	writeSnapshot(t, store, fp, shas[3])
	e, err := resolve(store, fp, shas[1], git)
	if err != nil {
		t.Fatal(err)
	}
	if e != nil {
		t.Fatalf("expected nil (newer snapshot is not an ancestor of older target), got %+v", e)
	}
}

func TestListBaseSHAsIgnoresIncomplete(t *testing.T) {
	store := t.TempDir()
	fp := "fp1"
	// complete
	writeSnapshot(t, store, fp, "complete")
	// incomplete: dir exists but no base_hashes.json
	if err := os.MkdirAll(entryDir(store, fp, "incomplete"), 0o755); err != nil {
		t.Fatal(err)
	}
	got, err := listBaseSHAs(store, fp)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 1 || got[0] != "complete" {
		t.Fatalf("want [complete], got %v", got)
	}
}
