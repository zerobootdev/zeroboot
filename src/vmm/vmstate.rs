use anyhow::{bail, Result};
use kvm_bindings::*;

use super::virtio_blk::VirtioQueueAddrs;

// Intra-kvm_sregs offsets relative to EFER position.
// These are determined by the kernel's kvm_sregs struct layout and do not change
// between Firecracker versions:
//   segments (8 * 24 = 192) + gdt (16) + idt (16) + cr0..cr4 (32) + cr8 (8) = 264 to EFER
const EFER_TO_SEGS: usize = 264;
const EFER_TO_GDT: usize = 72;
const EFER_TO_IDT: usize = 56;
const EFER_TO_CR: usize = 40;

pub struct SecondaryVcpuState {
    pub regs: kvm_regs,
    pub sregs: kvm_sregs,
    pub lapic: kvm_lapic_state,
    pub xcrs: kvm_xcrs,
    pub xsave: kvm_xsave,
}

pub struct ParsedVmState {
    pub regs: kvm_regs,
    pub sregs: kvm_sregs,
    pub msrs: Vec<kvm_msr_entry>,
    pub lapic: kvm_lapic_state,
    pub ioapic_redirtbl: [u64; 24],
    pub xcrs: kvm_xcrs,
    pub xsave: kvm_xsave,
    pub cpuid_entries: Vec<kvm_cpuid_entry2>,
    pub virtio_queue: Option<VirtioQueueAddrs>,
    pub secondary_vcpus: Vec<SecondaryVcpuState>,
}

/// Find the EFER offset for the first vCPU.
/// EFER is the primary anchor: we search for known EFER values (0xD01 = SCE|LME|LMA|NXE)
/// and validate by checking CR0 at offset -40 and APIC_BASE at offset +8 within kvm_sregs.
fn find_efer(data: &[u8]) -> Result<usize> {
    for &efer_val in &[0xD01u64, 0x501, 0xD00, 0x500] {
        for i in EFER_TO_SEGS..data.len().saturating_sub(16) {
            if r64(data, i) != efer_val {
                continue;
            }
            // CR0 at efer - 40: must have PE (bit 0) and PG (bit 31), fit in 32 bits
            let cr0 = r64(data, i - EFER_TO_CR);
            if cr0 & 0x80000001 != 0x80000001 || cr0 >> 32 != 0 {
                continue;
            }
            // APIC_BASE at efer + 8: must be 0xFEE00xxx
            let apic = r64(data, i + 8);
            if apic & 0xFFFFF000 == 0xFEE00000 {
                return Ok(i);
            }
        }
    }
    bail!("cannot find EFER in vmstate");
}

/// Find the first vCPU's kvm_regs by scanning backward from the segments block.
/// kvm_regs blocks (one per vCPU) are stored contiguously, followed by a
/// versionize header (variable size), then the kvm_sregs blocks. We find the
/// last regs block's rflags, then walk backward to find the first vCPU.
fn find_first_regs(data: &[u8], segs_off: usize) -> Result<usize> {
    // Scan backward from segs for rflags (last field of kvm_regs, offset 136).
    // The gap between regs end and segs start varies by Firecracker version.
    let mut last_regs_rflags = None;
    let scan_start = segs_off.saturating_sub(64);
    let scan_end = segs_off.saturating_sub(8);
    for pos in (scan_start..scan_end).rev() {
        if pos + 8 > data.len() {
            continue;
        }
        let val = r64(data, pos);
        // Valid rflags: bit 1 always set, reserved bits clear, reasonable value
        if val & 0x2 != 0 && val > 0 && val < 0x400000 {
            // Additional validation: RIP (8 bytes before rflags) should be nonzero
            if pos >= 8 && r64(data, pos - 8) != 0 {
                last_regs_rflags = Some(pos);
                break;
            }
        }
    }
    let rflags_pos =
        last_regs_rflags.ok_or_else(|| anyhow::anyhow!("cannot find rflags before segs"))?;

    // Last regs block starts 136 bytes before rflags
    let last_regs = rflags_pos - 136;

    // Walk backward by 144-byte (kvm_regs size) steps to find the first vCPU
    let mut first = last_regs;
    while first >= 144 {
        let prev_rflags = r64(data, first - 8);
        if prev_rflags & 0x2 != 0 && prev_rflags > 0 && prev_rflags < 0x400000 {
            first -= 144;
        } else {
            break;
        }
    }
    Ok(first)
}

