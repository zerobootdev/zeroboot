mod api;
mod vmm;

use anyhow::{bail, Result};
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use api::handlers::{
    batch_handler, exec_handler, health_handler, metrics_handler, AppState, Metrics, Template,
};
use vmm::firecracker;
use vmm::kvm::{create_snapshot_memfd, ForkedVm, VmSnapshot};
use vmm::vmstate;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "template" => cmd_template(&args[2..]),
        "bench" | "fork-bench" => cmd_fork_bench(&args[2..]),
        "serve" => cmd_serve(&args[2..]),
        "test-exec" => cmd_test_exec(&args[2..]),
        _ => {
            eprintln!("Usage: zeroboot <command>");
            eprintln!("  template <kernel> <rootfs> <workdir>  - Boot & snapshot a template VM");
            eprintln!("  bench <workdir>                       - Run fork benchmarks");
            eprintln!(
                "  test-exec <workdir> <command>         - Test executing a command in a fork"
            );
            eprintln!("  serve <workdir> [port] [--bind addr]  - Start API server (default bind: 0.0.0.0)");
            Ok(())
        }
    }
}

fn load_snapshot(workdir: &str) -> Result<(VmSnapshot, i32)> {
    let mem_path = format!("{}/snapshot/mem", workdir);
    let state_path = format!("{}/snapshot/vmstate", workdir);

    eprintln!("Loading snapshot from {}...", workdir);

    // Load memory
    let mem_data = std::fs::read(&mem_path)?;
    let mem_size = mem_data.len();
    eprintln!("  Memory: {} MiB", mem_size / 1024 / 1024);

    // Create memfd for CoW
    let memfd = create_snapshot_memfd(mem_data.as_ptr(), mem_size)?;
    drop(mem_data);

    // Load vmstate for CPU registers
    let state_data = std::fs::read(&state_path)?;
    let parsed = vmstate::parse_vmstate(&state_data)?;
    eprintln!(
        "  CPU state loaded: RIP={:#x}, RSP={:#x}, CR3={:#x}",
        parsed.regs.rip, parsed.regs.rsp, parsed.sregs.cr3
    );
    eprintln!("  MSRs: {} entries", parsed.msrs.len());

    eprintln!(
        "  CPUID: {} entries from Firecracker snapshot",
        parsed.cpuid_entries.len()
    );

    let snapshot = VmSnapshot {
        regs: parsed.regs,
        sregs: parsed.sregs,
        msrs: parsed.msrs,
        lapic: parsed.lapic,
        ioapic_redirtbl: parsed.ioapic_redirtbl,
        xcrs: parsed.xcrs,
        xsave: parsed.xsave,
        cpuid_entries: parsed.cpuid_entries,
        mem_size,
    };

    Ok((snapshot, memfd))
}

fn cmd_template(args: &[String]) -> Result<()> {
    if args.len() < 3 {
        bail!("Usage: zeroboot template <kernel> <rootfs> <workdir> [wait_secs] [init_path]");
    }
    let kernel = &args[0];
    let rootfs = &args[1];
    let workdir = &args[2];
    let wait_secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);
    let init_path = args.get(4).map(|s| s.as_str()).unwrap_or("/init.py");

    std::fs::create_dir_all(workdir)?;

    let start = Instant::now();
    let mem_mib: u32 = args
        .get(5)
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            std::env::var("ZEROBOOT_MEM_MIB")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(512);
    let (state_path, mem_path, mem_mib) = firecracker::create_template_snapshot(
        kernel, rootfs, workdir, mem_mib, wait_secs, init_path,
    )?;
    let elapsed = start.elapsed();

    println!("Template created in {:.2}s", elapsed.as_secs_f64());
    println!("  State: {}", state_path);
    println!("  Memory: {} ({} MiB)", mem_path, mem_mib);

    Ok(())
}

