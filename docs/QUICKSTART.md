# Zeroboot Quick Start Guide

> Based on chaosreload/zeroboot (feat/virtio-blk-filesystem branch)
> Environment: AWS c8i.xlarge, Ubuntu 22.04, nested virtualization enabled

---

## 1. Machine Setup

### 1.1 Launch a New Instance (Recommended)

Enable nested virtualization at launch time via `--cpu-options` — no need for a stop/modify/start cycle.

> ⚠️ Requires AWS CLI >= v2.34. Older versions don't support the `NestedVirtualization` parameter.

```bash
# Get the latest Ubuntu 22.04 AMI (ap-southeast-1)
AMI_ID=$(aws ec2 describe-images \
  --owners 099720109477 \
  --filters 'Name=name,Values=ubuntu/images/hvm-ssd/ubuntu-jammy-22.04-amd64-server-*' \
            'Name=state,Values=available' \
  --query 'sort_by(Images, &CreationDate)[-1].ImageId' \
  --region ap-southeast-1 --output text)

# Launch with nested virtualization enabled in one step
INSTANCE_ID=$(aws ec2 run-instances \
  --image-id $AMI_ID \
  --instance-type c8i.xlarge \
  --key-name <your-key-name> \
  --security-group-ids <your-sg-id> \
  --cpu-options "NestedVirtualization=enabled" \
  --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=zeroboot-fresh}]' \
  --region ap-southeast-1 \
  --query 'Instances[0].InstanceId' --output text)

echo "Instance ID: $INSTANCE_ID"

# Wait until running
aws ec2 wait instance-running --instance-ids $INSTANCE_ID --region ap-southeast-1
```

> ⚠️ Only c8i / m8i / r8i instance families support nested virtualization (Intel 8th-gen platform only).

### 1.2 Enable Nested Virtualization on an Existing Instance

If you already have a running C8i instance, stop it first:

```bash
aws ec2 stop-instances --region ap-southeast-1 --instance-ids $INSTANCE_ID
aws ec2 wait instance-stopped --region ap-southeast-1 --instance-ids $INSTANCE_ID

aws ec2 modify-instance-cpu-options \
  --region ap-southeast-1 \
  --instance-id $INSTANCE_ID \
  --nested-virtualization enabled

aws ec2 start-instances --region ap-southeast-1 --instance-ids $INSTANCE_ID
```

### 1.3 Verify KVM is Available

```bash
ssh ubuntu@<your-instance-ip>
ls -la /dev/kvm
# Expected: crw-rw-rw- 1 root kvm 10, 232 ...

# If permission denied:
sudo chmod 666 /dev/kvm
```

---

## 2. Install Firecracker

```bash
curl -L -o fc.tgz https://github.com/firecracker-microvm/firecracker/releases/download/v1.15.0/firecracker-v1.15.0-x86_64.tgz
tar xzf fc.tgz
sudo mv release-v1.15.0-x86_64/firecracker-v1.15.0-x86_64 /usr/local/bin/firecracker
sudo mv release-v1.15.0-x86_64/jailer-v1.15.0-x86_64 /usr/local/bin/jailer
sudo chmod +x /usr/local/bin/firecracker /usr/local/bin/jailer
firecracker --version
# Expected: Firecracker v1.15.0
```

---

## 3. Download Kernel

```bash
mkdir -p ~/fc-exp
cd ~/fc-exp

# Firecracker official quickstart kernel (4.14.174, fast boot)
curl -fsSL -o vmlinux.bin \
  https://s3.amazonaws.com/spec.ccfc.min/img/quickstart_guide/x86_64/kernels/vmlinux.bin

ls -lh vmlinux.bin
# Expected: ~21MB
```

---

## 4. Build Rootfs with Docker

Building with Docker is more reproducible and maintainable than debootstrap (similar to how E2B sandbox templates work).

### 4.1 Install Docker

