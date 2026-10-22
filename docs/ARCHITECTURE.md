# Architecture

## Overview

```
                    ┌─────────────────────────────────────────┐
                    │          API Server (axum/tokio)        │
                    │  auth · rate-limit · metrics · batch    │
                    └──────────────┬──────────────────────────┘
                                   │
                    ┌──────────────▼──────────────────────────┐
                    │          Fork Engine (kvm.rs)           │
                    │                                         │
                    │  1. KVM create_vm + create_irq_chip     │
                    │  2. Restore IOAPIC redirect table       │
                    │  3. mmap(MAP_PRIVATE) snapshot memory   │
                    │  4. Restore CPU: sregs → XCRS → XSAVE   │
                    │     → regs → LAPIC → MSRs → MP state    │
                    │  5. Serial I/O via 16550 UART emulation │
                    │  6. Virtio-blk MMIO + overlay CoW disk  │
                    └──────────────┬──────────────────────────┘
                                   │
               ┌───────────────────┼───────────────────┐
               ▼                   ▼                   ▼
         ┌──────────┐       ┌──────────┐       ┌──────────┐
         │  Fork A  │       │  Fork B  │       │  Fork C  │
         │  256MB   │       │  256MB   │       │  256MB   │
         │  (CoW)   │       │  (CoW)   │       │  (CoW)   │
         └──────────┘       └──────────┘       └──────────┘
         Actual RSS:         Actual RSS:         Actual RSS:
           ~265KB              ~265KB              ~265KB
```

## How It Works

### Template Creation (one-time)

Firecracker boots a VM with your runtime (Python+numpy+pandas, Node.js, etc.), pre-loads modules, and snapshots the full memory + CPU state. This takes ~15 seconds and produces a memory dump and vmstate file.

### Fork (~0.8ms)

Creates a new KVM VM, maps the snapshot memory with `MAP_PRIVATE` (copy-on-write), and restores all CPU state:

1. **KVM VM creation** — `KVM_CREATE_VM` + `KVM_CREATE_IRQCHIP` + `KVM_CREATE_PIT2`
2. **IOAPIC restore** — Read existing irqchip state, overwrite redirect table entries from snapshot, write back (do not zero-init)
3. **Memory mapping** — `mmap(MAP_PRIVATE)` on the snapshot file gives CoW semantics: reads hit the shared snapshot, writes trigger per-fork page faults
4. **CPU state restore** — Must follow exact order: `sregs` → `XCRS` → `XSAVE` → `regs` → `LAPIC` → `MSRs` → `MP_STATE`
5. **Serial I/O** — 16550 UART emulation for guest communication via `/dev/ttyS0`

### Isolation

Each fork gets its own KVM VM with private memory pages. Writes trigger CoW page faults — forks cannot see each other's data. This is hardware-enforced isolation via Intel VT-x/AMD-V, not containers or namespaces.

## Source Layout

| File | Purpose |
|---|---|
| `src/vmm/kvm.rs` | Fork engine: KVM VM + CoW mmap + CPU state restore + virtio-blk integration |
| `src/vmm/vmstate.rs` | Firecracker vmstate parser: auto-detect offsets + virtio queue addr detection |
| `src/vmm/virtio_blk.rs` | **Virtio-blk MMIO emulator + overlay CoW block device** |
| `src/vmm/firecracker.rs` | Template creation via Firecracker API |
| `src/vmm/serial.rs` | 16550 UART emulation for guest I/O |
| `src/api/handlers.rs` | HTTP API: exec, batch, health, metrics, auth |
| `src/main.rs` | CLI: template, test-exec, bench, serve |
| `guest/init.c` | Guest PID 1: serial command dispatcher + CODE: execution via popen(python3) |
| `sdk/python/` | Python SDK (zero dependencies) |
| `sdk/node/` | TypeScript SDK (zero dependencies, uses fetch) |
| `deploy/` | systemd service + fleet deploy script |

## Key Implementation Details

### Vmstate Parsing

Firecracker's vmstate is a binary blob with variable-length versionize sections. Offsets shift between rootfs variants and Firecracker versions. The parser auto-detects field locations using the IOAPIC base address (`0xFEC00000`) as an anchor pattern — never hardcode offsets.

### Entropy in Guests

`getrandom()` blocks in Firecracker VMs until the CRNG is initialized. Guest init scripts must seed entropy via the `RNDADDENTROPY` ioctl and pass `random.trust_cpu=on` as a kernel boot argument. The Node.js template uses a Python wrapper as PID 1 to handle entropy seeding before exec'ing node.

### Numpy SIMD Dispatch

Firecracker's CPUID filtering confuses numpy's runtime CPU feature detection. Set `NPY_DISABLE_CPU_FEATURES` in the guest init before importing numpy to avoid SIGILL crashes.

### IOAPIC Restore Pattern

Don't zero-init `kvm_irqchip`. Use `KVM_GET_IRQCHIP` first, then overwrite the redirect table entries from the snapshot, then `KVM_SET_IRQCHIP`. Zero-initializing corrupts other irqchip state and causes interrupt routing failures.