/// Find IOAPIC by searching for the standard MMIO base address 0xFEC00000.
fn find_ioapic(data: &[u8]) -> Result<usize> {
    let target: u64 = 0xFEC00000;
    for i in 0..data.len().saturating_sub(216) {
        if r64(data, i) == target {
            // Validate: first redir table entry at offset 24 should be a small value
            if i + 216 <= data.len() {
                let redir0 = r64(data, i + 24);
                if redir0 < 0x100000 {
                    return Ok(i);
                }
            }
        }
    }
    bail!("cannot find IOAPIC base address 0xFEC00000 in vmstate");
}

/// Find first LAPIC by its version register signature.
fn find_lapic(data: &[u8]) -> Result<usize> {
    for i in 0..data.len().saturating_sub(1024) {
        // Version register at byte offset 0x30 should be 0x50014 or 0x60014
        let ver = r32(data, i + 0x30);
        if ver & 0xFF0FF != 0x50014 && ver & 0xFF0FF != 0x60014 {
            continue;
        }
        // Byte 0 reserved, should be 0
        if r32(data, i) != 0 {
            continue;
        }
        // Spurious Interrupt Vector at 0xF0 should have enable bit (8) set
        if r32(data, i + 0xF0) & 0x100 == 0 {
            continue;
        }
        return Ok(i);
    }
    bail!("cannot find LAPIC in vmstate");
}

/// Check if an MXCSR value is valid: all exception masks set (bits 7-12) and reserved bits clear.
fn valid_mxcsr(v: u32) -> bool {
    v & 0x1F80 == 0x1F80 && v & 0xFFFF0000 == 0
}

/// Find first XSAVE area by FCW (0x037F) + valid MXCSR (at offset 24) pattern.
/// MXCSR exception flags (bits 0-5) and DAZ/FTZ bits may vary depending on what
/// code ran before the snapshot (e.g. numpy sets precision exception flags).
fn find_xsave(data: &[u8], search_after: usize) -> Option<usize> {
    for i in search_after..data.len().saturating_sub(4096) {
        if r16(data, i) == 0x037F && i + 28 <= data.len() && valid_mxcsr(r32(data, i + 24)) {
            return Some(i);
        }
    }
    None
}

/// Try to find XCRs between sregs end and xsave start.
/// Returns parsed kvm_xcrs or a default with XCR0 = x87.
fn find_and_parse_xcrs(data: &[u8], search_start: usize, search_end: usize) -> kvm_xcrs {
    // Search for nr_xcrs(u32=1) + flags(u32=0) + xcr(u32=0) + reserved(u32=0) + value(u64)
    // where value is a valid XCR0 (bit 0 set for x87)
    for i in search_start..search_end.saturating_sub(24) {
        if r32(data, i) == 1
            && r32(data, i + 4) == 0
            && r32(data, i + 8) == 0
            && r32(data, i + 12) == 0
        {
            let val = r64(data, i + 16);
            if val > 0 && val < 0x10000 && val & 1 != 0 {
                return parse_xcrs(data, i);
            }
        }
    }
    // Default: XCR0 = 1 (x87 only)
    let mut xcrs = kvm_xcrs::default();
    xcrs.nr_xcrs = 1;
    xcrs.xcrs[0].xcr = 0;
    xcrs.xcrs[0].value = 1;
    xcrs
}

/// kvm_sregs total serialized size: segments(192) + gdt(16) + idt(16) + crs(32) + cr8(8) + efer(8) + apic_base(8) + interrupt_bitmap(32) = 312
const SREGS_SIZE: usize = 312;

