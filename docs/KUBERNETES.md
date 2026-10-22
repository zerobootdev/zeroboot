# Running Zeroboot on Kubernetes

Zeroboot can be deployed as a stateful service inside a Kubernetes cluster.
This guide covers node requirements, KVM device access, persistent storage for
snapshots, reference manifests, and autoscaling.

---

## Architecture overview

```
Internet → K8s Service
               │
        ┌──────┼──────┐
        │      │      │
     Node-1  Node-2  Node-3     ← KVM-capable nodes (kvm-capable=true)
        │      │      │
     Pod-1   Pod-2   Pod-3      ← one Pod per Node (DaemonSet, default)
        │      │      │
     VM VM   VM VM   VM VM      ← KVM forks happen inside the Pod, sub-millisecond
```

**Key point:** Kubernetes manages the lifecycle of the zeroboot *server* process.
It does not schedule individual sandboxes — each `v1/exec` request is handled
entirely within the Pod that receives it via a KVM fork (~0.8 ms). Kubernetes'
role is capacity management: health checks, rolling updates, and node-level scaling.

**Why DaemonSet?** zeroboot is tightly bound to the host's `/dev/kvm` and CPU
microarchitecture. The natural unit of scale is a Node, not a Pod replica —
adding a KVM-capable node should automatically bring up a new zeroboot instance.
DaemonSet is the correct primitive for this semantics.

---

## Node requirements

### Instance families with KVM support

Not all EC2 instance types expose `/dev/kvm`. The following families support KVM
and are suitable for zeroboot:

| Family | KVM method | Notes |
|---|---|---|
| `c8i`, `m8i`, `r8i` | ✅ **Nested virtualization** | **Recommended** — Intel 8th-gen Nitro platform; supports nested virt without metal. Enable at launch via `--cpu-options NestedVirtualization=enabled` (requires AWS CLI ≥ v2.34) |
| `c6i`, `c6a`, `c7i`, `m6i`, `m7i`, `r6i`, `r7i` | ✅ Bare-metal only | KVM available only on `.metal` sizes (e.g. `c6i.metal`) |
| `c5`, `m5`, `r5` | ✅ Bare-metal only | Older Nitro generation; `.metal` sizes only |
| `t3`, `t4g` | ❌ Not available | Burstable — `/dev/kvm` not exposed |
| `t2` | ❌ Not available | No Nitro, no KVM |
| Any ARM (`*g`) | ❌ Architecture mismatch | Firecracker x86_64 binary required |

**TL;DR for EKS node groups:** Use `c8i`, `m8i`, or `r8i` with nested virtualization
enabled — these are the only non-metal families where regular (non-`.metal`) instance
sizes expose `/dev/kvm`. All other families require `.metal` sizes which are significantly
more expensive and harder to schedule in K8s.

```bash
# Enable nested virtualization when launching a new instance (c8i/m8i/r8i only)
aws ec2 run-instances \
  --instance-type c8i.xlarge \
  --cpu-options "NestedVirtualization=enabled" \
  ...
```

> On GCP: `n2`, `n2d`, `c2`, `c3` families support KVM.
> On Azure: `Dv3`, `Ev3`, `Dsv3` with nested virtualization enabled.

### Label KVM-capable nodes

```bash
kubectl label node <node-name> kvm-capable=true
```

The DaemonSet's `nodeSelector` uses this label to ensure Pods are only scheduled
where `/dev/kvm` is available.

---

## EKS deployment: managed vs self-managed node groups

> **TL;DR:** Use a self-managed node group. EKS managed node groups silently
> drop `CpuOptions.NestedVirtualization` — your nodes will start without `/dev/kvm`.

### The problem with managed node groups

