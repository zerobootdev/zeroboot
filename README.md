<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/logo-light.svg">
    <img alt="Zeroboot" src="assets/logo-light.svg" width="500">
  </picture>
</p>

<p align="center">
  <strong>Sub-millisecond VM sandboxes for AI agents via copy-on-write forking</strong>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-2021_edition-orange" alt="Rust"></a>
  <a href="https://api.zeroboot.dev/v1/health"><img src="https://img.shields.io/badge/api-live-brightgreen" alt="API Status"></a>
</p>

---

![demo](demo/demo.gif)

## Try it

```bash
curl -X POST https://api.zeroboot.dev/v1/exec \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer zb_demo_hn2026' \
  -d '{"code":"import numpy as np; print(np.random.rand(3))"}'
```

## Benchmarks

| Metric | Zeroboot | E2B | microsandbox | Daytona |
|---|---|---|---|---|
| Spawn latency p50 | **0.79ms** | ~150ms | ~200ms | ~27ms |
| Spawn latency p99 | 1.74ms | ~300ms | ~400ms | ~90ms |
| Memory per sandbox | ~265KB | ~128MB | ~50MB | ~50MB |
| Fork + exec (Python) | **~8ms** | - | - | - |
| 1000 concurrent forks | 815ms | - | - | - |

Each sandbox is a real KVM virtual machine with hardware-enforced memory isolation.

## How it works

```
  Firecracker snapshot ──► mmap(MAP_PRIVATE) ──► KVM VM + restored CPU state
                              (copy-on-write)         (~0.8ms)
```

1. **Template** (one-time): Firecracker boots a VM, pre-loads your runtime, and snapshots memory + CPU state
2. **Fork** (~0.8ms): Creates a new KVM VM, maps snapshot memory as CoW, restores all CPU state
3. **Isolation**: Each fork is a separate KVM VM with hardware-enforced memory isolation

## SDKs

**Python** &mdash; [sdk/python](sdk/python/)

```python
from zeroboot import Sandbox
sb = Sandbox("zb_live_your_key")
result = sb.run("print(1 + 1)")
```

**TypeScript** &mdash; [sdk/node](sdk/node/)

```typescript
import { Sandbox } from "@zeroboot/sdk";
const result = await new Sandbox("zb_live_your_key").run("console.log(1+1)");
```

## Docs

- [API Reference](docs/API.md)
- [Deployment Guide](docs/DEPLOYMENT.md)
- [Architecture](docs/ARCHITECTURE.md)

## Author

Zeroboot was created in March 2026 and is maintained by **Adam Miribyan** ([@adammiribyan](https://github.com/adammiribyan)). Project site: [zeroboot.dev](https://zeroboot.dev).

If you use zeroboot in research, please cite it; see [CITATION.cff](CITATION.cff) or use the "Cite this repository" button.

## Cited by

- *DeltaBox: Scaling Stateful AI Agents with Millisecond-Level Sandbox Checkpoint/Rollback*, arXiv:2605.22781 (May 2026).
- *Memory Compression for High-Fanout Agent Sandboxes* (AgentZip), arXiv:2609.11294 (September 2026).

If your paper or project uses zeroboot, open an issue or pull request to add it here.

## Status

Working prototype. The fork primitive, benchmarks, and API are real, but not production-hardened yet. [Open an issue](https://github.com/zerobootdev/zeroboot/issues) if you're interested.

## Self-host or managed

Zeroboot is open source. Self-host it on any Linux box with KVM, or use the managed API:

    curl -X POST https://api.zeroboot.dev/v1/exec \
      -H 'Content-Type: application/json' \
      -H 'Authorization: Bearer zb_demo_hn2026' \
      -d '{"code":"import numpy as np; print(np.random.rand(3))"}'

Building the managed service for teams that don't want to run their own infra. Sign up for early access: https://tally.so/r/aQGkpb

## Known limitations

- Forks share CSPRNG state from the snapshot. Kernel entropy is reseeded via RNDADDENTROPY but userspace PRNGs (numpy, OpenSSL) need explicit reseeding per fork. See [Firecracker's guidance](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/random-for-clones.md).
- Single vCPU per fork. Multi-vCPU is architecturally possible but not implemented.
- No networking inside forks. Sandboxes communicate via serial I/O only.
- Template updates require a full re-snapshot (~15s). No incremental patching.

## License

[Apache-2.0](LICENSE)