pub fn parse_vmstate(data: &[u8]) -> Result<ParsedVmState> {
    // Primary anchor: find EFER for first vCPU
    let efer_off = find_efer(data)?;

    // Derive all kvm_sregs field offsets from EFER (fixed struct layout)
    let segs_off = efer_off - EFER_TO_SEGS;
    let gdt_off = efer_off - EFER_TO_GDT;
    let idt_off = efer_off - EFER_TO_IDT;
    let cr_off = efer_off - EFER_TO_CR;
    let apic_base_off = efer_off + 8;

    // Find other structures independently (works for any number of vCPUs)
    let regs_off = find_first_regs(data, segs_off)?;
    let ioapic_off = find_ioapic(data)?;
    let lapic_off = find_lapic(data)?;
    let xsave_off = find_xsave(data, efer_off);

    // XCRs: search in the gap between sregs end and xsave, with fallback
    // interrupt_bitmap [u64;4] follows apic_base, so sregs ends at apic_base + 8 + 32 = efer + 48
    let xcrs = find_and_parse_xcrs(data, efer_off + 48, xsave_off.unwrap_or(data.len()));

    // Detect number of vCPUs from consecutive regs blocks (each 144 bytes)
    let vcpu_count = count_vcpu_regs(data, regs_off, segs_off);

    // Extract secondary vCPU states (vCPU 1, 2, ...)
    let mut secondary_vcpus = Vec::new();
    for i in 1..vcpu_count {
        // Regs: contiguous 144-byte blocks
        let sec_regs_off = regs_off + i * 144;
        // Sregs: contiguous 312-byte blocks
        let sec_segs_off = segs_off + i * SREGS_SIZE;
        let sec_efer_off = sec_segs_off + EFER_TO_SEGS;
        // LAPIC: contiguous 1024-byte blocks
        let sec_lapic_off = lapic_off + i * 1024;
        // XSAVE: find the i-th occurrence after the first
        let sec_xsave_off = xsave_off.and_then(|off| find_nth_xsave(data, off, i));
        let sec_xcrs = find_and_parse_xcrs(
            data,
            sec_efer_off + 48,
            sec_xsave_off.unwrap_or(data.len()),
        );
        secondary_vcpus.push(SecondaryVcpuState {
            regs: parse_regs(data, sec_regs_off),
            sregs: parse_sregs(
                data,
                sec_segs_off,
                sec_segs_off + 192,
                sec_segs_off + 208,
                sec_segs_off + 224,
                sec_efer_off,
                sec_efer_off + 8,
            ),
            lapic: if sec_lapic_off + 1024 <= data.len() {
                parse_lapic(data, sec_lapic_off)
            } else {
                kvm_lapic_state::default()
            },
            xcrs: sec_xcrs,
            xsave: sec_xsave_off
                .map(|off| parse_xsave(data, off))
                .unwrap_or_default(),
        });
    }

    Ok(ParsedVmState {
        regs: parse_regs(data, regs_off),
        sregs: parse_sregs(data, segs_off, gdt_off, idt_off, cr_off, efer_off, apic_base_off),
        msrs: parse_msrs(data),
        lapic: parse_lapic(data, lapic_off),
        ioapic_redirtbl: parse_ioapic_redirtbl(data, ioapic_off),
        xcrs,
        xsave: xsave_off
            .map(|off| parse_xsave(data, off))
            .unwrap_or_default(),
        cpuid_entries: parse_cpuid(data),
        virtio_queue: parse_virtio_queue_addrs(data),
        secondary_vcpus,
    })
}

/// Count vCPU regs blocks between regs_off and segs_off.
fn count_vcpu_regs(data: &[u8], regs_off: usize, segs_off: usize) -> usize {
    let mut count = 1;
    let mut off = regs_off + 144;
    while off + 144 <= segs_off {
        let rflags = r64(data, off + 136);
        if rflags & 0x2 != 0 && rflags > 0 && rflags < 0x400000 {
            count += 1;
            off += 144;
        } else {
            break;
        }
    }
    count
}

/// Find the n-th XSAVE area after the first one.
fn find_nth_xsave(data: &[u8], first_xsave: usize, n: usize) -> Option<usize> {
    let mut off = first_xsave + 4096; // skip first XSAVE
    for _ in 0..n {
        // Search for next XSAVE (FCW=0x037F + valid MXCSR)
        while off + 4096 <= data.len() {
            if r16(data, off) == 0x037F && off + 28 <= data.len() && valid_mxcsr(r32(data, off + 24))
            {
                break;
            }
            off += 1;
        }
        if off + 4096 > data.len() {
            return None;
        }
    }
    Some(off)
}

