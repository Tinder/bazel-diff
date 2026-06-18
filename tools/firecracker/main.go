// Command bazel-diff-snap orchestrates Firecracker microVM snapshots to give
// instant starts of bazel-diff on large monorepos. See docs/firecracker-snapshots.md.
//
//	bazel-diff-snap record  --workspace <p> --base-sha <sha>   --store <dir> [opts]
//	bazel-diff-snap consume --workspace <p> --target-sha <sha> --store <dir> --out <f> [opts]
//
// `record` warms a Bazel server (via `bazel-diff warmup`) and snapshots it,
// keyed by fingerprint + base SHA. `consume` resolves a compatible snapshot
// (fail-safe: cold fallback on fingerprint mismatch or no ancestor), restores
// it, and computes impacted targets against the baked-in base hashes.
package main

import (
	"flag"
	"fmt"
	"os"
	"runtime"
	"strings"
	"time"
)

// exit codes
const (
	exitOK           = 0
	exitError        = 1
	exitColdFallback = 2 // consume found no compatible snapshot; caller runs cold path
)

func main() {
	if len(os.Args) < 2 {
		usage()
		os.Exit(exitError)
	}
	var err error
	switch os.Args[1] {
	case "record":
		err = runRecord(os.Args[2:])
	case "consume":
		os.Exit(runConsume(os.Args[2:]))
	case "-h", "--help", "help":
		usage()
		return
	default:
		fmt.Fprintf(os.Stderr, "unknown command %q\n\n", os.Args[1])
		usage()
		os.Exit(exitError)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(exitError)
	}
}

func usage() {
	fmt.Fprint(os.Stderr, `bazel-diff-snap — Firecracker snapshot orchestration for bazel-diff

usage:
  bazel-diff-snap record  --workspace <p> --base-sha <sha>   --store <dir> [opts]
  bazel-diff-snap consume --workspace <p> --target-sha <sha> --store <dir> --out <f> [opts]

common opts:
  --bazel <path>        bazel binary (default "bazel")
  --bazel-diff <path>   bazel-diff binary (default "bazel-diff")
  --driver local|firecracker   (default "local")
  --flag <f>            extra bazel-diff flag, repeatable (must match record/consume)

consume exit codes: 0 ok, 1 error, 2 no compatible snapshot (run cold path)
`)
}

// commonFlags are shared by record and consume.
type commonFlags struct {
	workspace string
	store     string
	bazel     string
	bazelDiff string
	driver    string
	flags     multiFlag
	// firecracker driver config
	firecrackerBin string
	socket         string
	kernel         string
	vcpus          int
	memMib         int
	guestSnapDir   string
	guestAddr      string
	guestKey       string
}

func registerCommon(fs *flag.FlagSet, c *commonFlags) {
	fs.StringVar(&c.workspace, "workspace", "", "path to the Bazel workspace (git repo)")
	fs.StringVar(&c.store, "store", "", "snapshot store directory")
	fs.StringVar(&c.bazel, "bazel", "bazel", "bazel binary")
	fs.StringVar(&c.bazelDiff, "bazel-diff", "bazel-diff", "bazel-diff binary")
	fs.StringVar(&c.driver, "driver", "local", "driver: local | firecracker")
	fs.Var(&c.flags, "flag", "extra bazel-diff flag (repeatable)")
	fs.StringVar(&c.firecrackerBin, "firecracker-bin", "firecracker", "firecracker binary")
	fs.StringVar(&c.socket, "socket", "/tmp/bazel-diff-fc.sock", "firecracker API socket")
	fs.StringVar(&c.kernel, "kernel", "", "guest kernel image (firecracker driver)")
	fs.IntVar(&c.vcpus, "vcpus", 4, "guest vCPUs (firecracker driver)")
	fs.IntVar(&c.memMib, "mem-mib", 8192, "guest memory MiB (firecracker driver)")
	fs.StringVar(&c.guestSnapDir, "guest-snap-dir", "/snap", "in-guest snapshot dir")
	fs.StringVar(&c.guestAddr, "guest-addr", "", "guest ssh address user@host (firecracker driver)")
	fs.StringVar(&c.guestKey, "guest-key", "", "guest ssh identity file (firecracker driver)")
}

