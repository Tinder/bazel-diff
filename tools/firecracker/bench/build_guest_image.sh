#!/usr/bin/env bash
# Build the Firecracker guest image (kernel + rootfs.base.ext4) that the
# `firecracker` driver boots, warms, and snapshots.
#
# The rootfs bakes in everything `bazel-diff warmup` / `generate-hashes` needs so
# the guest is self-contained and offline: a JDK, a pinned `bazel` binary, git,
# the bazel-diff fat jar (wrapped as `/usr/local/bin/bazel-diff`), the workspace,
# an sshd that trusts the operator key, and the /snap dir warmup writes to.
# fcDriver.baseRootfs() expects the rootfs at <kernel-dir>/rootfs.base.ext4, so
# both land in OUT together.
#
# Networking matches setup_tap.sh + the bazel-diff-snap defaults: the guest's
# stock fcnet-setup.sh derives the eth0 IP from the NIC MAC (06:00:AC:10:00:02 ->
# 172.16.0.2/30), so no DHCP and no in-image network config is needed.
#
# Requires (Linux): unsquashfs, mke2fs (e2fsprogs), chroot, curl, and network
# access for the one-time base download + apt install into the chroot.
#
#   OUT=/tmp/fc-image \
#   BAZEL_DIFF_JAR=bazel-bin/cli/bazel-diff_deploy.jar \
#   BAZEL_BIN=$(which bazelisk) \
#   WORKSPACE_SRC=/tmp/fcbench \
#   SSH_PUBKEY=~/.ssh/fc_guest.pub \
#   tools/firecracker/bench/build_guest_image.sh
set -euo pipefail

ARCH="${ARCH:-aarch64}"
KERNEL_VER="${KERNEL_VER:-6.1.128}"
CI_BASE="${CI_BASE:-https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.12/${ARCH}}"
OUT="${OUT:-/tmp/fc-image}"
SIZE_MB="${SIZE_MB:-6144}"
JDK_PKG="${JDK_PKG:-openjdk-21-jdk-headless}"

BAZEL_DIFF_JAR="${BAZEL_DIFF_JAR:?set BAZEL_DIFF_JAR to the bazel-diff_deploy.jar}"
BAZEL_BIN="${BAZEL_BIN:?set BAZEL_BIN to a bazel/bazelisk binary to bake in}"
WORKSPACE_SRC="${WORKSPACE_SRC:?set WORKSPACE_SRC to the git workspace to bake in}"
SSH_PUBKEY="${SSH_PUBKEY:?set SSH_PUBKEY to the public key the guest should trust}"

if [[ $EUID -ne 0 ]]; then echo "run as root (chroot + mke2fs): sudo -E $0" >&2; exit 1; fi

mkdir -p "$OUT"
KERNEL="$OUT/vmlinux-$KERNEL_VER"
ROOTFS_IMG="$OUT/rootfs.base.ext4"
WORK="$OUT/rootfs.work"

echo ">> [1/6] fetch kernel + base rootfs"
[[ -f "$KERNEL" ]] || curl -fsSL -o "$KERNEL" "$CI_BASE/vmlinux-$KERNEL_VER"
SQUASHFS="$OUT/ubuntu.squashfs"
[[ -f "$SQUASHFS" ]] || curl -fsSL -o "$SQUASHFS" "$CI_BASE/ubuntu-24.04.squashfs"

echo ">> [2/6] extract base rootfs"
rm -rf "$WORK"
unsquashfs -q -d "$WORK" "$SQUASHFS"

echo ">> [3/6] inject ssh key + snap dir"
install -d -m 700 "$WORK/root/.ssh"
install -m 600 "$SSH_PUBKEY" "$WORK/root/.ssh/authorized_keys"
install -d -m 755 "$WORK/snap"
# Generate guest host keys at build time so the first ssh doesn't race sshd.
ssh-keygen -A -f "$WORK" >/dev/null 2>&1 || chroot "$WORK" ssh-keygen -A
# Standalone always-on sshd: socket-activated sshd (the Ubuntu default) does not
# reliably service connections after a snapshot restore. Enable ssh.service and
# disable ssh.socket so the daemon is already listening when the VM resumes.
if [[ -e "$WORK/lib/systemd/system/ssh.service" ]]; then
  mkdir -p "$WORK/etc/systemd/system/multi-user.target.wants"
  ln -sf /lib/systemd/system/ssh.service \
    "$WORK/etc/systemd/system/multi-user.target.wants/ssh.service"
  rm -f "$WORK/etc/systemd/system/sockets.target.wants/ssh.socket"
  rm -f "$WORK/etc/systemd/system/ssh.service.requires/ssh.socket"