fn r64(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn r32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}
fn r16(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(d[o..o + 2].try_into().unwrap())
}

fn parse_regs(d: &[u8], o: usize) -> kvm_regs {
    kvm_regs {
        rax: r64(d, o),
        rbx: r64(d, o + 8),
        rcx: r64(d, o + 16),
        rdx: r64(d, o + 24),
        rsi: r64(d, o + 32),
        rdi: r64(d, o + 40),
        rsp: r64(d, o + 48),
        rbp: r64(d, o + 56),
        r8: r64(d, o + 64),
        r9: r64(d, o + 72),
        r10: r64(d, o + 80),
        r11: r64(d, o + 88),
        r12: r64(d, o + 96),
        r13: r64(d, o + 104),
        r14: r64(d, o + 112),
        r15: r64(d, o + 120),
        rip: r64(d, o + 128),
        rflags: r64(d, o + 136),
    }
}

fn parse_seg(d: &[u8], o: usize) -> kvm_segment {
    kvm_segment {
        base: r64(d, o),
        limit: r32(d, o + 8),
        selector: r16(d, o + 12),
        type_: d[o + 14],
        present: d[o + 15],
        dpl: d[o + 16],
        db: d[o + 17],
        s: d[o + 18],
        l: d[o + 19],
        g: d[o + 20],
        avl: d[o + 21],
        unusable: d[o + 22],
        padding: d[o + 23],
    }
}

fn parse_sregs(
    d: &[u8],
    segs: usize,
    gdt: usize,
    idt: usize,
    cr: usize,
    efer: usize,
    apic_base: usize,
) -> kvm_sregs {
    let mut s = kvm_sregs::default();
    s.cs = parse_seg(d, segs);
    s.ds = parse_seg(d, segs + 24);
    s.es = parse_seg(d, segs + 48);
    s.fs = parse_seg(d, segs + 72);
    s.gs = parse_seg(d, segs + 96);
    s.ss = parse_seg(d, segs + 120);
    s.tr = parse_seg(d, segs + 144);
    s.ldt = parse_seg(d, segs + 168);
    s.gdt = kvm_dtable {
        base: r64(d, gdt),
        limit: r16(d, gdt + 8),
        padding: [0; 3],
    };
    s.idt = kvm_dtable {
        base: r64(d, idt),
        limit: r16(d, idt + 8),
        padding: [0; 3],
    };
    s.cr0 = r64(d, cr);
    s.cr2 = r64(d, cr + 8);
    s.cr3 = r64(d, cr + 16);
    s.cr4 = r64(d, cr + 24);
    s.efer = r64(d, efer);
    s.apic_base = r64(d, apic_base);
    s
}

fn parse_lapic(d: &[u8], o: usize) -> kvm_lapic_state {
    let mut l = kvm_lapic_state::default();
    for i in 0..1024 {
        l.regs[i] = d[o + i] as i8;
    }
    l
}

fn parse_ioapic_redirtbl(d: &[u8], ioapic_off: usize) -> [u64; 24] {
    let mut tbl = [0u64; 24];
    // IOAPIC layout: base_address(8) + ioregsel(4) + id(4) + irr(4) + pad(4) = 24 byte header
    let redir_off = ioapic_off + 24;
    if d.len() >= redir_off + 24 * 8 {
        for i in 0..24 {
            tbl[i] = r64(d, redir_off + i * 8);
        }
    }
    tbl
}

fn parse_xcrs(d: &[u8], o: usize) -> kvm_xcrs {
    let mut xcrs = kvm_xcrs::default();
    if o + 24 <= d.len() {
        xcrs.nr_xcrs = r32(d, o);
        xcrs.flags = r32(d, o + 4);
        if xcrs.nr_xcrs >= 1 && xcrs.nr_xcrs <= 16 {
            for i in 0..xcrs.nr_xcrs as usize {
                let eo = o + 8 + i * 16;
                if eo + 16 <= d.len() {
                    xcrs.xcrs[i].xcr = r32(d, eo);
                    xcrs.xcrs[i].reserved = r32(d, eo + 4);
                    xcrs.xcrs[i].value = r64(d, eo + 8);
                }
            }
        }
    }
    xcrs
}

