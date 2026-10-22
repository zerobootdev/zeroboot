#!/bin/bash
set -euo pipefail

WORKDIR="${ZEROBOOT_WORKDIR:-/var/lib/zeroboot}"
KERNEL="${ZEROBOOT_KERNEL:-${WORKDIR}/vmlinux-fc}"
ROOTFS_PYTHON="${ZEROBOOT_ROOTFS_PYTHON:-${WORKDIR}/rootfs-python.ext4}"
ROOTFS_NODE="${ZEROBOOT_ROOTFS_NODE:-}"
PORT="${ZEROBOOT_PORT:-8080}"
TEMPLATE_WAIT="${ZEROBOOT_TEMPLATE_WAIT:-15}"

# ── Validate KVM access ───────────────────────────────────────────────────────
if [ ! -c /dev/kvm ]; then
    echo "ERROR: /dev/kvm not found. Node must support KVM and use the KVM device plugin."
    exit 1
fi

# ── Check required files ──────────────────────────────────────────────────────
if [ ! -f "$KERNEL" ]; then
    echo "ERROR: Kernel not found at $KERNEL"
    echo "Mount a PersistentVolume to $WORKDIR containing vmlinux-fc and rootfs images."
    exit 1
fi

if [ ! -f "$ROOTFS_PYTHON" ]; then
    echo "ERROR: Python rootfs not found at $ROOTFS_PYTHON"
    exit 1
fi

# ── Create template if snapshot doesn't exist ────────────────────────────────
PYTHON_SNAPSHOT="${WORKDIR}/python/snapshot/vmstate"

if [ ! -f "$PYTHON_SNAPSHOT" ]; then
    echo "No snapshot found — creating Python template (this takes ~${TEMPLATE_WAIT}s)..."
    mkdir -p "${WORKDIR}/python"
    cp "$ROOTFS_PYTHON" "${WORKDIR}/python-rootfs.ext4"
    /usr/local/bin/zeroboot template \
        "$KERNEL" \
        "${WORKDIR}/python-rootfs.ext4" \
        "${WORKDIR}/python" \
        "$TEMPLATE_WAIT" \
        /init
    echo "Template created."
else
    echo "Snapshot found — skipping template creation."
fi

# ── Build serve target ────────────────────────────────────────────────────────
SERVE_TARGET="python:${WORKDIR}/python"

if [ -n "$ROOTFS_NODE" ] && [ -f "$ROOTFS_NODE" ]; then
    NODE_SNAPSHOT="${WORKDIR}/node/snapshot/vmstate"
    if [ ! -f "$NODE_SNAPSHOT" ]; then
        echo "Creating Node.js template..."
        mkdir -p "${WORKDIR}/node"
        cp "$ROOTFS_NODE" "${WORKDIR}/node-rootfs.ext4"
        /usr/local/bin/zeroboot template \
            "$KERNEL" \
            "${WORKDIR}/node-rootfs.ext4" \
            "${WORKDIR}/node" \
            "$TEMPLATE_WAIT" \
            /init-node.sh
        echo "Node template created."
    fi
    SERVE_TARGET="${SERVE_TARGET},node:${WORKDIR}/node"
fi

# ── Start API server ──────────────────────────────────────────────────────────
echo "Starting zeroboot API server on port ${PORT}..."
exec /usr/local/bin/zeroboot serve "$SERVE_TARGET" "$PORT"
