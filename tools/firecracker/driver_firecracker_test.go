package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestNetConfigBootArg(t *testing.T) {
	// Disabled (zero value) => no ip= directive, and bootArgs is the bare cmdline.
	var off netConfig
	if off.enabled() {
		t.Fatal("zero-value netConfig should be disabled")
	}
	if off.bootArg() != "" {
		t.Fatalf("disabled bootArg should be empty, got %q", off.bootArg())
	}
	if got := (fcDriver{}).bootArgs(); strings.Contains(got, "ip=") {
		t.Fatalf("device-less bootArgs should have no ip=, got %q", got)
	}
	// Must boot from the rootfs block device (Firecracker doesn't synthesize root=).
	if got := (fcDriver{}).bootArgs(); !strings.Contains(got, "root=/dev/vda") {
		t.Fatalf("bootArgs must specify the root device, got %q", got)
	}

	// Enabled => point-to-point ip= directive baked into the kernel cmdline.
	on := netConfig{
		tapDevice: "fc-tap0",
		guestIP:   "172.16.0.2",
		hostIP:    "172.16.0.1",
		netmask:   "255.255.255.252",
		guestMAC:  "06:00:AC:10:00:02",
	}
	if !on.enabled() {
		t.Fatal("configured netConfig should be enabled")
	}
	want := "ip=172.16.0.2::172.16.0.1:255.255.255.252::eth0:off"
	if on.bootArg() != want {
		t.Fatalf("bootArg: want %q, got %q", want, on.bootArg())
	}
	full := fcDriver{net: on}.bootArgs()
	if !strings.Contains(full, "console=ttyS0") || !strings.Contains(full, "root=/dev/vda") || !strings.HasSuffix(full, want) {
		t.Fatalf("bootArgs should keep base flags and append ip=, got %q", full)
	}
}

func TestEnsureTapExists(t *testing.T) {
	// A name that cannot exist under /sys/class/net should fail with guidance.
	err := ensureTapExists("definitely-not-a-real-tap-xyz")
	if err == nil {
		t.Fatal("expected error for missing TAP")
	}
	if !strings.Contains(err.Error(), "not found") {
		t.Fatalf("error should mention the missing TAP, got %v", err)
	}

	// "lo" exists as /sys/class/net/lo on Linux, so the check passes for it.
	// Only assert that on a host that actually has the sysfs entry (not macOS).
	if _, statErr := os.Stat("/sys/class/net/lo"); statErr == nil {
		if err := ensureTapExists("lo"); err != nil {
			t.Fatalf("ensureTapExists(lo) should pass when /sys/class/net/lo exists, got %v", err)
		}
	}
}

// fakeGuest is a guestRunner that fails its first failUntil exec() calls, then
// succeeds, recording every command. copyOut always succeeds and is recorded.
type fakeGuest struct {
	failUntil int
	calls     int
	execCmds  []string
	copies    [][2]string
	execErr   error
}

func (g *fakeGuest) exec(command string) error {
	g.calls++
	g.execCmds = append(g.execCmds, command)
	if g.calls <= g.failUntil {
		if g.execErr != nil {
			return g.execErr
		}
		return errFakeGuest
	}
	return nil
}

func (g *fakeGuest) copyOut(guestPath, hostPath string) error {
	g.copies = append(g.copies, [2]string{guestPath, hostPath})
	return nil
}

var errFakeGuest = &fakeErr{"guest not ready"}

type fakeErr struct{ s string }

func (e *fakeErr) Error() string { return e.s }

func TestWaitForGuestRetriesThenSucceeds(t *testing.T) {
	g := &fakeGuest{failUntil: 2}
	d := fcDriver{guest: g, pollInterval: time.Millisecond}
	if err := d.waitForGuest(5 * time.Second); err != nil {
		t.Fatalf("expected success after retries, got %v", err)
	}
	if g.calls != 3 {
		t.Fatalf("expected 3 attempts (2 fail + 1 ok), got %d", g.calls)
	}
}