```bash
# Remove conflicting packages
sudo apt remove $(dpkg --get-selections docker.io docker-compose docker-compose-v2 docker-doc podman-docker containerd runc 2>/dev/null | cut -f1) 2>/dev/null || true

# Add Docker's official GPG key
sudo apt update
sudo apt install -y ca-certificates curl
sudo install -m 0755 -d /etc/apt/keyrings
sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
sudo chmod a+r /etc/apt/keyrings/docker.asc

# Add Docker repository
sudo tee /etc/apt/sources.list.d/docker.sources <<EOF
Types: deb
URIs: https://download.docker.com/linux/ubuntu
Suites: $(. /etc/os-release && echo "${UBUNTU_CODENAME:-$VERSION_CODENAME}")
Components: stable
Signed-By: /etc/apt/keyrings/docker.asc
EOF

sudo apt update
sudo apt install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
sudo usermod -aG docker $USER
```

### 4.2 Create Dockerfile

```bash
mkdir -p ~/zeroboot-rootfs
cd ~/zeroboot-rootfs

# Download guest init source
curl -fsSL -o init.c \
  https://raw.githubusercontent.com/chaosreload/zeroboot/feat/virtio-blk-filesystem/guest/init.c

cat > Dockerfile << 'EOF'
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update -qq && \
    apt-get install -y --no-install-recommends \
      python3 python3-pip gcc libc6-dev && \
    pip3 install --no-cache-dir numpy pandas && \
    apt-get clean && rm -rf /var/lib/apt/lists/*

# Compile statically-linked guest init (PID 1 inside the VM)
COPY init.c /init.c
RUN gcc -O2 -static -o /init /init.c && rm /init.c
EOF
```

### 4.3 Build and Export to ext4

```bash
cd ~/zeroboot-rootfs

# Build image (~3 min, mostly pip install)
sudo docker build -t zeroboot-rootfs .

# Verify static linking
sudo docker run --rm zeroboot-rootfs ldd /init
# Expected: not a dynamic executable

# Export as tar
sudo docker create --name tmp-rootfs zeroboot-rootfs
sudo docker export tmp-rootfs -o rootfs.tar
sudo docker rm tmp-rootfs

# Pack into ext4 image
cd ~/fc-exp
dd if=/dev/zero of=rootfs.ext4 bs=1M count=1500 status=progress
mkfs.ext4 -F rootfs.ext4

sudo mkdir -p /mnt/rootfs_out
sudo mount -o loop rootfs.ext4 /mnt/rootfs_out
sudo tar xf ~/zeroboot-rootfs/rootfs.tar -C /mnt/rootfs_out
sudo umount /mnt/rootfs_out

ls -lh rootfs.ext4
# Expected: ~1.5GB
```

### 4.4 Verify Rootfs

```bash
sudo mount -o loop,ro rootfs.ext4 /mnt/rootfs_out
sudo chroot /mnt/rootfs_out python3 -c "import numpy, pandas; print('numpy', numpy.__version__, 'pandas', pandas.__version__)"
sudo chroot /mnt/rootfs_out ldd /init
sudo umount /mnt/rootfs_out
```

---

## 5. Build Zeroboot

```bash
# Install dependencies (C toolchain + Rust)
sudo apt-get install -y build-essential
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

# Clone the repo
git clone -b feat/virtio-blk-filesystem \
  https://github.com/chaosreload/zeroboot.git ~/zeroboot
cd ~/zeroboot

# Build in release mode (~30s)
cargo build --release

ls -lh target/release/zeroboot
# Expected: ~1.8MB ELF binary
```

---

## 6. Create Template (Take Snapshot)

```bash
# Drop page cache to avoid OOM during snapshot
echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null

mkdir -p ~/zeroboot-work

# Take snapshot (~10-15 seconds)
# Args: <kernel> <rootfs> <workdir> <wait_secs> <init_path> <mem_mib>
~/zeroboot/target/release/zeroboot template \
  ~/fc-exp/vmlinux.bin \
  ~/fc-exp/rootfs.ext4 \
  ~/zeroboot-work \
  10 /init 512

# Expected output:
# Starting Firecracker...
# Firecracker VM started
# Waiting 10s for guest to boot...
# Pausing VM...
# Creating snapshot...
# Snapshot created: state=14312B, mem=512MB
# Template created in 13.xx s
```

Verify the output:

