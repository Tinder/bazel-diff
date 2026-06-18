package main

import (
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

// guestRunner executes commands and moves files in/out of the running microVM.
// Implementations are infrastructure-specific (SSH over a TAP/vsock network);
// abstracting them keeps fcDriver testable and lets operators swap transports.
type guestRunner interface {
	exec(command string) error
	copyOut(guestPath, hostPath string) error
}

// fcDriver records and consumes snapshots using Firecracker microVMs.
//
// REQUIRES Linux + KVM (/dev/kvm) on the host, a prepared kernel + rootfs base
// image (bazel + JDK + git + the bazel-diff binary + the workspace baked in),
// and a configured guestRunner. None of that is exercisable on macOS; this is
// the path that runs on the self-hosted CI host (RFC §6).
type fcDriver struct {
	firecrackerBin string // path to the `firecracker` binary
	socketPath     string // API unix socket
	kernelImage    string // guest kernel
	vcpus          int
	memMib         int
	guest          guestRunner

	// guestSnapDir is where warmup writes base_hashes.json / fingerprint.json
	// inside the guest (matches the CLI defaults under /snap).
	guestSnapDir string
}

func (fcDriver) name() string { return "firecracker" }

// boot launches the firecracker process against socketPath and returns it so the
// caller can tear it down. The process is detached from our stdout.
func (d fcDriver) boot() (*exec.Cmd, error) {
	_ = os.Remove(d.socketPath)
	cmd := exec.Command(d.firecrackerBin, "--api-sock", d.socketPath)
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("starting firecracker: %w", err)
	}
	// Wait for the API socket to appear.
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(d.socketPath); err == nil {
			return cmd, nil
		}
		time.Sleep(50 * time.Millisecond)
	}
	_ = cmd.Process.Kill()
	return nil, fmt.Errorf("firecracker API socket %s never appeared", d.socketPath)
}

func teardown(cmd *exec.Cmd) {
	if cmd != nil && cmd.Process != nil {
		_ = cmd.Process.Kill()
		_, _ = cmd.Process.Wait()
	}
}

// record boots a fresh VM on a read-write copy of the rootfs, warms it, copies
// the base hashes + fingerprint out to the store entry, then pauses and writes a
// full snapshot. The frozen rootfs becomes the consume-time backing image.
func (d fcDriver) record(r recordRequest) error {
	rootfs := r.Entry.rootfs()
	if err := copyFile(d.baseRootfs(), rootfs); err != nil {
		return fmt.Errorf("preparing rootfs: %w", err)
	}

	proc, err := d.boot()
	if err != nil {
		return err
	}
	defer teardown(proc)
	c := newFCClient(d.socketPath)

	if err := c.setMachineConfig(machineConfig{VCPUCount: d.vcpus, MemSizeMib: d.memMib}); err != nil {
		return err
	}
	if err := c.setBootSource(bootSource{
		KernelImagePath: d.kernelImage,
		BootArgs:        "console=ttyS0 reboot=k panic=1 pci=off",
	}); err != nil {
		return err
	}
	if err := c.addDrive(drive{
		DriveID: "rootfs", PathOnHost: rootfs, IsRootDevice: true, IsReadOnly: false,
	}); err != nil {
		return err
	}
	if err := c.instanceStart(); err != nil {
		return err
	}

	// Inside the guest: warm the server, baking base_hashes.json + fingerprint.json.
	warmup := fmt.Sprintf(
		"bazel-diff warmup -w %s -b %s --base-hashes %s --fingerprint-output %s %s",
		r.Workspace, r.Bazel,
		filepath.Join(d.guestSnapDir, baseHashesName),
		filepath.Join(d.guestSnapDir, fingerprintName),
		strings.Join(r.Flags, " "),
	)
	if err := d.guest.exec(warmup); err != nil {
		return fmt.Errorf("guest warmup: %w", err)
	}
	// Copy the artifacts out for host-side bookkeeping + cold-fallback reuse.
	if err := d.guest.copyOut(filepath.Join(d.guestSnapDir, baseHashesName), r.Entry.baseHashes()); err != nil {
		return err
	}
	_ = d.guest.copyOut(filepath.Join(d.guestSnapDir, fingerprintName),
		filepath.Join(r.Entry.Dir, fingerprintName))

	if err := c.pause(); err != nil {
		return err
	}
	if err := c.createSnapshot(snapshotCreate{
		SnapshotType: "Full",
		SnapshotPath: r.Entry.vmstate(),
		MemFilePath:  r.Entry.memFile(),
	}); err != nil {
		return err
	}
	return nil
}

