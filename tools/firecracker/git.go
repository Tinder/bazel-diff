package main

import (
	"fmt"
	"os/exec"
	"strconv"
	"strings"
)

// gitClient runs git against a fixed workspace directory.
type gitClient struct {
	workspace string
}

func (g gitClient) run(args ...string) (string, error) {
	cmd := exec.Command("git", append([]string{"-C", g.workspace}, args...)...)
	out, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("git %s: %w", strings.Join(args, " "), err)
	}
	return strings.TrimSpace(string(out)), nil
}

// isAncestor reports whether ancestor is an ancestor of (or equal to) descendant.
func (g gitClient) isAncestor(ancestor, descendant string) (bool, error) {
	cmd := exec.Command("git", "-C", g.workspace,
		"merge-base", "--is-ancestor", ancestor, descendant)
	err := cmd.Run()
	if err == nil {
		return true, nil
	}
	// Exit code 1 == "not an ancestor"; anything else is a real error.
	if exit, ok := err.(*exec.ExitError); ok && exit.ExitCode() == 1 {
		return false, nil
	}
	return false, fmt.Errorf("git merge-base --is-ancestor %s %s: %w", ancestor, descendant, err)
}

// distance returns the number of commits in from..to (commits reachable from
// `to` but not from `from`). Smaller means a nearer ancestor.
func (g gitClient) distance(from, to string) (int, error) {
	out, err := g.run("rev-list", "--count", from+".."+to)
	if err != nil {
		return 0, err
	}
	n, err := strconv.Atoi(out)
	if err != nil {
		return 0, fmt.Errorf("parsing rev-list count %q: %w", out, err)
	}
	return n, nil
}

// checkout checks out the given revision, discarding local changes.
func (g gitClient) checkout(sha string) error {
	_, err := g.run("checkout", "--force", "--quiet", sha)
	return err
}
