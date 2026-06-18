//go:build fcintegration

// Real-microVM end-to-end canary for the firecracker driver (RFC §5.3).
//
// Unlike the rest of the suite (pure logic + a fake API socket), this test boots
// an actual Firecracker microVM, so it is gated behind the `fcintegration` build
// tag and a set of env vars. It runs on a Linux + /dev/kvm host with a guest
// image built by bench/build_guest_image.sh and a TAP from bench/setup_tap.sh:
//
//	FC_BIN        path to the firecracker binary
//	FC_KERNEL     guest kernel (rootfs.base.ext4 must sit beside it)
//	FC_STORE      empty dir for the snapshot store
//	FC_GUEST_ADDR root@172.16.0.2
//	FC_GUEST_KEY  ssh identity trusted by the guest
//	FC_TAP        host TAP name (e.g. fc-tap0)
//	FC_WORKSPACE  in-guest workspace path (e.g. /work)
//	FC_BASE_SHA / FC_TARGET_SHA   revisions baked into the guest repo
//
// It exercises the exact code the unit tests cannot: fcDriver.record (boot →
// NIC over TAP → warmup → pause → snapshot) and fcDriver.consume (restore →
// resume → git checkout → generate-hashes → get-impacted-targets → scp out),
// then asserts a non-empty impacted set. Build/run:
//
//	go test -tags fcintegration -run TestFirecrackerRecordConsume -v ./...
package main

import (
	"os"
	"path/filepath"
	"strconv"
	"testing"
)

func envOrSkip(t *testing.T, key string) string {
	t.Helper()
	v := os.Getenv(key)
	if v == "" {
		t.Skipf("set %s to run the firecracker integration canary", key)
	}
	return v
}

func TestFirecrackerRecordConsume(t *testing.T) {
	fcBin := envOrSkip(t, "FC_BIN")
	kernel := envOrSkip(t, "FC_KERNEL")
	store := envOrSkip(t, "FC_STORE")
	guestAddr := envOrSkip(t, "FC_GUEST_ADDR")
	guestKey := envOrSkip(t, "FC_GUEST_KEY")
	tap := envOrSkip(t, "FC_TAP")
	workspace := envOrSkip(t, "FC_WORKSPACE")
	baseSHA := envOrSkip(t, "FC_BASE_SHA")
	targetSHA := envOrSkip(t, "FC_TARGET_SHA")

	bazel := getenvDefault("FC_BAZEL", "bazel")
	bazelDiff := getenvDefault("FC_BAZEL_DIFF", "bazel-diff")

	d := fcDriver{
		firecrackerBin: fcBin,
		socketPath:     filepath.Join(t.TempDir(), "fc.sock"),
		kernelImage:    kernel,
		vcpus:          atoiDefault("FC_VCPUS", 2),
		memMib:         atoiDefault("FC_MEM_MIB", 2048),
		guestSnapDir:   "/snap",
		guest:          sshGuest{addr: guestAddr, identity: guestKey, sshOpts: noHostKeyChecking},
		net: netConfig{
			tapDevice: tap,
			guestIP:   "172.16.0.2",
			hostIP:    "172.16.0.1",
			netmask:   "255.255.255.252",
			guestMAC:  "06:00:AC:10:00:02",
		},
	}

	e, err := newEntry(store, "itfp", baseSHA)
	if err != nil {
		t.Fatal(err)
	}

	if err := d.record(recordRequest{
		Workspace: workspace, BaseSHA: baseSHA, Bazel: bazel, BazelDiff: bazelDiff, Entry: e,
	}); err != nil {
		t.Fatalf("record: %v", err)
	}
	if _, err := os.Stat(e.baseHashes()); err != nil {
		t.Fatalf("record did not produce base hashes: %v", err)
	}

	// FC_OUT lets the workflow capture the impacted set on a stable path so it can
	// diff it against the local driver's output (the RFC §5.3 correctness canary).
	out := getenvDefault("FC_OUT", filepath.Join(t.TempDir(), "impacted.txt"))
	if err := d.consume(consumeRequest{
		Workspace: workspace, TargetSHA: targetSHA, Bazel: bazel, BazelDiff: bazelDiff, Entry: e, Out: out,
	}); err != nil {
		t.Fatalf("consume: %v", err)
	}
	b, err := os.ReadFile(out)
	if err != nil || len(b) == 0 {
		t.Fatalf("consume produced no impacted targets (err=%v, len=%d)", err, len(b))
	}
	t.Logf("impacted targets:\n%s", b)
}

var noHostKeyChecking = []string{
	"-o", "StrictHostKeyChecking=no",
	"-o", "UserKnownHostsFile=/dev/null",
	"-o", "LogLevel=ERROR",
}

func getenvDefault(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func atoiDefault(key string, def int) int {
	if v := os.Getenv(key); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			return n
		}
	}
	return def
}
