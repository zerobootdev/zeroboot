use anyhow::{bail, Result};
use kvm_bindings::*;

// Reference offsets for Firecracker v1.12.0, 1 vCPU, x86_64.
// These are the known offsets for ONE specific vmstate layout.
// The actual offsets may shift due to variable-length versionize sections,
// so we auto-detect the base shift using anchor patterns.
const REF_IOAPIC: usize = 0x0591;
const REF_LAPIC: usize = 0x2541;
const REF_REGS: usize = 0x2955;
const REF_SEGS: usize = 0x29ED;
const REF_GDT: usize = 0x2AAD;
const REF_IDT: usize = 0x2ABD;
const REF_CR: usize = 0x2ACD;
const REF_EFER: usize = 0x2AF5;
const REF_APIC_BASE: usize = 0x2AFD;
const REF_XCRS: usize = 0x2B75;
const REF_XSAVE: usize = 0x2D0D;

pub struct ParsedVmState {
    pub regs: kvm_regs,
    pub sregs: kvm_sregs,
    pub msrs: Vec<kvm_msr_entry>,
    pub lapic: kvm_lapic_state,
    pub ioapic_redirtbl: [u64; 24],
    pub xcrs: kvm_xcrs,
    pub xsave: kvm_xsave,
    pub cpuid_entries: Vec<kvm_cpuid_entry2>,
}

/// Find the base offset shift by locating the IOAPIC base address (0xFEC00000).
/// Returns the signed delta to subtract from reference offsets.
fn detect_offset_shift(data: &[u8]) -> Result<isize> {
    let target: u64 = 0xFEC00000; // Standard IOAPIC MMIO base
    for i in 0..data.len().saturating_sub(8) {
        if r64(data, i) == target {
            let shift = REF_IOAPIC as isize - i as isize;
            // Validate: check that EFER at the shifted offset looks right (0xD01 or 0x501)
            let efer_off = (REF_EFER as isize - shift) as usize;
            if efer_off + 8 <= data.len() {
                let efer = r64(data, efer_off);
                if efer == 0xD01 || efer == 0x501 {
                    return Ok(shift);
                }
            }
        }
    }
    bail!("cannot detect vmstate layout: IOAPIC base address 0xFEC00000 not found");
}

fn adj(reference: usize, shift: isize) -> usize {
    (reference as isize - shift) as usize
}

pub fn parse_vmstate(data: &[u8]) -> Result<ParsedVmState> {
    let shift = detect_offset_shift(data)?;
    let efer_off = adj(REF_EFER, shift);
    if efer_off + 8 > data.len() {
        bail!("vmstate too small: {} bytes", data.len());
    }

    Ok(ParsedVmState {
        regs: parse_regs(data, adj(REF_REGS, shift)),
        sregs: parse_sregs(data, shift),
        msrs: parse_msrs(data),
        lapic: parse_lapic(data, adj(REF_LAPIC, shift)),
        ioapic_redirtbl: parse_ioapic_redirtbl(data, adj(REF_IOAPIC, shift)),
        xcrs: parse_xcrs(data, adj(REF_XCRS, shift)),
        xsave: parse_xsave(data, adj(REF_XSAVE, shift)),
        cpuid_entries: parse_cpuid(data),
    })
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

fn parse_sregs(d: &[u8], shift: isize) -> kvm_sregs {
    let segs = adj(REF_SEGS, shift);
    let gdt = adj(REF_GDT, shift);
    let idt = adj(REF_IDT, shift);
    let cr = adj(REF_CR, shift);
    let efer = adj(REF_EFER, shift);
    let apic_base = adj(REF_APIC_BASE, shift);

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
/// Entries are stored as a versionize Vec: [count:u64] [capacity:u64] then
/// count entries of [header:u64=0x28] [function:u32 index:u32 flags:u32 eax:u32 ebx:u32 ecx:u32 edx:u32 padding:12].
/// We locate CPUID leaf 0 by searching for its vendor string pattern.
fn parse_cpuid(data: &[u8]) -> Vec<kvm_cpuid_entry2> {
    // Each entry: 8-byte versionize header (0x28) + 40-byte kvm_cpuid_entry2 = 48 bytes
    const ENTRY_SIZE: usize = 48;
    const HEADER_VAL: u64 = 0x28; // 40 bytes payload

    // Search for CPUID leaf 0: header=0x28, function=0, index=0, flags=0,
    // then eax=some_max_leaf, ebx=vendor ("Auth" or "Genu")
    let auth = b"Auth"; // AuthenticAMD
    let genu = b"Genu"; // GenuineIntel

    for i in 0..data.len().saturating_sub(ENTRY_SIZE) {
        if r64(data, i) != HEADER_VAL {
            continue;
        }
        if r32(data, i + 8) != 0 {
            continue;
        } // function == 0
        if r32(data, i + 12) != 0 {
            continue;
        } // index == 0
        // Check vendor string in ebx (offset 24 from entry start)
        if i + 28 > data.len() {
            continue;
        }
        let ebx_bytes = &data[i + 24..i + 28];
        if ebx_bytes != auth && ebx_bytes != genu {
            continue;
        }

        // Found CPUID leaf 0 at offset i. Read the count from 16 bytes before.
        if i < 16 {
            continue;
        }
        let count = r64(data, i - 16) as usize;
        if count == 0 || count > 256 {
            continue;
        }
        // Validate: capacity should match count (or be >= count)
        let capacity = r64(data, i - 8) as usize;
        if capacity < count || capacity > 256 {
            continue;
        }

        let mut entries = Vec::with_capacity(count);
        for j in 0..count {
            let off = i + j * ENTRY_SIZE;
            if off + ENTRY_SIZE > data.len() {
                break;
            }
            // Verify header
            if r64(data, off) != HEADER_VAL {
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
        }

        if entries.len() == count {
            return entries;
        }
    }

    Vec::new() // Fallback: no CPUID found
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
