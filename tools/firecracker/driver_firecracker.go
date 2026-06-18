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
	net            netConfig

	// guestSnapDir is where warmup writes base_hashes.json / fingerprint.json
	// inside the guest (matches the CLI defaults under /snap).
	guestSnapDir string
}

// netConfig is the guest networking the driver attaches: a virtio-net device
// backed by a host TAP, with a static point-to-point address pair so the host
// can ssh in without DHCP. The TAP is owned by CI/operator setup (see
// bench/setup_tap.sh) — the driver only references it by name and bakes the
// guest-side address into the kernel `ip=` boot arg. Zero value => no network
// (the device-less boot used by the boot smoke test).
type netConfig struct {
	tapDevice string // host TAP name, e.g. "fc-tap0"
	guestIP   string // guest address, e.g. "172.16.0.2"
	hostIP    string // host/gateway address, e.g. "172.16.0.1"
	netmask   string // e.g. "255.255.255.252"
	guestMAC  string // stable MAC so the guest's NIC name survives restore
}

func (n netConfig) enabled() bool { return n.tapDevice != "" }

// bootArg renders the kernel `ip=` directive that statically configures eth0 in
// the guest at boot, so it is reachable over the TAP with no DHCP server.
// Format: ip=<client>:<server>:<gw>:<mask>:<host>:<dev>:<autoconf>.
func (n netConfig) bootArg() string {
	if !n.enabled() {
		return ""
	}
	return fmt.Sprintf("ip=%s::%s:%s::eth0:off", n.guestIP, n.hostIP, n.netmask)
}

func (fcDriver) name() string { return "firecracker" }

// bootArgs is the guest kernel command line. It boots from the root virtio-block
// device (the rootfs drive is added as the first /dev/vda; Firecracker does not
// synthesize a `root=` arg, so we must pass it). When networking is configured
// the `ip=` directive is appended so the restored guest comes up addressable.
// Because boot args are captured in the snapshot, the address baked here is what
// consume-time ssh must target.
func (d fcDriver) bootArgs() string {
	args := "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw"
	if na := d.net.bootArg(); na != "" {
		args += " " + na
	}
	return args
}

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
		BootArgs:        d.bootArgs(),
	}); err != nil {
		return err
	}
	if err := c.addDrive(drive{
		DriveID: "rootfs", PathOnHost: rootfs, IsRootDevice: true, IsReadOnly: false,
	}); err != nil {
		return err
	}
	// Attach the guest NIC before boot. Net devices cannot be hot-added, and the
	// device config is captured in the snapshot so consume reconnects to a TAP of
	// the same name. Skipped only by the device-less boot smoke test.
	if d.net.enabled() {
		if err := c.addNetworkInterface(networkInterface{
			IfaceID: "eth0", HostDevName: d.net.tapDevice, GuestMAC: d.net.guestMAC,
		}); err != nil {
			return err
		}
	}
	if err := c.instanceStart(); err != nil {
		return err
	}

	// Inside the guest: check out the base revision, then warm the server, baking
	// base_hashes.json + fingerprint.json. The checkout mirrors localDriver.record
	// (and consume's target checkout) so the baked hashes are for the base SHA
	// regardless of what revision the image happens to be baked at.
	warmup := strings.Join([]string{
		fmt.Sprintf("git -C %s checkout --force --quiet %s", r.Workspace, r.BaseSHA),
		fmt.Sprintf("bazel-diff warmup -w %s -b %s --base-hashes %s --fingerprint-output %s %s",
			r.Workspace, r.Bazel,
			filepath.Join(d.guestSnapDir, baseHashesName),
			filepath.Join(d.guestSnapDir, fingerprintName),
			strings.Join(r.Flags, " ")),
	}, " && ")
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
	// On restore Firecracker reconnects the snapshotted virtio-net device to a
	// host TAP with the same name it had at record time. If that TAP is missing
	// the restore fails, so check the precondition up front with a clear error.
	if d.net.enabled() {
		if err := ensureTapExists(d.net.tapDevice); err != nil {
			return err
		}
	}

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

// ensureTapExists verifies the host TAP is present before a restore. TAP setup
// needs CAP_NET_ADMIN and is the operator/CI's responsibility (bench/setup_tap.sh),
// so the driver only checks for it rather than creating it — keeping the tool
// privilege-free. A network device appears as /sys/class/net/<name>.
func ensureTapExists(tap string) error {
	if _, err := os.Stat(filepath.Join("/sys/class/net", tap)); err != nil {
		return fmt.Errorf("host TAP %q not found (set it up first, e.g. bench/setup_tap.sh): %w", tap, err)
	}
	return nil
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