fn cmd_test_exec(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        bail!("Usage: zeroboot test-exec <workdir> <command>");
    }
    let workdir = &args[0];
    let command = args[1..].join(" ");

    let (snapshot, memfd) = load_snapshot(workdir)?;

    eprintln!("Forking VM...");
    let fork_start = Instant::now();
    let mut vm = ForkedVm::fork_cow(&snapshot, memfd)?;
    eprintln!("  Fork time: {:.1}µs", vm.fork_time_us);

    // Send command to guest
    eprintln!("Sending command: {}", command);
    let cmd_str = format!("{}\n", command);
    if let Err(e) = vm.send_serial(cmd_str.as_bytes()) {
        eprintln!("  send_serial failed: {}", e);
    }

    // Run and collect output
    let exec_start = Instant::now();
    let output = vm.run_until_marker("ZEROBOOT_DONE", 500_000_000)?;
    let exec_time = exec_start.elapsed();

    let total_time = fork_start.elapsed();
    eprintln!("  Exec time: {:.2}ms", exec_time.as_secs_f64() * 1000.0);
    eprintln!("  Total time: {:.2}ms", total_time.as_secs_f64() * 1000.0);

    println!("=== Output ===");
    println!("{}", output);

    unsafe {
        libc::close(memfd);
    }
    Ok(())
}