func (c commonFlags) makeDriver() (driver, error) {
	switch c.driver {
	case "local", "":
		return localDriver{}, nil
	case "firecracker":
		if runtime.GOOS != "linux" {
			return nil, fmt.Errorf("firecracker driver requires Linux (host is %s)", runtime.GOOS)
		}
		if _, err := os.Stat("/dev/kvm"); err != nil {
			return nil, fmt.Errorf("firecracker driver requires /dev/kvm: %w", err)
		}
		if c.kernel == "" || c.guestAddr == "" {
			return nil, fmt.Errorf("firecracker driver requires --kernel and --guest-addr")
		}
		return fcDriver{
			firecrackerBin: c.firecrackerBin,
			socketPath:     c.socket,
			kernelImage:    c.kernel,
			vcpus:          c.vcpus,
			memMib:         c.memMib,
			guestSnapDir:   c.guestSnapDir,
			guest:          sshGuest{addr: c.guestAddr, identity: c.guestKey},
		}, nil
	default:
		return nil, fmt.Errorf("unknown driver %q", c.driver)
	}
}

func runRecord(args []string) error {
	fs := flag.NewFlagSet("record", flag.ExitOnError)
	var c commonFlags
	var baseSHA string
	registerCommon(fs, &c)
	fs.StringVar(&baseSHA, "base-sha", "", "base revision to snapshot")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if c.workspace == "" || c.store == "" || baseSHA == "" {
		return fmt.Errorf("record requires --workspace, --store, --base-sha")
	}
	store, err := mustAbs(c.store)
	if err != nil {
		return err
	}
	d, err := c.makeDriver()
	if err != nil {
		return err
	}
	git := gitClient{workspace: c.workspace}

	// Fingerprint is computed against the base revision's environment.
	if err := git.checkout(baseSHA); err != nil {
		return err
	}
	fp, err := computeFingerprint(c.bazelDiff, c.workspace, c.bazel, c.flags)
	if err != nil {
		return err
	}
	fmt.Fprintf(os.Stderr, "record: fingerprint=%s base=%s driver=%s\n", fp, baseSHA, d.name())

	e, err := newEntry(store, fp, baseSHA)
	if err != nil {
		return err
	}
	if err := d.record(recordRequest{
		Workspace: c.workspace, BaseSHA: baseSHA, Bazel: c.bazel,
		BazelDiff: c.bazelDiff, Flags: c.flags, Entry: e,
	}); err != nil {
		return err
	}
	bazelVer, _ := readBazelLabel(c.bazel, c.workspace)
	if err := writeMetadata(e, metadata{
		Fingerprint: fp, BazelVersion: bazelVer,
	}, time.Now()); err != nil {
		return err
	}
	fmt.Fprintf(os.Stderr, "record: wrote snapshot to %s\n", e.Dir)
	return nil
}

func runConsume(args []string) int {
	fs := flag.NewFlagSet("consume", flag.ExitOnError)
	var c commonFlags
	var targetSHA, out string
	registerCommon(fs, &c)
	fs.StringVar(&targetSHA, "target-sha", "", "target revision to analyse")
	fs.StringVar(&out, "out", "", "path to write impacted targets")
	if err := fs.Parse(args); err != nil {
		return exitError
	}
	if c.workspace == "" || c.store == "" || targetSHA == "" || out == "" {
		fmt.Fprintln(os.Stderr, "consume requires --workspace, --store, --target-sha, --out")
		return exitError
	}
	store, err := mustAbs(c.store)
	if err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		return exitError
	}
	d, err := c.makeDriver()
	if err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		return exitError
	}
	git := gitClient{workspace: c.workspace}

	// Fingerprint the *target* environment, then resolve a compatible snapshot.
	if err := git.checkout(targetSHA); err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		return exitError
	}
	fp, err := computeFingerprint(c.bazelDiff, c.workspace, c.bazel, c.flags)
	if err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		return exitError
	}
	e, err := resolve(store, fp, targetSHA, git)
	if err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		return exitError
	}
	if e == nil {
		// Fail-safe: no compatible snapshot. Caller runs the cold path.
		fmt.Fprintf(os.Stderr, "consume: no compatible snapshot for fingerprint=%s target=%s — cold fallback\n", fp, targetSHA)
		return exitColdFallback
	}
	fmt.Fprintf(os.Stderr, "consume: using snapshot base=%s (fingerprint=%s) driver=%s\n", e.BaseSHA, fp, d.name())

	if err := d.consume(consumeRequest{
		Workspace: c.workspace, TargetSHA: targetSHA, Bazel: c.bazel,
		BazelDiff: c.bazelDiff, Flags: c.flags, Entry: *e, Out: out,
	}); err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		return exitError
	}
	fmt.Fprintf(os.Stderr, "consume: wrote impacted targets to %s\n", out)
	return exitOK
}

// multiFlag collects a repeatable string flag.
type multiFlag []string

func (m *multiFlag) String() string     { return strings.Join(*m, " ") }
func (m *multiFlag) Set(v string) error { *m = append(*m, v); return nil }