fn parse_xsave(d: &[u8], o: usize) -> kvm_xsave {
    let mut xsave = kvm_xsave::default();
    let size = std::cmp::min(4096, d.len().saturating_sub(o));
    if size > 0 {
        let src = &d[o..o + size];
        for i in 0..size / 4 {
            xsave.region[i] =
                u32::from_le_bytes([src[i * 4], src[i * 4 + 1], src[i * 4 + 2], src[i * 4 + 3]]);
        }
    }
    xsave
}

/// Parse CPUID entries from Firecracker's vmstate.
/// Supports two formats:
///   v1.12.0: [count:u64] [capacity:u64] then entries of [header:u64=0x28] [kvm_cpuid_entry2:40]
///   v1.15.0: entries stored as packed [kvm_cpuid_entry2:40] without per-entry headers
/// We locate CPUID leaf 0 by searching for its vendor string pattern.
fn parse_cpuid(data: &[u8]) -> Vec<kvm_cpuid_entry2> {
    let auth = b"Auth"; // AuthenticAMD
    let genu = b"Genu"; // GenuineIntel

    for vendor_pos in 0..data.len().saturating_sub(4) {
        let vendor = &data[vendor_pos..vendor_pos + 4];
        if vendor != auth && vendor != genu {
            continue;
        }

        // Try new format (v1.15.0+): vendor at entry_start + 16 (ebx offset in kvm_cpuid_entry2)
        // Entry size = 40 bytes, no per-entry header
        if vendor_pos >= 16 {
            let entry_off = vendor_pos - 16;
            if r32(data, entry_off) == 0 && r32(data, entry_off + 4) == 0 {
                let entries = collect_cpuid_entries(data, entry_off, 40);
                if entries.len() >= 5 {
                    return entries;
                }
            }
        }

        // Try old format (v1.12.0): vendor at header_start + 24 (header:8 + ebx_offset:16)
        // Entry size = 48 bytes (8-byte header + 40-byte entry)
        if vendor_pos >= 24 {
            let header_off = vendor_pos - 24;
            if header_off + 48 <= data.len() && r64(data, header_off) == 0x28 {
                if r32(data, header_off + 8) == 0 && r32(data, header_off + 12) == 0 {
                    let entries = collect_cpuid_entries_old(data, header_off, 48);
                    if entries.len() >= 5 {
                        return entries;
                    }
                }
            }
        }
    }

    Vec::new()
}

/// Collect CPUID entries in new format (40-byte packed kvm_cpuid_entry2, no header).
/// Stops at the first vCPU boundary (detected by function number decreasing, which
/// indicates the start of the next vCPU's sorted CPUID entries).
fn collect_cpuid_entries(data: &[u8], leaf0_off: usize, stride: usize) -> Vec<kvm_cpuid_entry2> {
    fn valid_cpuid_func(f: u32) -> bool {
        f <= 0x1F
            || (0x40000000..=0x40000020).contains(&f)
            || (0x80000000..=0x80000020).contains(&f)
    }

    let mut entries = Vec::new();
    let mut off = leaf0_off;
    let mut prev_func = 0u32;
    while off + stride <= data.len() {
        let func = r32(data, off);
        if !valid_cpuid_func(func) {
            break;
        }
        // Detect vCPU boundary: if function decreases (e.g., from 0x8000000x back to 0x0),
        // we've hit the next vCPU's entries
        if !entries.is_empty() && func < prev_func && func == 0 {
            break;
        }
        prev_func = func;
        entries.push(kvm_cpuid_entry2 {
            function: func,
            index: r32(data, off + 4),
            flags: r32(data, off + 8),
            eax: r32(data, off + 12),
            ebx: r32(data, off + 16),
            ecx: r32(data, off + 20),
            edx: r32(data, off + 24),
            padding: [0; 3],
        });
        off += stride;
    }
    entries
}

/// Collect CPUID entries in old format (8-byte header=0x28 + 40-byte entry).
fn collect_cpuid_entries_old(
    data: &[u8],
    header0_off: usize,
    stride: usize,
) -> Vec<kvm_cpuid_entry2> {
    let mut entries = Vec::new();
    let mut off = header0_off;
    while off + stride <= data.len() {
        if r64(data, off) != 0x28 {
            break;
        }
        entries.push(kvm_cpuid_entry2 {
            function: r32(data, off + 8),
            index: r32(data, off + 12),
            flags: r32(data, off + 16),
            eax: r32(data, off + 20),
            ebx: r32(data, off + 24),
            ecx: r32(data, off + 28),
            edx: r32(data, off + 32),
            padding: [0; 3],
        });
        off += stride;
    }
    entries
}