fn cmd_fork_bench(args: &[String]) -> Result<()> {
    if args.len() < 1 {
        bail!("Usage: zeroboot bench <workdir>");
    }
    let workdir = &args[0];
    let (snapshot, memfd) = load_snapshot(workdir)?;
    let mem_size = snapshot.mem_size;

    eprintln!("\n=== Zeroboot Fork Benchmark ===\n");

    // Phase 1: Pure mmap CoW
    let mut mmap_times: Vec<f64> = Vec::with_capacity(10000);
    for _ in 0..100 {
        // warmup
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mem_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                memfd,
                0,
            )
        };
        if p != libc::MAP_FAILED {
            unsafe {
                libc::munmap(p, mem_size);
            }
        }
    }
    for _ in 0..10000 {
        let start = Instant::now();
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mem_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                memfd,
                0,
            )
        };
        mmap_times.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        if p != libc::MAP_FAILED {
            unsafe {
                libc::munmap(p, mem_size);
            }
        }
    }
    mmap_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    print_percentiles("Pure mmap CoW", &mmap_times);

    // Phase 2: Full fork with CPU state restore
    eprintln!();
    let mut fork_times: Vec<f64> = Vec::with_capacity(1000);
    // warmup
    for _ in 0..20 {
        let vm = ForkedVm::fork_cow(&snapshot, memfd)?;
        drop(vm);
    }
    for _ in 0..1000 {
        let start = Instant::now();
        let vm = ForkedVm::fork_cow(&snapshot, memfd)?;
        fork_times.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        drop(vm);
    }
    fork_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    print_percentiles("Full fork (KVM + CoW + CPU restore)", &fork_times);

    // Phase 3: Fork + execute echo (measures end-to-end latency)
    eprintln!();
    eprintln!("Phase 3: Fork + execute 'echo hello' (100 iterations)...");
    let mut exec_times: Vec<f64> = Vec::with_capacity(100);
    let mut success_count = 0;

    for i in 0..100 {
        let start = Instant::now();
        let mut vm = ForkedVm::fork_cow(&snapshot, memfd)?;
        let _ = vm.send_serial(b"echo hello\n");
        let output = vm.run_until_marker("hello", 100_000_000)?;
        let t = start.elapsed().as_secs_f64() * 1000.0;
        exec_times.push(t);
        if output.contains("hello") {
            success_count += 1;
        } else if i == 0 {
            eprintln!(
                "  Warning: output doesn't contain 'hello': {}",
                &output[..output.len().min(200)]
            );
        }
        drop(vm);
    }
    exec_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("  Fork + exec echo ({}/100 successful):", success_count);
    if !exec_times.is_empty() {
        let n = exec_times.len();
        println!("    P50:  {:>8.3} ms", exec_times[n / 2]);
        println!("    P95:  {:>8.3} ms", exec_times[n * 95 / 100]);
        println!("    P99:  {:>8.3} ms", exec_times[n * 99 / 100]);
    }

    // Phase 4: Concurrent forks
    eprintln!();
    eprintln!("Phase 4: Concurrent fork test");
    for count in &[10usize, 100, 1000] {
        let start = Instant::now();
        let mut vms: Vec<ForkedVm> = Vec::with_capacity(*count);
        for _ in 0..*count {
            vms.push(ForkedVm::fork_cow(&snapshot, memfd)?);
        }
        let total = start.elapsed();
        let rss_kb = get_rss_kb();
        println!(
            "  {} concurrent: {:.1}ms total, {:.1}µs/fork, RSS: {:.1}MB ({:.1}KB/fork)",
            count,
            total.as_secs_f64() * 1000.0,
            total.as_secs_f64() * 1_000_000.0 / *count as f64,
            rss_kb as f64 / 1024.0,
            rss_kb as f64 / *count as f64
        );
        drop(vms);
    }

    // Phase 5: Isolation test
    eprintln!();
    eprintln!("Phase 5: Memory isolation test");
    {
        let secret: u64 = 0xDEADBEEF_CAFEBABE;
        let offset: usize = 0x50000;

        let fork_a = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mem_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                memfd,
                0,
            )
        };
        unsafe {
            *(fork_a.add(offset) as *mut u64) = secret;
        }

        let fork_b = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mem_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                memfd,
                0,
            )
        };
        let b_val = unsafe { *(fork_b.add(offset) as *const u64) };

        if b_val == secret {
            println!("  FAIL: Isolation broken!");
        } else {
            println!(
                "  PASS: Isolation verified (Fork B reads {:#x}, not secret {:#x})",
                b_val, secret
            );
        }

        unsafe {
            *(fork_b.add(offset) as *mut u64) = 0x1111111111111111;
            let a_val = *(fork_a.add(offset) as *const u64);
            if a_val == secret {
                println!("  PASS: Bidirectional isolation OK");
            } else {
                println!("  FAIL: Fork A corrupted after Fork B write");
            }
            libc::munmap(fork_a, mem_size);
            libc::munmap(fork_b, mem_size);
        }
    }

    // Summary
    let p50 = fork_times[fork_times.len() / 2];
    let p99 = fork_times[fork_times.len() * 99 / 100];
    let p50_ms = p50 / 1000.0;
    let p99_ms = p99 / 1000.0;

    println!();
    println!("=== Comparison Table ===");
    println!(
        "| {:20} | {:>12} | {:>12} | {:>12} | {:>12} |",
        "Metric", "Zeroboot", "E2B", "microsandbox", "Daytona"
    );
    println!(
        "|{:-<22}|{:-<14}|{:-<14}|{:-<14}|{:-<14}|",
        "", "", "", "", ""
    );
    println!(
        "| {:20} | {:>9.3}ms | {:>9}ms | {:>9}ms | {:>9}ms |",
        "Spawn latency p50", p50_ms, "~150", "~200", "~27"
    );
    println!(
        "| {:20} | {:>9.3}ms | {:>9}ms | {:>9}ms | {:>9}ms |",
        "Spawn latency p99", p99_ms, "~300", "~400", "~90"
    );
    println!(
        "| {:20} | {:>9}KB | {:>9}MB | {:>9}MB | {:>9}MB |",
        "Memory per sandbox", "~265", "~128", "~50", "~50"
    );
    println!(
        "| {:20} | {:>9} | {:>9} | {:>9} | {:>9} |",
        "Max concurrent", "1000+", "~100", "~100", "~1000"
    );

    let speedup = 27.0 / p50_ms;
    println!();
    println!("Speedup vs Daytona: {:.0}x faster", speedup);
    if p50_ms < 1.0 {
        println!("*** SUB-MILLISECOND SPAWN ACHIEVED! ***");
    }

    unsafe {
        libc::close(memfd);
    }
    Ok(())
}

fn load_api_keys() -> Vec<String> {
    let path =
        std::env::var("ZEROBOOT_API_KEYS_FILE").unwrap_or_else(|_| "api_keys.json".to_string());
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str::<Vec<String>>(&data).unwrap_or_default(),
        Err(_) => {
            eprintln!("  No API keys file ({}), auth disabled", path);
            Vec::new()
        }
    }
}

