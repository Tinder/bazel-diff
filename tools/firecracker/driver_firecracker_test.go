package main

import (
	"path/filepath"
	"strings"
	"testing"
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

	// "lo" always exists as /sys/class/net/lo, so the check passes for it.
	if _, statErr := filepathGlobLo(); statErr == nil {
		if err := ensureTapExists("lo"); err != nil {
			t.Fatalf("ensureTapExists(lo) should pass on Linux, got %v", err)
		}
	}
}

// filepathGlobLo reports whether /sys/class/net/lo is present, so the lo branch
// of TestEnsureTapExists only runs on a Linux host that has it.
func filepathGlobLo() ([]string, error) {
	return filepath.Glob("/sys/class/net/lo")
}