/// Search for virtio-blk queue addresses in vmstate binary data.
/// Supports two serialization formats:
///   v1.15.0+: GuestAddress uses Versionize v2 encoding: [02][u32 LE] per address (5 bytes each)
///   v1.12.0:  GuestAddress as raw u64 LE (8 bytes each), three consecutive
fn parse_virtio_queue_addrs(data: &[u8]) -> Option<VirtioQueueAddrs> {
    // Format 1 (v1.15.0+): [02][u32 desc][02][u32 avail][02][u32 used][u16 next_avail][u16 next_used]
    // Total pattern: 15 bytes for addresses + 4 bytes for avail/used indices = 19 bytes
    for i in 0..data.len().saturating_sub(19) {
        if data[i] != 0x02 || data[i + 5] != 0x02 || data[i + 10] != 0x02 {
            continue;
        }
        let desc = r32(data, i + 1) as u64;
        let avail = r32(data, i + 6) as u64;
        let used = r32(data, i + 11) as u64;

        if !valid_queue_addrs(desc, avail, used) {
            continue;
        }

        return Some(VirtioQueueAddrs {
            desc_table: desc,
            avail_ring: avail,
            used_ring: used,
            queue_size: 256,
            mmio_base: 0xC0001000,
            gsi: 5,
        });
    }

    // Format 2 (v1.12.0): three consecutive u64 values
    for i in 0..data.len().saturating_sub(24) {
        let desc = r64(data, i);
        let avail = r64(data, i + 8);
        let used = r64(data, i + 16);

        if !valid_queue_addrs(desc, avail, used) {
            continue;
        }

        return Some(VirtioQueueAddrs {
            desc_table: desc,
            avail_ring: avail,
            used_ring: used,
            queue_size: 256,
            mmio_base: 0xC0001000,
            gsi: 5,
        });
    }

    None
}

fn valid_queue_addrs(desc: u64, avail: u64, used: u64) -> bool {
    if desc == 0 || avail == 0 || used == 0 {
        return false;
    }
    if desc & 0xFFF != 0 || avail & 0xFFF != 0 || used & 0xFFF != 0 {
        return false;
    }
    if desc > 0x40000000 || avail > 0x40000000 || used > 0x40000000 {
        return false;
    }
    if desc < 0x100000 {
        return false;
    }
    if !(desc < avail && avail < used) {
        return false;
    }
    // For queue_size=256: desc table = 256*16 = 4096 bytes (1 page),
    // avail ring = 6 + 2*256 = 518 bytes (padded to 1 page),
    // so expect avail = desc + 0x1000, used = avail + 0x1000
    if avail != desc + 0x1000 || used != avail + 0x1000 {
        return false;
    }
    true
}

fn parse_msrs(data: &[u8]) -> Vec<kvm_msr_entry> {
    let targets: &[(u32, fn(u64) -> bool)] = &[
        (0xc0000081, |v| v != 0),
        (0xc0000082, |v| v > 0xffffffff80000000 || v == 0),
        (0xc0000083, |v| v > 0xffffffff80000000 || v == 0),
        (0xc0000084, |_| true),
        (0xc0000102, |_| true),
        (0x4b564d00, |v| v != 0 && v < 0x100000000),
        (0x4b564d01, |v| v != 0 && v < 0x100000000),
    ];
    let mut entries = Vec::new();
    for i in 0..data.len().saturating_sub(16) {
        let idx = r32(data, i);
        let res = r32(data, i + 4);
        if res != 0 {
            continue;
        }
        let val = r64(data, i + 8);
        for &(t, f) in targets {
            if idx == t && f(val) {
                entries.push(kvm_msr_entry {
                    index: t,
                    reserved: 0,
                    data: val,
                });
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    entries.reverse();
    entries.retain(|e| seen.insert(e.index));
    entries.reverse();
    entries
}