fn cmd_serve(args: &[String]) -> Result<()> {
    if args.len() < 1 {
        bail!("Usage: zeroboot serve <workdir>[,lang:workdir2,...] [port] [--bind <addr>]");
    }
    let port: u16 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(8080);

    // Parse optional --bind flag (default 0.0.0.0 for container/K8s compatibility).
    // K8s health probes connect from the Pod IP (not localhost), and Service ClusterIP
    // routing requires the server to listen on all interfaces.
    // For local dev, use: --bind 127.0.0.1
    let bind_addr = {
        let mut addr = "0.0.0.0".to_string();
        let mut i = 2;
        while i + 1 < args.len() {
            if args[i] == "--bind" {
                addr = args[i + 1].clone();
                break;
            }
            i += 1;
        }
        addr
    };

    // Parse workdir specs: "workdir" or "python:workdir1,node:workdir2"
    let mut templates = std::collections::HashMap::new();
    for spec in args[0].split(',') {
        let (lang, dir) = if let Some((l, d)) = spec.split_once(':') {
            (l.to_string(), d.to_string())
        } else {
            ("python".to_string(), spec.to_string())
        };
        let (snapshot, memfd) = load_snapshot(&dir)?;
        eprintln!("  Template '{}' loaded from {}", lang, dir);
        templates.insert(lang, Template { snapshot, memfd });
    }

    let api_keys = load_api_keys();
    if !api_keys.is_empty() {
        eprintln!("  API keys loaded: {}", api_keys.len());
    }

    let state = Arc::new(AppState {
        templates,
        api_keys,
        rate_limiters: std::sync::Mutex::new(std::collections::HashMap::new()),
        metrics: Metrics::new(),
    });

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let app = axum::Router::new()
            .route("/exec", axum::routing::post(exec_handler.clone()))
            .route("/v1/exec", axum::routing::post(exec_handler))
            .route("/v1/exec/batch", axum::routing::post(batch_handler))
            .route("/health", axum::routing::get(health_handler.clone()))
            .route("/v1/health", axum::routing::get(health_handler))
            .route("/v1/metrics", axum::routing::get(metrics_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(format!("{}:{}", bind_addr, port))
            .await
            .unwrap();
        eprintln!("Zeroboot API server listening on {}:{}", bind_addr, port);
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
        eprintln!("Server shutdown complete");
    });

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.ok();
    eprintln!("Received SIGINT, shutting down gracefully...");
}

fn print_percentiles(label: &str, times: &[f64]) {
    let n = times.len();
    println!("  {} ({} iterations):", label, n);
    println!(
        "    Min:  {:>8.1} µs ({:.3} ms)",
        times[0],
        times[0] / 1000.0
    );
    println!(
        "    Avg:  {:>8.1} µs ({:.3} ms)",
        times.iter().sum::<f64>() / n as f64,
        times.iter().sum::<f64>() / n as f64 / 1000.0
    );
    println!(
        "    P50:  {:>8.1} µs ({:.3} ms)",
        times[n / 2],
        times[n / 2] / 1000.0
    );
    println!(
        "    P95:  {:>8.1} µs ({:.3} ms)",
        times[n * 95 / 100],
        times[n * 95 / 100] / 1000.0
    );
    println!(
        "    P99:  {:>8.1} µs ({:.3} ms)",
        times[n * 99 / 100],
        times[n * 99 / 100] / 1000.0
    );
    println!(
        "    Max:  {:>8.1} µs ({:.3} ms)",
        times[n - 1],
        times[n - 1] / 1000.0
    );
}

fn get_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        })
        .unwrap_or(0)
}

#[allow(dead_code)]
fn debug_sizes() {
    eprintln!(
        "kvm_ioapic_state: {}",
        std::mem::size_of::<kvm_bindings::kvm_ioapic_state>()
    );
    eprintln!(
        "kvm_irqchip: {}",
        std::mem::size_of::<kvm_bindings::kvm_irqchip>()
    );
    eprintln!(
        "redirtbl entry: {}",
        std::mem::size_of::<kvm_bindings::kvm_ioapic_state__bindgen_ty_1>()
    );
}