// consume restores the snapshot on a copy-on-write overlay, checks out the
// target revision in the warm guest, and runs generate-hashes +
// get-impacted-targets, copying the impacted list out to r.Out.
func (d fcDriver) consume(r consumeRequest) error {
	overlay := r.Out + ".rootfs.overlay"
	if err := copyFile(r.Entry.rootfs(), overlay); err != nil {
		return fmt.Errorf("preparing COW overlay: %w", err)
	}
	defer os.Remove(overlay)

	proc, err := d.boot()
	if err != nil {
		return err
	}
	defer teardown(proc)
	c := newFCClient(d.socketPath)

	if err := c.loadSnapshot(snapshotLoad{
		SnapshotPath: r.Entry.vmstate(),
		MemBackend:   memBackend{BackendType: "File", BackendPath: r.Entry.memFile()},
		ResumeVM:     true,
	}); err != nil {
		return fmt.Errorf("loading snapshot: %w", err)
	}

	guestTarget := filepath.Join(d.guestSnapDir, "target_hashes.json")
	guestOut := filepath.Join(d.guestSnapDir, "impacted.txt")
	script := strings.Join([]string{
		fmt.Sprintf("git -C %s checkout --force --quiet %s", r.Workspace, r.TargetSHA),
		fmt.Sprintf("bazel-diff generate-hashes -w %s -b %s %s %s",
			r.Workspace, r.Bazel, guestTarget, strings.Join(r.Flags, " ")),
		fmt.Sprintf("bazel-diff get-impacted-targets -w %s -b %s -sh %s -fh %s -o %s",
			r.Workspace, r.Bazel,
			filepath.Join(d.guestSnapDir, baseHashesName), guestTarget, guestOut),
	}, " && ")
	if err := d.guest.exec(script); err != nil {
		return fmt.Errorf("guest consume: %w", err)
	}
	return d.guest.copyOut(guestOut, r.Out)
}

func (d fcDriver) baseRootfs() string {
	// The base rootfs lives next to the kernel by convention.
	return filepath.Join(filepath.Dir(d.kernelImage), "rootfs.base.ext4")
}

func copyFile(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	if err := os.MkdirAll(filepath.Dir(dst), 0o755); err != nil {
		return err
	}
	out, err := os.Create(dst)
	if err != nil {
		return err
	}
	defer out.Close()
	if _, err := io.Copy(out, in); err != nil {
		return err
	}
	return out.Close()
}

// sshGuest is the default guestRunner: it shells out to `ssh`/`scp`. Operators
// configure the address + identity for their CI network.
type sshGuest struct {
	addr     string // user@host
	identity string // path to ssh private key, optional
	sshOpts  []string
}

func (g sshGuest) base(tool string) []string {
	args := []string{}
	if g.identity != "" {
		args = append(args, "-i", g.identity)
	}
	args = append(args, g.sshOpts...)
	_ = tool
	return args
}

func (g sshGuest) exec(command string) error {
	args := append(g.base("ssh"), g.addr, command)
	cmd := exec.Command("ssh", args...)
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

func (g sshGuest) copyOut(guestPath, hostPath string) error {
	if err := os.MkdirAll(filepath.Dir(hostPath), 0o755); err != nil {
		return err
	}
	args := append(g.base("scp"), g.addr+":"+guestPath, hostPath)
	cmd := exec.Command("scp", args...)
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	return cmd.Run()
}
