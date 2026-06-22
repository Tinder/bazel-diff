package main

import (
	"os/exec"
	"strings"
)

// readBazelLabel returns the bazel "Build label" version string for bookkeeping
// in snapshot metadata. Best-effort: returns "" on any error.
func readBazelLabel(bazel, workspace string) (string, error) {
	cmd := exec.Command(bazel, "version")
	cmd.Dir = workspace
	out, err := cmd.Output()
	if err != nil {
		return "", err
	}
	for _, line := range strings.Split(string(out), "\n") {
		if strings.HasPrefix(line, "Build label: ") {
			return strings.TrimSpace(strings.TrimPrefix(line, "Build label: ")), nil
		}
	}
	return "", nil
}