EKS managed node groups take your Launch Template, then generate a new internal
Launch Template that merges only a subset of fields. `CpuOptions` is not in that
subset — even though it is **not** listed in the [official blocked-fields docs](https://docs.aws.amazon.com/eks/latest/userguide/launch-templates.html#launch-template-basics).

Symptoms:
- `ls /dev/kvm` returns "No such file or directory"
- `/proc/cpuinfo` has no `vmx` flag
- `eksctl create nodegroup` succeeds, but KVM is silently missing

You can verify by inspecting the EKS-generated internal Launch Template:

```bash
# Get the internal LT id (not your LT)
aws ec2 describe-launch-template-versions   --launch-template-id <EKS_GENERATED_LT_ID> --versions 1   --query "LaunchTemplateVersions[0].LaunchTemplateData.CpuOptions"
# Expected for managed nodegroup: null  (even if you set it in your own LT)
```

### The solution: self-managed node group

Create an Auto Scaling Group with a Launch Template directly — bypassing EKS's
internal LT generation. The provided script handles the full setup:

```bash
export AWS_PROFILE=your-profile
export CLUSTER_NAME=zeroboot-eks
export REGION=ap-southeast-1

# Step 1: Create cluster without node group
eksctl create cluster -f deploy/eks/eks-cluster-only.yaml

# Step 2: Create self-managed KVM node group
bash deploy/eks/eks-self-managed-kvm.sh
```

The script:
1. Creates an IAM node role + instance profile
2. Registers the role with EKS via `create-access-entry`
3. Queries the latest EKS-optimized AL2023 AMI
4. Creates a Launch Template with `CpuOptions.NestedVirtualization=enabled`
5. Creates an ASG referencing the LT directly
6. Verifies `/dev/kvm` is present on the new nodes

> **Note:** `eksctl`'s `nodeGroups` (non-managed) do not support `launchTemplate`.
> Only `managedNodeGroups` does — but managed NGs drop `CpuOptions`. The script
> uses raw AWS CLI (`ec2 create-launch-template` + `autoscaling create-auto-scaling-group`)
> to sidestep both limitations.


---

## KVM device access without `privileged: true`

Pods request `/dev/kvm` via the [KVM device plugin](https://github.com/kubevirt/kubevirt/tree/main/cmd/virt-handler)
from the KubeVirt project:

```bash
# Install KVM device plugin (DaemonSet)
kubectl apply -f https://github.com/kubevirt/kubevirt/releases/latest/download/kubevirt-operator.yaml
kubectl apply -f https://github.com/kubevirt/kubevirt/releases/latest/download/kubevirt-cr.yaml
```

Once installed, Pods can request KVM access via resources (already set in `daemonset.yaml` / `deployment.yaml`):

```yaml
resources:
  limits:
    devices.kubevirt.io/kvm: "1"
```

This grants `/dev/kvm` access without `privileged: true` or `hostDevice` mounts.

---

## Persistent storage for snapshots

Zeroboot's `template` command snapshots ~512 MB of VM memory to disk. Without
persistent storage, every Pod restart triggers a ~15 s re-snapshot.

### DaemonSet: hostPath (default)

The default `daemonset.yaml` mounts `/var/lib/zeroboot` directly from the Node:

```yaml
volumes:
  - name: data
    hostPath:
      path: /var/lib/zeroboot
      type: DirectoryOrCreate
```

**Why hostPath?** Firecracker snapshots are bound to the host CPU microarchitecture
and KVM hypervisor state — they cannot be safely moved across nodes or restored
on a different CPU family. Local storage is the semantically correct choice for
this workload. `DirectoryOrCreate` ensures the path is created automatically when
a new node joins the cluster.

**Node drain / failure:** When a node is drained or fails, the Pods on that node
stop. Active sandbox requests (in-flight `v1/exec` calls) will be interrupted and
must be retried by the caller. The Firecracker snapshot remains on the node's disk;
on restart, the Pod reuses the existing snapshot (~2 s startup) rather than
rebuilding from scratch (~19 s). Cross-node snapshot migration is not supported —
this is a deliberate trade-off for simplicity. Node-level autoscaling (Karpenter)
handles capacity; snapshot portability is a future operator-layer enhancement.

### Deployment: PVC (advanced/single-replica)

The alternative `deployment.yaml` uses a PersistentVolumeClaim, appropriate when:
- Running a single replica (PVC `accessMode: ReadWriteOnce`)
- You need HPA or manual replica control

```bash
kubectl apply -f deploy/k8s/pvc.yaml
```

The directory layout on either storage type:

```
/var/lib/zeroboot/
├── vmlinux-fc          ← kernel binary (~21 MB)
├── rootfs-python.ext4  ← base rootfs image (pre-loaded numpy/pandas)
├── python/             ← snapshot created by entrypoint on first boot
│   ├── snapshot/
│   │   ├── vmstate     ← CPU register state (~14 KB)
│   │   └── mem         ← 512 MB memory image (CoW source)
│   └── rootfs_path
└── api_keys.json       ← optional API key list
```

> **Populate the storage before first deploy.** Copy `vmlinux-fc` and
> `rootfs-python.ext4` to each node's `/var/lib/zeroboot` (for DaemonSet) or
> to the PVC (for Deployment) via a one-shot init Job or `kubectl cp`.
> The entrypoint creates the snapshot automatically on first boot if missing.

### Storage class recommendations (Deployment/PVC only)

| Cloud | StorageClass | Notes |
|---|---|---|
| AWS | `gp3` | Default for EKS; good random-read IOPS for CoW page faults |
| GCP | `premium-rwo` | SSD-backed, low latency |
| Azure | `managed-premium` | SSD, required for sub-ms fork performance |

Avoid `gp2` or spinning-disk storage classes — the CoW page fault path is
latency-sensitive and benefits from SSD IOPS.

---

## Deploying

### Option A: DaemonSet (recommended)

Automatically places one zeroboot Pod on every KVM-capable node. New nodes
join the pool automatically with no manual intervention.

```bash
# 1. Create namespace
kubectl apply -f deploy/k8s/namespace.yaml

# 2. Deploy DaemonSet + Service
kubectl apply -f deploy/k8s/daemonset.yaml
kubectl apply -f deploy/k8s/service.yaml

# 3. Watch rollout — first boot takes ~30s for snapshot creation on each node
kubectl rollout status daemonset/zeroboot -n zeroboot

# 4. Verify (one pod per KVM-capable node)
kubectl get pods -n zeroboot -o wide
kubectl exec -n zeroboot ds/zeroboot -- curl -s localhost:8080/v1/health
```

### Option B: Deployment (advanced)

Use when you need HPA or fine-grained replica control. Requires a PVC.

```bash
# 1. Create namespace + PVC
kubectl apply -f deploy/k8s/namespace.yaml
kubectl apply -f deploy/k8s/pvc.yaml

# 2. Deploy
kubectl apply -f deploy/k8s/deployment.yaml
kubectl apply -f deploy/k8s/service.yaml

# 3. Watch rollout
kubectl rollout status deployment/zeroboot -n zeroboot

# 4. Apply HPA (optional, requires prometheus-adapter)
kubectl apply -f deploy/k8s/hpa.yaml
```

---

## Autoscaling

### DaemonSet: node-level autoscaling (recommended)

With DaemonSet, the correct scaling primitive is **adding nodes**, not adding
Pod replicas. Use Karpenter or Cluster Autoscaler to provision new KVM-capable
nodes when load increases:

```
high load → Karpenter adds KVM node → DaemonSet schedules Pod automatically → capacity available
```

Configure Karpenter with a NodePool targeting KVM-capable instance types (see
below). This is the semantically correct scaling model for zeroboot: one Pod
per KVM device, scale by expanding the node pool.

### Deployment: Pod-level HPA (advanced)

If you use `deployment.yaml`, HPA scales Pod replicas across available KVM nodes.
Apply `hpa.yaml` and configure `prometheus-adapter` to expose `zeroboot_concurrent_forks`.

> **Note:** HPA for Deployment combined with `podAntiAffinity` means replicas
> are bounded by the number of KVM-capable nodes. Horizontal scaling beyond node
> count requires node-level scaling anyway — consider using DaemonSet instead.

### Why not CPU-based HPA?

Zeroboot workloads are **memory-bound**, not CPU-bound. Each concurrent fork
adds ~265 KB of CoW memory pressure. CPU utilization is a poor scaling signal.

### Custom metric HPA (Deployment only)

The `zeroboot_concurrent_forks` gauge (exposed at `/v1/metrics`) reflects the
number of active VM sandboxes per Pod. Use this for HPA when running `deployment.yaml`:

```bash
# Apply HPA (requires prometheus-adapter, see below)
kubectl apply -f deploy/k8s/hpa.yaml
```

Scale-out triggers when average concurrent forks per Pod exceeds 800. Adjust
this threshold based on your Node's available memory:

```
max_concurrent_forks ≈ (node_memory - 2GB_overhead) / 265KB_per_fork
# Example: 8GB node → (8192 - 2048) / 0.265 ≈ 23,000 theoretical max
# Practical limit with snapshot RSS: ~1000–2000 per Pod
```

### Exposing the metric via prometheus-adapter

Add to your `prometheus-adapter` ConfigMap:

```yaml
rules:
  - seriesQuery: 'zeroboot_concurrent_forks{namespace!="",pod!=""}'
    resources:
      overrides:
        namespace: {resource: "namespace"}
        pod: {resource: "pod"}
    name:
      matches: "zeroboot_concurrent_forks"
      as: "zeroboot_concurrent_forks"
    metricsQuery: 'avg_over_time(zeroboot_concurrent_forks{<<.LabelMatchers>>}[1m])'
```

### Karpenter node provisioning

For cluster autoscaling with Karpenter, create a NodePool that targets KVM-capable instances:

```yaml
apiVersion: karpenter.sh/v1
kind: NodePool
metadata:
  name: zeroboot-kvm
spec:
  template:
    metadata:
      labels:
        kvm-capable: "true"
    spec:
      requirements:
        - key: karpenter.k8s.aws/instance-family
          operator: In
          values: [c8i, m8i, r8i]  # only these families support nested virt on non-metal sizes
        - key: karpenter.k8s.aws/instance-size
          operator: In
          values: [xlarge, 2xlarge, 4xlarge]
        - key: kubernetes.io/arch
          operator: In
          values: [amd64]
  limits:
    cpu: 100
```

> **Scaling latency note:** Karpenter takes 60–120 s to provision a new KVM
> node (EC2 start + kubelet join + Pod scheduling + snapshot load). Karpenter
> handles **capacity expansion** for sustained load — it is not designed to
> absorb sudden request spikes. Size your warm pool (`minReplicas`) to handle
> peak burst traffic; use HPA to scale within the existing node pool first.

---

## Monitoring

Zeroboot exposes Prometheus metrics at `/v1/metrics` (not `/metrics`).

### ServiceMonitor (Prometheus Operator)

```yaml
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: zeroboot
  namespace: zeroboot
spec:
  selector:
    matchLabels:
      app: zeroboot
  endpoints:
    - port: http
      path: /v1/metrics
      interval: 15s
```

### Key metrics

| Metric | Type | Description |
|---|---|---|
| `zeroboot_concurrent_forks` | gauge | Active VM sandboxes — **use for HPA** |
| `zeroboot_fork_time_milliseconds` | histogram | Fork latency (P50/P99) |
| `zeroboot_exec_time_milliseconds` | histogram | Code execution latency |
| `zeroboot_total_time_milliseconds` | histogram | End-to-end request latency |
| `zeroboot_total_executions{status}` | counter | Success / error / timeout counts |
| `zeroboot_memory_usage_bytes` | gauge | Process RSS — monitor for memory pressure |

---

## Configuration reference

All configuration is via environment variables (set in `daemonset.yaml` / `deployment.yaml`):

| Variable | Default | Description |
|---|---|---|
| `ZEROBOOT_WORKDIR` | `/var/lib/zeroboot` | Working directory (PVC mount point) |
| `ZEROBOOT_KERNEL` | `$WORKDIR/vmlinux-fc` | Path to kernel binary |
| `ZEROBOOT_ROOTFS_PYTHON` | `$WORKDIR/rootfs-python.ext4` | Python rootfs image |
| `ZEROBOOT_ROOTFS_NODE` | _(unset)_ | Node.js rootfs image (optional) |
| `ZEROBOOT_PORT` | `8080` | API server port |
| `ZEROBOOT_TEMPLATE_WAIT` | `15` | Seconds to wait during template snapshot |
| `ZEROBOOT_API_KEYS_FILE` | _(unset)_ | Path to JSON array of API keys |

---

### Server bind address

By default, `zeroboot serve` binds to `0.0.0.0` (all interfaces), which is
required for Kubernetes health probes and Service routing. To restrict to
localhost (e.g. for local development), pass `--bind 127.0.0.1`:

```bash
zeroboot serve python:/workdir/python 8080 --bind 127.0.0.1
```

The `ZEROBOOT_BIND` environment variable (default: `0.0.0.0`) controls the
bind address when running via the Docker entrypoint.


---

## Limitations

- **Snapshot CPU-affinity:** Firecracker snapshots are bound to the host CPU
  microarchitecture. Snapshots cannot be moved between nodes of different instance
  families (e.g., c8i → c6i). Keep KVM-capable nodes homogeneous within a cluster.
- **No cross-node sandbox migration:** Active sandbox state lives in KVM memory
  on the host. When a node drains, in-flight requests are interrupted; callers
  must retry. Cross-node live migration is a future operator-layer enhancement.
- **DaemonSet + hostPath:** Data on `/var/lib/zeroboot` is local to each node.
  On node termination, the snapshot is lost. New nodes rebuild the snapshot on
  first boot (~19 s); subsequent restarts reuse the cached snapshot (~2 s).
- **Deployment + ReadWriteOnce PVC:** Each Pod needs its own PVC. Running multiple
  replicas on the same PVC is not supported. Use DaemonSet for multi-node deployments.
- **Snapshot on first boot:** The first Pod startup after storage initialization
  takes ~15–30 s while the template snapshot is created.
- **x86_64 only:** Firecracker and the guest kernel are x86_64. ARM nodes are
  not supported.