```bash
ls -lh ~/zeroboot-work/snapshot/
# vmstate  (~14KB, CPU register state)
# mem      (~512MB, memory image)

cat ~/zeroboot-work/rootfs_path
# Should show the absolute path to rootfs.ext4
```

---

## 7. Test Execution

### 7.1 Basic echo

```bash
echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null

~/zeroboot/target/release/zeroboot test-exec ~/zeroboot-work "echo hello"
# Expected:
# Fork time: ~1ms
# === Output ===
# echo hello
# hello
# ZEROBOOT_DONE
```

### 7.2 Read a file (verify filesystem access)

```bash
~/zeroboot/target/release/zeroboot test-exec ~/zeroboot-work "cat /etc/os-release"
# Expected: Ubuntu 22.04 release info
```

### 7.3 Execute Python code

```bash
# Simple calculation
~/zeroboot/target/release/zeroboot test-exec ~/zeroboot-work "CODE:print(1+1)"
# Expected: 2

# Using numpy
~/zeroboot/target/release/zeroboot test-exec ~/zeroboot-work \
  "CODE:import numpy as np; print(np.array([1,2,3]).mean())"
# Expected: 2.0

# Write a file (verify CoW isolation — base image is never modified)
~/zeroboot/target/release/zeroboot test-exec ~/zeroboot-work \
  "CODE:open('/tmp/test','w').write('hello'); print(open('/tmp/test').read())"
# Expected: hello
```

---

## 8. Benchmark

```bash
echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null
~/zeroboot/target/release/zeroboot bench ~/zeroboot-work 2>/dev/null
# Reference numbers (c8i.xlarge, warm page cache):
# Fork P50: ~655µs  ← sub-millisecond!
# Fork P99: ~996µs
# Fork + echo P50: ~5.8ms
# Memory per fork (100 concurrent): ~169KB
```

---

## 9. Start the API Server

```bash
~/zeroboot/target/release/zeroboot serve ~/zeroboot-work 8080
# Zeroboot API server listening on port 8080
```

In another terminal:

```bash
# Health check
curl localhost:8080/v1/health

# Execute Python
curl -X POST localhost:8080/v1/exec \
  -H 'Content-Type: application/json' \
  -d '{"code": "print(1+1)"}'

# Expected response:
# {"id":"...","stdout":"2","stderr":"","exit_code":0,"fork_time_ms":0.65,...}
```

---

## 10. How It Works

```
test-exec / serve
    │
    ├─ load_snapshot()
    │    ├─ sendfile(mem_file → memfd)   [512MB, kernel-to-kernel, no user buffer]
    │    └─ parse_vmstate()              [CPU registers, virtio queue addresses]
    │
    └─ fork_cow()  [~1ms]
         ├─ KVM: create_vm + create_irq_chip
         ├─ mmap(memfd, MAP_PRIVATE)      [CoW: shared reads, page fault on write]
         ├─ Restore CPU state: sregs → XCRS → XSAVE → regs → LAPIC → MSRs
         ├─ Create OverlayBlockDevice     [per-fork in-memory CoW layer]
         └─ VM run loop
              ├─ IoOut/IoIn  → 16550 UART  (serial I/O)
              └─ MmioWrite   → VirtioBlk   (filesystem I/O)
```

---

## Troubleshooting

**Q: `ls: cannot access '/dev/kvm'`**  
A: Nested virtualization is not enabled. Follow Step 1.

**Q: `OOM Killed`**  
A: Run `echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null` to free page cache before taking the snapshot.

**Q: `echo hello` works but `CODE:` hangs**  
A: The snapshot was taken before Python finished booting. Increase `wait_secs` in Step 6 to 15.

**Q: `Warning: snapshot CPUID rejected`**  
A: Expected under nested virtualization. Harmless — suppress with `2>/dev/null`.

**Q: `Too many open files (os error 24)` at 1000-concurrent bench**  
A: Run `ulimit -n 65535` to raise the file descriptor limit.

**Q: AWS CLI error `Unknown parameter in CpuOptions: NestedVirtualization`**  
A: Upgrade AWS CLI to v2.34+.