## Virtio-Blk Filesystem Emulation

Zeroboot implements a full virtio-blk MMIO device emulator (`src/vmm/virtio_blk.rs`) so each
forked VM has a working filesystem without relying on Firecracker at runtime.

### Data Flow

```
  rootfs.ext4  (read-only, shared across all forks via Arc<File>)
       |
       v
  OverlayBlockDevice  (per-fork in-memory CoW layer)
       |  read:  check overlay HashMap<sector, Vec<u8>> first
       |         on miss: pread() from shared base image
       |  write: insert sector into overlay (base image untouched)
       v
  VirtioBlk MMIO emulator  (handles KVM_EXIT_MmioWrite / MmioRead)
       |  guest writes QueueNotify to 0xC0001000+0x050
       |  -> read avail ring -> parse descriptor chain
       |  -> dispatch read/write/flush -> update used ring -> inject IRQ (GSI 5)
       v
  Guest kernel ext4  (transparent block device /dev/vda)
```

### Key Algorithms

**Overlay CoW isolation**
- Each fork owns a `HashMap<u64, Vec<u8>>` keyed by 512-byte sector number
- Writes only touch the overlay; the shared base image is opened O_RDONLY
- Forks never observe each other's writes; overlay is freed when the fork is dropped

**VIRTIO_F_EVENT_IDX suppression fix**
- The guest uses event index suppression to avoid redundant `QueueNotify` writes
- After draining the queue, the emulator writes `last_avail_idx` into the `avail_event`
  field of the used ring header — this tells the guest "notify me for the next request"
- Without this update, only the first I/O per wake-up would be processed

**Vmstate queue address detection (Firecracker v1.12 + v1.15)**
- v1.15 serializes `GuestAddress` with a 2-byte Versionize prefix: `[0x02][u32_LE]`
- Parser searches for pattern `[02][u32][02][u32][02][u32]` (desc/avail/used ring GPA)
- Falls back to 3-consecutive-raw-u64 format for Firecracker v1.12 compatibility

### Guest Init Protocol

`guest/init.c` is a statically-linked PID 1 that mounts filesystems and listens on `/dev/ttyS0`:

| Host sends | Guest action | Guest responds |
|---|---|---|
| `CODE:<python_code>\n` | Writes code to `/tmp/zb_code.py`, runs `python3 /tmp/zb_code.py 2>&1` | stdout + stderr |
| `echo <text>\n` | Writes text to serial | text |
| `cat <path>\n` | Opens file, reads to serial | file contents |
| *(any command)* | After response | `ZEROBOOT_DONE\n` |

### Building a Custom Rootfs

```bash
# 1. Bootstrap Ubuntu 22.04 minimal
sudo debootstrap --arch=amd64 jammy /tmp/rootfs http://archive.ubuntu.com/ubuntu/
echo "deb http://archive.ubuntu.com/ubuntu jammy main universe" | sudo tee /tmp/rootfs/etc/apt/sources.list

# 2. Install Python 3 + scientific packages
sudo chroot /tmp/rootfs apt-get update -qq
sudo chroot /tmp/rootfs apt-get install -y python3 python3-pip gcc
sudo chroot /tmp/rootfs pip3 install numpy pandas

# 3. Compile and install guest init (statically linked)
sudo cp guest/init.c /tmp/rootfs/init.c
sudo chroot /tmp/rootfs gcc -O2 -static -o /init /init.c
sudo rm /tmp/rootfs/init.c

# 4. Package as ext4 image (~1.5 GB)
dd if=/dev/zero of=rootfs.ext4 bs=1M count=1500
mkfs.ext4 -F rootfs.ext4
sudo mount -o loop rootfs.ext4 /mnt/out
sudo cp -a /tmp/rootfs/. /mnt/out/
sudo umount /mnt/out
```

### Creating a Template

```bash
# Boot the VM via Firecracker, wait for guest to reach the serial listen loop, snapshot
./zeroboot template vmlinux.bin rootfs.ext4 ./workdir 10 /init 512
#                   <kernel>   <rootfs>    <workdir> <wait_s> <init> <mem_mib>
```

`workdir/rootfs_path` is written automatically and picked up by `bench` / `serve` / `test-exec`.

### Observed Performance (c8i.xlarge, nested virtualization)

| Metric | Value |
|---|---|
| Pure CoW mmap P50 | 0.7 µs |
| Full fork (KVM + CPU restore) P50 | **655 µs** |
| Full fork P99 | **996 µs** |
| Fork + echo hello P50 | 5.8 ms |
| Fork + CODE:print(1+1) | ~205 ms |
| Fork + CODE:import numpy; ... | ~450 ms |
| Fork + cat /etc/os-release | ~30 ms |
| Memory per fork (100 concurrent) | ~169 KB |

Python exec latency (200–450 ms) reflects on-demand `.so` loading through virtio-blk.
This is a one-time cost per fork; pages are reused across CoW forks once warm.
**Planned optimization**: pre-warm all `.so` pages before snapshotting → zero disk I/O after fork.
