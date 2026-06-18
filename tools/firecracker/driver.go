package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// recordRequest is everything a driver needs to produce a snapshot for a base SHA.
type recordRequest struct {
	Workspace string
	BaseSHA   string
	Bazel     string
	BazelDiff string
	Flags     []string
	Entry     entry
}

// consumeRequest is everything a driver needs to compute impacted targets for a
// target SHA against a recorded snapshot.
type consumeRequest struct {
	Workspace string
	TargetSHA string
	Bazel     string
	BazelDiff string
	Flags     []string
	Entry     entry
	Out       string
}

// driver abstracts *where* the warm Bazel server lives. The firecracker driver
// runs warmup/consume inside a microVM and snapshots it; the local driver runs
// them directly on the host (no snapshotting — useful for testing the
// orchestration and as a portable cold/warm proxy).
type driver interface {
	name() string
	record(recordRequest) error
	consume(consumeRequest) error
}

// localDriver runs everything on the host. It does NOT snapshot — record simply
// runs `bazel-diff warmup` to bake base_hashes.json + fingerprint.json into the
// store entry, and consume runs generate-hashes + get-impacted-targets directly.
// This is the path that runs anywhere (incl. macOS) and underpins the unit
// tests and the cold/warm proxy benchmark.
type localDriver struct {
	// runner lets tests stub command execution. nil => real exec.
	runner func(name string, args ...string) error
}

func (localDriver) name() string { return "local" }

func (d localDriver) exec(name string, args ...string) error {
	if d.runner != nil {
		return d.runner(name, args...)
	}
	cmd := exec.Command(name, args...)
	cmd.Stdout = os.Stderr // diagnostics go to stderr; stdout stays clean
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%s %s: %w", name, strings.Join(args, " "), err)
	}
	return nil
}

func (d localDriver) record(r recordRequest) error {
	git := gitClient{workspace: r.Workspace}
	if err := git.checkout(r.BaseSHA); err != nil {
		return err
	}
	args := []string{
		"warmup",
		"-w", r.Workspace,
		"-b", r.Bazel,
		"--base-hashes", r.Entry.baseHashes(),
		"--fingerprint-output", filepath.Join(r.Entry.Dir, fingerprintName),
	}
	args = append(args, r.Flags...)
	return d.exec(r.BazelDiff, args...)
}

func (d localDriver) consume(r consumeRequest) error {
	git := gitClient{workspace: r.Workspace}
	if err := git.checkout(r.TargetSHA); err != nil {
		return err
	}
	targetHashes := filepath.Join(filepath.Dir(r.Out), "target_hashes.json")
	genArgs := []string{"generate-hashes", "-w", r.Workspace, "-b", r.Bazel, targetHashes}
	genArgs = append(genArgs, r.Flags...)
	if err := d.exec(r.BazelDiff, genArgs...); err != nil {
		return err
	}
	return d.exec(r.BazelDiff,
		"get-impacted-targets",
		"-w", r.Workspace,
		"-b", r.Bazel,
		"-sh", r.Entry.baseHashes(),
		"-fh", targetHashes,
		"-o", r.Out,
	)
}
