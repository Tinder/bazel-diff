package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os/exec"
)

// fingerprintJSON is the subset of `bazel-diff fingerprint` output we consume.
type fingerprintJSON struct {
	Fingerprint string            `json:"fingerprint"`
	Flags       map[string]string `json:"flags"`
}

// computeFingerprint runs `bazel-diff fingerprint` for the given workspace and
// flag set and returns the cache key. The flag set passed here MUST match the
// flags used for generate-hashes/warmup, or the key will (correctly) differ.
func computeFingerprint(bazelDiff, workspace, bazel string, flags []string) (string, error) {
	// No -o: the fingerprint command writes JSON to stdout by default.
	args := []string{"fingerprint", "-w", workspace, "-b", bazel}
	args = append(args, flags...)
	cmd := exec.Command(bazelDiff, args...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return "", fmt.Errorf("bazel-diff fingerprint failed: %w\n%s", err, stderr.String())
	}
	var parsed fingerprintJSON
	if err := json.Unmarshal(stdout.Bytes(), &parsed); err != nil {
		return "", fmt.Errorf("parsing fingerprint JSON: %w", err)
	}
	if parsed.Fingerprint == "" {
		return "", fmt.Errorf("fingerprint output had empty fingerprint: %s", stdout.String())
	}
	return parsed.Fingerprint, nil
}
