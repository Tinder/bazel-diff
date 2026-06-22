package main

import (
	"os"
	"path/filepath"
	"testing"
)

// writeFakeBazel writes an executable shell stub at dir/bazel that prints the
// given lines for `bazel version` and returns its path.
func writeFakeBazel(t *testing.T, dir, versionOutput string) string {
	t.Helper()
	p := filepath.Join(dir, "bazel")
	body := "#!/bin/sh\nif [ \"$1\" = version ]; then cat <<'EOF'\n" + versionOutput + "\nEOF\nfi\n"
	if err := os.WriteFile(p, []byte(body), 0o755); err != nil {
		t.Fatal(err)
	}
	return p
}

func TestReadBazelLabel(t *testing.T) {
	dir := t.TempDir()
	bazel := writeFakeBazel(t, dir, "Build label: 8.5.1\nBuild time: ...")
	got, err := readBazelLabel(bazel, dir)
	if err != nil {
		t.Fatal(err)
	}
	if got != "8.5.1" {
		t.Fatalf("want 8.5.1, got %q", got)
	}
}

func TestReadBazelLabelNoLabel(t *testing.T) {
	dir := t.TempDir()
	bazel := writeFakeBazel(t, dir, "no version line here")
	got, err := readBazelLabel(bazel, dir)
	if err != nil {
		t.Fatal(err)
	}
	if got != "" {
		t.Fatalf("want empty for missing label, got %q", got)
	}
}

func TestReadBazelLabelExecError(t *testing.T) {
	if _, err := readBazelLabel("/no/such/bazel/binary", t.TempDir()); err == nil {
		t.Fatal("expected error when bazel binary is missing")
	}
}