fi

echo ">> [4/6] install JDK + git into the chroot"
mount --bind /dev "$WORK/dev"; mount -t proc proc "$WORK/proc"; mount -t sysfs sys "$WORK/sys"
# devpts is needed by package postinst scripts (e.g. the JDK invokes java, which
# calls posix_openpt); without it dpkg --configure fails on minimized bases.
mkdir -p "$WORK/dev/pts"; mount -t devpts devpts "$WORK/dev/pts"
cp /etc/resolv.conf "$WORK/etc/resolv.conf"
trap 'umount -l "$WORK/dev/pts" "$WORK/dev" "$WORK/proc" "$WORK/sys" 2>/dev/null || true' EXIT
chroot "$WORK" /bin/bash -euxc "
  export DEBIAN_FRONTEND=noninteractive
  # Some hosts' chroot /tmp is missing/unwritable for the unprivileged _apt
  # sandbox user, which makes apt fail to stage temp files ('Couldn't create
  # temporary file ...') and then report repos as unsigned. Ensure a sticky
  # world-writable /tmp and run apt as root (sandbox off) to be host-agnostic.
  install -d -m 1777 /tmp
  # Minimized base images can lack apt's spool/log dirs; recreate them so
  # downloads and dpkg configuration (which writes apt logs) succeed.
  install -d /var/cache/apt/archives/partial /var/lib/apt/lists/partial /var/log/apt
  # ...and lack /usr/share/man/manN, which the JDK's update-alternatives needs
  # to create man symlinks (else 'error creating symbolic link .../java.1.gz').
  mkdir -p /usr/share/man/man1 /usr/share/man/man2 /usr/share/man/man3 \
           /usr/share/man/man4 /usr/share/man/man5 /usr/share/man/man6 \
           /usr/share/man/man7 /usr/share/man/man8
  apt-get -o APT::Sandbox::User=root update -qq
  apt-get -o APT::Sandbox::User=root install -y --no-install-recommends $JDK_PKG git ca-certificates >/dev/null
  apt-get clean && rm -rf /var/lib/apt/lists/*
"

echo ">> [5/6] bake bazel + bazel-diff + workspace"
install -m 755 "$BAZEL_BIN" "$WORK/usr/local/bin/bazel"
install -d -m 755 "$WORK/opt/bazel-diff"
install -m 644 "$BAZEL_DIFF_JAR" "$WORK/opt/bazel-diff/bazel-diff_deploy.jar"
cat > "$WORK/usr/local/bin/bazel-diff" <<'EOF'
#!/bin/sh
exec java -jar /opt/bazel-diff/bazel-diff_deploy.jar "$@"
EOF
chmod 755 "$WORK/usr/local/bin/bazel-diff"
# Bake the workspace (git repo) under /work. record/consume `git checkout` here.
# Own it as root (the guest runs commands as root) so git doesn't reject the repo
# with "detected dubious ownership" (exit 128) when the source uid differs.
rm -rf "$WORK/work"
cp -a "$WORKSPACE_SRC" "$WORK/work"
chown -R 0:0 "$WORK/work"

echo ">> [6/6] build ext4 image ($SIZE_MB MiB) at $ROOTFS_IMG"
umount -l "$WORK/dev/pts" "$WORK/dev" "$WORK/proc" "$WORK/sys" 2>/dev/null || true
trap - EXIT
rm -f "$ROOTFS_IMG"
mke2fs -q -F -L rootfs -t ext4 -d "$WORK" "$ROOTFS_IMG" "${SIZE_MB}M"

echo
echo "done:"
echo "  kernel : $KERNEL"
echo "  rootfs : $ROOTFS_IMG   (baseRootfs convention: rootfs.base.ext4 next to kernel)"
echo "  guest workspace: /work   guest snap dir: /snap   ssh: root@172.16.0.2"
