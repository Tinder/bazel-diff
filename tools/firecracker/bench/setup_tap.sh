#!/usr/bin/env bash
# Set up the host TAP that backs the guest NIC for the firecracker driver.
#
# The orchestrator (`bazel-diff-snap --driver firecracker`) is deliberately
# privilege-free: it references the TAP by name and checks it exists, but never
# creates it (that needs CAP_NET_ADMIN). This script is the operator/CI step that
# creates it. Run it once per host before record/consume; it is idempotent.
#
#   sudo tools/firecracker/bench/setup_tap.sh [TAP] [HOST_IP] [MASK_BITS]
#
# Defaults match the bazel-diff-snap defaults and the guest image's
# fcnet-setup.sh MAC->IP convention (MAC 06:00:AC:10:00:02 => guest 172.16.0.2):
#   TAP=fc-tap0  HOST_IP=172.16.0.1  MASK_BITS=30  (guest is .2 on the /30)
set -euo pipefail

TAP="${1:-fc-tap0}"
HOST_IP="${2:-172.16.0.1}"
MASK_BITS="${3:-30}"
USER_NAME="${SUDO_USER:-$(id -un)}"

if [[ $EUID -ne 0 ]]; then
  echo "must run as root (TAP creation needs CAP_NET_ADMIN): sudo $0 $*" >&2
  exit 1
fi

# Idempotent: recreate cleanly so re-runs don't accumulate stale addresses.
if ip link show "$TAP" >/dev/null 2>&1; then
  ip link del "$TAP"
fi
ip tuntap add dev "$TAP" mode tap user "$USER_NAME"
ip addr add "${HOST_IP}/${MASK_BITS}" dev "$TAP"
ip link set "$TAP" up

# Allow the guest to reach the outside world if the host forwards (optional;
# harmless if the guest needs no egress). Not enabled by default.
echo "TAP $TAP up: host ${HOST_IP}/${MASK_BITS}, owned by $USER_NAME"
ip addr show "$TAP"
