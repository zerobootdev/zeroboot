use anyhow::{Result, bail};
use kvm_bindings::*;
use kvm_bindings::CpuId;
use kvm_ioctls::{Kvm, VcpuExit, VmFd, VcpuFd};
use std::fs::File;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::serial::Serial;
use super::virtio_blk::{VirtioBlk, VirtioQueueAddrs};
use super::vmstate::SecondaryVcpuState;

extern "C" fn noop_signal_handler(_: libc::c_int) {}

const COM1_PORT: u16 = 0x3f8;
const COM1_PORT_END: u16 = 0x3ff;
const COM1_IRQ: u32 = 4;

pub struct VmSnapshot {
    pub regs: kvm_regs,
    pub sregs: kvm_sregs,
    pub msrs: Vec<kvm_msr_entry>,
    pub lapic: kvm_lapic_state,
    pub ioapic_redirtbl: [u64; 24],
    pub xcrs: kvm_xcrs,
    pub xsave: kvm_xsave,
    pub cpuid_entries: Vec<kvm_cpuid_entry2>,
    pub mem_size: usize,
    pub virtio_queue: Option<VirtioQueueAddrs>,
    pub secondary_vcpus: Vec<SecondaryVcpuState>,
}


pub struct ForkedVm {
    pub vm_fd: VmFd,
    pub vcpu_fd: VcpuFd,
    pub mem_ptr: *mut u8,
    pub mem_size: usize,
    pub serial: Serial,
    pub virtio_blk: Option<VirtioBlk>,
    pub fork_time_us: f64,
    _kvm: Kvm,
}

// Safety: mem_ptr points to a private mmap region owned exclusively by this ForkedVm.
// No other thread can access it without going through ForkedVm methods.
// SAFETY: ForkedVm is never shared across threads without external synchronization.
unsafe impl Send for ForkedVm {}
unsafe impl Sync for ForkedVm {}