func TestWaitForGuestTimesOut(t *testing.T) {
	g := &fakeGuest{failUntil: 1 << 30} // always fail
	d := fcDriver{guest: g, pollInterval: time.Millisecond}
	err := d.waitForGuest(20 * time.Millisecond)
	if err == nil {
		t.Fatal("expected timeout error")
	}
	if !strings.Contains(err.Error(), "not reachable") {
		t.Fatalf("error should mention unreachable, got %v", err)
	}
}

func TestWarmupCommand(t *testing.T) {
	d := fcDriver{guestSnapDir: "/snap"}
	cmd := d.warmupCommand(recordRequest{
		Workspace: "/work", BaseSHA: "abc", Bazel: "bazel", Flags: []string{"--useCquery"},
	})
	for _, want := range []string{
		"git -C /work checkout --force --quiet abc",
		"bazel-diff warmup -w /work -b bazel",
		"--base-hashes /snap/base_hashes.json",
		"--fingerprint-output /snap/fingerprint.json",
		"--useCquery",
		" && ",
	} {
		if !strings.Contains(cmd, want) {
			t.Fatalf("warmupCommand missing %q in:\n%s", want, cmd)
		}
	}
}

func TestConsumeScript(t *testing.T) {
	d := fcDriver{guestSnapDir: "/snap"}
	s := d.consumeScript(consumeRequest{Workspace: "/work", TargetSHA: "def", Bazel: "bazel"})
	for _, want := range []string{
		"git -C /work checkout --force --quiet def",
		"bazel-diff generate-hashes -w /work -b bazel /snap/target_hashes.json",
		"get-impacted-targets -w /work -b bazel -sh /snap/base_hashes.json -fh /snap/target_hashes.json -o /snap/impacted.txt",
	} {
		if !strings.Contains(s, want) {
			t.Fatalf("consumeScript missing %q in:\n%s", want, s)
		}
	}
}

func TestBaseRootfs(t *testing.T) {
	d := fcDriver{kernelImage: "/img/vmlinux-6.1.128"}
	if got := d.baseRootfs(); got != "/img/rootfs.base.ext4" {
		t.Fatalf("baseRootfs: want /img/rootfs.base.ext4, got %s", got)
	}
}

func TestCopyFile(t *testing.T) {
	dir := t.TempDir()
	src := dir + "/src"
	if err := os.WriteFile(src, []byte("hello"), 0o644); err != nil {
		t.Fatal(err)
	}
	dst := dir + "/nested/dir/dst" // parent dirs must be created
	if err := copyFile(src, dst); err != nil {
		t.Fatal(err)
	}
	b, err := os.ReadFile(dst)
	if err != nil || string(b) != "hello" {
		t.Fatalf("copyFile result wrong: %q err=%v", string(b), err)
	}
	if err := copyFile(dir+"/missing", dst); err == nil {
		t.Fatal("copyFile of a missing source should error")
	}
}

func TestSSHGuestBaseArgs(t *testing.T) {
	g := sshGuest{addr: "root@h", identity: "/k", sshOpts: []string{"-o", "BatchMode=yes"}}
	args := g.base("ssh")
	joined := strings.Join(args, " ")
	if !strings.Contains(joined, "-i /k") || !strings.Contains(joined, "BatchMode=yes") {
		t.Fatalf("base args missing identity/opts: %v", args)
	}
	// No identity => no -i flag.
	if strings.Contains(strings.Join(sshGuest{addr: "x"}.base("ssh"), " "), "-i") {
		t.Fatal("base args should omit -i when identity is empty")
	}
}

func TestTeardownNilSafe(t *testing.T) {
	teardown(nil) // must not panic on a nil cmd / nil process
}

func TestFCDriverName(t *testing.T) {
	if (fcDriver{}).name() != "firecracker" {
		t.Fatal("fcDriver.name() should be 'firecracker'")
	}
}

func TestBootMissingBinary(t *testing.T) {
	d := fcDriver{
		firecrackerBin: "/definitely/not/a/firecracker/binary",
		socketPath:     filepath.Join(t.TempDir(), "fc.sock"),
	}
	if _, err := d.boot(); err == nil {
		t.Fatal("boot should error when the firecracker binary is missing")
	}
}