impl ForkedVm {
    pub fn fork_cow(snapshot: &VmSnapshot, memfd: i32, block_file: Option<Arc<File>>) -> Result<Self> {
        let start = Instant::now();

        let kvm = Kvm::new()?;
        let vm_fd = kvm.create_vm()?;
        vm_fd.create_irq_chip()?;
        vm_fd.create_pit2(kvm_pit_config::default())?;

        // Restore IOAPIC redirect table from Firecracker snapshot.
        // Use get_irqchip to get KVM's initialized IOAPIC state, then overwrite
        // just the redirect table entries to match the guest kernel's configuration.
        {
            let mut irqchip = kvm_irqchip::default();
            irqchip.chip_id = KVM_IRQCHIP_IOAPIC;
            vm_fd.get_irqchip(&mut irqchip)
                .map_err(|e| anyhow::anyhow!("get_irqchip(IOAPIC): {}", e))?;
            unsafe {
                for i in 0..24 {
                    irqchip.chip.ioapic.redirtbl[i].bits = snapshot.ioapic_redirtbl[i];
                }
            }
            vm_fd.set_irqchip(&irqchip)
                .map_err(|e| anyhow::anyhow!("set_irqchip(IOAPIC): {}", e))?;
        }

        let fork_mem = unsafe {
            libc::mmap(
                ptr::null_mut(), snapshot.mem_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE, memfd, 0,
            )
        };
        if fork_mem == libc::MAP_FAILED {
            bail!("mmap failed: {}", std::io::Error::last_os_error());
        }

        unsafe {
            vm_fd.set_user_memory_region(kvm_userspace_memory_region {
                slot: 0, guest_phys_addr: 0,
                memory_size: snapshot.mem_size as u64,
                userspace_addr: fork_mem as u64, flags: 0,
            })?;
        }

        let vcpu_fd = vm_fd.create_vcpu(0)?;

        // Use Firecracker's exact CPUID from the snapshot, not host defaults.
        // This ensures KVM interprets XSAVE state correctly and the guest sees
        // the same CPU features it saw at boot (matching XSAVE layout, AVX, etc.).
        if !snapshot.cpuid_entries.is_empty() {
            let cpuid = CpuId::from_entries(&snapshot.cpuid_entries)
                .map_err(|e| anyhow::anyhow!("CpuId::from_entries: {:?}", e))?;
            match vcpu_fd.set_cpuid2(&cpuid) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("  Warning: snapshot CPUID rejected ({}), using host CPUID", e);
                    let cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
                    vcpu_fd.set_cpuid2(&cpuid)
                        .map_err(|e| anyhow::anyhow!("set_cpuid2 (host fallback): {}", e))?;
                }
            }
        } else {
            let cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
            vcpu_fd.set_cpuid2(&cpuid)
                .map_err(|e| anyhow::anyhow!("set_cpuid2 (host): {}", e))?;
        }

        vcpu_fd.set_sregs(&snapshot.sregs)
            .map_err(|e| anyhow::anyhow!("set_sregs: {}", e))?;

        // Restore XCRS (must be after sregs — CR4.OSXSAVE must be set first)
        if snapshot.xcrs.nr_xcrs > 0 {
            vcpu_fd.set_xcrs(&snapshot.xcrs)
                .map_err(|e| anyhow::anyhow!("set_xcrs: {}", e))?;
        }

        // Restore XSAVE (FPU/SSE/AVX state — must be after XCRS)
        vcpu_fd.set_xsave(&snapshot.xsave)
            .map_err(|e| anyhow::anyhow!("set_xsave: {}", e))?;

        vcpu_fd.set_regs(&snapshot.regs)
            .map_err(|e| anyhow::anyhow!("set_regs: {}", e))?;

        // Restore LAPIC
        let _ = vcpu_fd.set_lapic(&snapshot.lapic);

        // Restore MSRs (kvm-clock, syscall, etc.)
        if !snapshot.msrs.is_empty() {
            let mut msrs = Msrs::new(snapshot.msrs.len()).unwrap();
            for (i, entry) in snapshot.msrs.iter().enumerate() {
                let mut e = *entry;
                if e.index == 0x4b564d00 { e.data |= 1; }
                msrs.as_mut_slice()[i] = e;
            }
            let _ = vcpu_fd.set_msrs(&msrs);
        }

        // MUST be last: set MP state to RUNNABLE so vCPU isn't stuck in HLT
        let _ = vcpu_fd.set_mp_state(kvm_mp_state { mp_state: 0 });

        // Create and restore secondary vCPUs, running them in background threads
        for (i, sec) in snapshot.secondary_vcpus.iter().enumerate() {
            let vcpu_id = (i + 1) as u64;
            let mut sec_fd = vm_fd.create_vcpu(vcpu_id)
                .map_err(|e| anyhow::anyhow!("create_vcpu({}): {}", vcpu_id, e))?;

            // Use host CPUID for secondary vCPUs (same as primary fallback)
            let cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
            let _ = sec_fd.set_cpuid2(&cpuid);

            let _ = sec_fd.set_sregs(&sec.sregs);
            if sec.xcrs.nr_xcrs > 0 {
                let _ = sec_fd.set_xcrs(&sec.xcrs);
            }
            let _ = sec_fd.set_xsave(&sec.xsave);
            let _ = sec_fd.set_regs(&sec.regs);
            let _ = sec_fd.set_lapic(&sec.lapic);

            // Restore MSRs on secondary vCPU too
            if !snapshot.msrs.is_empty() {
                let mut msrs = Msrs::new(snapshot.msrs.len()).unwrap();
                for (j, entry) in snapshot.msrs.iter().enumerate() {
                    msrs.as_mut_slice()[j] = *entry;
                }
                let _ = sec_fd.set_msrs(&msrs);
            }

            let _ = sec_fd.set_mp_state(kvm_mp_state { mp_state: 0 });

            // Run secondary vCPU in background thread.
            // It will mostly idle (HLT) and wake for timer interrupts/IPIs.
            std::thread::spawn(move || {
                loop {
                    match sec_fd.run() {
                        Ok(VcpuExit::IoIn(_, data)) => { data[0] = 0xff; }
                        Ok(VcpuExit::IoOut(_, _)) => {}
                        Ok(VcpuExit::MmioRead(_, data)) => {
                            for b in data.iter_mut() { *b = 0xff; }
                        }
                        Ok(VcpuExit::MmioWrite(_, _)) => {}
                        Ok(VcpuExit::Hlt) => {} // idle; KVM will wake on interrupt
                        Ok(VcpuExit::Shutdown) => break,
                        Ok(VcpuExit::InternalError) => break,
                        Err(e) if e.errno() == libc::EAGAIN => {}
                        Err(e) if e.errno() == libc::EINTR => {}
                        Err(_) => break,
                        _ => {}
                    }
                }
            });
        }

        // Create virtio-blk emulator if queue addresses and block device are available
        let virtio_blk = match (&snapshot.virtio_queue, block_file) {
            (Some(queue), Some(file)) => {
                let mut blk = VirtioBlk::new(queue.clone(), file);
                blk.init_from_guest_memory(fork_mem as *const u8, snapshot.mem_size);
                Some(blk)
            }
            _ => None,
        };

        Ok(Self {
            _kvm: kvm, vm_fd, vcpu_fd,
            mem_ptr: fork_mem as *mut u8, mem_size: snapshot.mem_size,
            serial: Serial::new(),
            virtio_blk,
            fork_time_us: start.elapsed().as_secs_f64() * 1_000_000.0,
        })
    }

    pub fn inject_serial_irq(&self) -> Result<()> {
        self.vm_fd.set_irq_line(COM1_IRQ, true).map_err(|e| anyhow::anyhow!("{}", e))?;
        self.vm_fd.set_irq_line(COM1_IRQ, false).map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    pub fn send_serial(&mut self, data: &[u8]) -> Result<()> {
        self.serial.queue_input(data);
        self.serial.set_ier_data_ready(true);
        self.inject_serial_irq()?;
        Ok(())
    }

    pub fn run_until_marker(&mut self, marker: &str, max_exits: u64) -> Result<String> {
        self.run_until_marker_timeout(marker, max_exits, None)
    }

    pub fn run_until_marker_timeout(&mut self, marker: &str, max_exits: u64, timeout: Option<Duration>) -> Result<String> {
        if self.serial.has_pending_input() {
            self.serial.set_ier_data_ready(true);
            let _ = self.inject_serial_irq();
        }

        let debug = std::env::var("ZEROBOOT_DEBUG").is_ok();
        let deadline = timeout.map(|d| Instant::now() + d);

        // Set up a per-thread timer to interrupt KVM_RUN via signal.
        // KVM_RUN blocks in kernel mode; the only way to break out is a signal
        // that causes the ioctl to return EINTR.
        let _timer_guard = if let Some(dur) = timeout {
            // Install no-op SIGALRM handler (SA_RESTART=false so ioctl gets EINTR)
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = noop_signal_handler as *const () as usize;
                sa.sa_flags = 0; // no SA_RESTART
                libc::sigaction(libc::SIGALRM, &sa, std::ptr::null_mut());
            }
            // Create a per-thread timer using CLOCK_THREAD_CPUTIME_ID won't work
            // across threads, so use a helper thread that sleeps then signals us.
            // Cast pthread_t to usize so the closure is Send (pthread_t is *mut c_void on musl)
            let thread_id = unsafe { libc::pthread_self() } as usize;
            let handle = std::thread::spawn(move || {
                std::thread::sleep(dur);
                unsafe { libc::pthread_kill(thread_id as libc::pthread_t, libc::SIGALRM); }
            });
            Some(handle)
        } else {
            None
        };

        let mut exit_count: u64 = 0;
        let mut io_in_count: u64 = 0;
        let mut io_out_count: u64 = 0;
        let mut hlt_count: u64 = 0;
        let mut _last_io_in_offset: u16 = 0;
        let mut _last_io_in_val: u8 = 0;

        for _ in 0..max_exits {
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    bail!("Execution timed out");
                }
            }
            exit_count += 1;
            match self.vcpu_fd.run() {
                Ok(exit) => match exit {
                    VcpuExit::IoOut(port, data) => {
                        if port >= COM1_PORT && port <= COM1_PORT_END {
                            io_out_count += 1;
                            let offset = port - COM1_PORT;
                            let _thri_before = self.serial.thri_enabled();
                            for &b in data { self.serial.write(offset, b); }
                            // Level-triggered THRI: keep IRQ line asserted while
                            // THRI is enabled and de-assert when disabled. This
                            // matches real UART behavior where THRE interrupt
                            // stays pending until acknowledged by disabling THRI.
                            if offset == 1 {
                                if self.serial.thri_enabled() {
                                    let _ = self.vm_fd.set_irq_line(COM1_IRQ, true);
                                } else {
                                    let _ = self.vm_fd.set_irq_line(COM1_IRQ, false);
                                }
                            }
                            if debug && offset == 0 && exit_count < 5000 {
                                let ch = data[0];
                                if ch >= 0x20 && ch < 0x7f {
                                    eprintln!("  [{}] IoOut chr='{}' out_len={}", exit_count, ch as char, self.serial.output.len());
                                } else {
                                    eprintln!("  [{}] IoOut byte=0x{:02x} out_len={}", exit_count, ch, self.serial.output.len());
                                }
                            } else if debug && exit_count < 5000 {
                                eprintln!("  [{}] IoOut reg={} val=0x{:02x}", exit_count, offset, data[0]);
                            }
                            if String::from_utf8_lossy(&self.serial.output).contains(marker) {
                                if debug { eprintln!("  MARKER FOUND at exit {}", exit_count); }
                                return Ok(String::from_utf8_lossy(&self.serial.output).to_string());
                            }
                        }
                    }
                    VcpuExit::IoIn(port, data) => {
                        if port >= COM1_PORT && port <= COM1_PORT_END {
                            let offset = port - COM1_PORT;
                            data[0] = self.serial.read(offset);
                            io_in_count += 1;
                            _last_io_in_offset = offset;
                            _last_io_in_val = data[0];
                            if debug && exit_count < 5000 {
                                if offset == 0 {
                                    let ch = data[0];
                                    if ch >= 0x20 && ch < 0x7f {
                                        eprintln!("  [{}] IoIn  RX chr='{}' pending={}", exit_count, ch as char, self.serial.input_len());
                                    } else {
                                        eprintln!("  [{}] IoIn  RX byte=0x{:02x} pending={}", exit_count, ch, self.serial.input_len());
                                    }
                                } else if offset == 2 {
                                    eprintln!("  [{}] IoIn  IIR=0x{:02x}", exit_count, data[0]);
                                } else if offset == 5 {
                                    eprintln!("  [{}] IoIn  LSR=0x{:02x}", exit_count, data[0]);
                                }
                            }
                        } else {
                            data[0] = 0xff;
                        }
                    }
                    VcpuExit::Hlt => {
                        hlt_count += 1;
                        if debug { eprintln!("  [{}] HLT pending={}", exit_count, self.serial.has_pending_input()); }
                        if self.serial.has_pending_input() {
                            let _ = self.inject_serial_irq();
                        } else {
                            if debug { eprintln!("  DONE: exits={} io_in={} io_out={} hlt={}", exit_count, io_in_count, io_out_count, hlt_count); }
                            return Ok(String::from_utf8_lossy(&self.serial.output).to_string());
                        }
                    }
                    VcpuExit::Shutdown => {
                        return Ok(String::from_utf8_lossy(&self.serial.output).to_string());
                    }
                    VcpuExit::MmioRead(addr, data) => {
                        match &self.virtio_blk {
                            Some(blk) if blk.handles_mmio(addr) => blk.mmio_read(addr, data),
                            _ => { for b in data.iter_mut() { *b = 0xff; } }
                        }
                    }
                    VcpuExit::MmioWrite(addr, data) => {
                        if let Some(ref mut blk) = self.virtio_blk {
                            if blk.handles_mmio(addr) {
                                blk.mmio_write(addr, data, self.mem_ptr, self.mem_size, &self.vm_fd);
                            }
                        }
                    }
                    VcpuExit::InternalError => bail!("KVM internal error"),
                    _ => {}
                },
                Err(e) => {
                    if e.errno() == libc::EAGAIN { continue; }
                    if e.errno() == libc::EINTR {
                        // Signal interrupted KVM_RUN — check if we've timed out
                        if let Some(dl) = deadline {
                            if Instant::now() >= dl {
                                bail!("Execution timed out");
                            }
                        }
                        continue;
                    }
                    bail!("KVM run: {} (errno {})", e, e.errno());
                }
            }
        }
        Ok(String::from_utf8_lossy(&self.serial.output).to_string())
    }
}

impl Drop for ForkedVm {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.mem_ptr as *mut libc::c_void, self.mem_size); }
    }
}


/// Create a snapshot memfd by copying directly from a file using copy_file_range.
/// This avoids allocating a user-space buffer for the entire snapshot memory,
/// keeping peak RSS low (kernel-to-kernel copy).
pub fn create_snapshot_memfd_from_file(src_file: &File, mem_size: usize) -> Result<i32> {
    use std::os::unix::io::AsRawFd;

    let name = std::ffi::CString::new("zeroboot-snapshot").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 { bail!("memfd_create failed"); }
    if unsafe { libc::ftruncate(fd, mem_size as i64) } < 0 {
        unsafe { libc::close(fd); }
        bail!("ftruncate failed");
    }

    let src_fd = src_file.as_raw_fd();
    let mut offset: i64 = 0;
    let mut remaining = mem_size;
    while remaining > 0 {
        let chunk = remaining.min(64 * 1024 * 1024); // 64MB chunks
        let n = unsafe {
            libc::sendfile(fd, src_fd, &mut offset, chunk)
        };
        if n <= 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd); }
            bail!("sendfile failed: {}", err);
        }
        remaining -= n as usize;
    }

    Ok(fd)
}
