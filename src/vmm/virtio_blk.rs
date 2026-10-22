//! Virtio-blk MMIO emulator with overlay CoW block device.
//!
//! Handles KVM_EXIT_MMIO for the virtio-mmio block device, processing
//! virtqueue descriptors for read/write/flush requests. Each forked VM
//! gets an independent overlay layer (HashMap<sector, data>) on top of
//! a shared read-only base image.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use kvm_ioctls::VmFd;

const SECTOR_SIZE: u64 = 512;

// Virtio MMIO register offsets
const MMIO_MAGIC: u64 = 0x000;
const MMIO_VERSION: u64 = 0x004;
const MMIO_DEVICE_ID: u64 = 0x008;
const MMIO_VENDOR_ID: u64 = 0x00C;
const MMIO_DEVICE_FEATURES: u64 = 0x010;
const MMIO_QUEUE_NUM_MAX: u64 = 0x034;
const MMIO_QUEUE_READY: u64 = 0x044;
const MMIO_QUEUE_NOTIFY: u64 = 0x050;
const MMIO_INTERRUPT_STATUS: u64 = 0x060;
const MMIO_INTERRUPT_ACK: u64 = 0x064;
const MMIO_STATUS: u64 = 0x070;
const MMIO_CONFIG_GENERATION: u64 = 0x0FC;
const MMIO_CONFIG: u64 = 0x100;

// Virtio block request types
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;

// Descriptor flags
const VIRTQ_DESC_F_NEXT: u16 = 1;

#[derive(Clone)]
pub struct VirtioQueueAddrs {
    pub desc_table: u64,
    pub avail_ring: u64,
    pub used_ring: u64,
    pub queue_size: u16,
    pub mmio_base: u64,
    pub gsi: u32,
}

struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

pub struct OverlayBlockDevice {
    base_fd: i32,
    capacity_sectors: u64,
    overlay: HashMap<u64, Vec<u8>>,
}

impl OverlayBlockDevice {
    pub fn new(base_file: &File) -> Self {
        let size = base_file.metadata().map(|m| m.len()).unwrap_or(0);
        Self {
            base_fd: base_file.as_raw_fd(),
            capacity_sectors: size / SECTOR_SIZE,
            overlay: HashMap::new(),
        }
    }

    fn read_sectors(&self, sector: u64, buf: &mut [u8]) {
        let num_sectors = buf.len() / SECTOR_SIZE as usize;
        for i in 0..num_sectors {
            let s = sector + i as u64;
            let off = i * SECTOR_SIZE as usize;
            let end = off + SECTOR_SIZE as usize;
            if let Some(data) = self.overlay.get(&s) {
                buf[off..end].copy_from_slice(data);
            } else {
                unsafe {
                    libc::pread(
                        self.base_fd,
                        buf[off..end].as_mut_ptr() as *mut libc::c_void,
                        SECTOR_SIZE as usize,
                        (s * SECTOR_SIZE) as i64,
                    );
                }
            }
        }
    }

    fn write_sectors(&mut self, sector: u64, data: &[u8]) {
        let num_sectors = data.len() / SECTOR_SIZE as usize;
        for i in 0..num_sectors {
            let s = sector + i as u64;
            let off = i * SECTOR_SIZE as usize;
            let end = off + SECTOR_SIZE as usize;
            self.overlay.insert(s, data[off..end].to_vec());
        }
    }
}

pub struct VirtioBlk {
    queue: VirtioQueueAddrs,
    device: OverlayBlockDevice,
    last_avail_idx: u16,
    interrupt_status: u32,
    status: u32,
    _base_file: Arc<File>,
}

impl VirtioBlk {
    pub fn new(queue: VirtioQueueAddrs, base_file: Arc<File>) -> Self {
        let device = OverlayBlockDevice::new(&base_file);
        Self {
            queue,
            device,
            last_avail_idx: 0,
            interrupt_status: 0,
            status: 0xF, // ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK
            _base_file: base_file,
        }
    }

    /// Read current avail_ring.idx from guest memory so we don't re-process
    /// requests that were already completed before the snapshot.
    pub fn init_from_guest_memory(&mut self, mem_ptr: *const u8, mem_size: usize) {
        let addr = (self.queue.avail_ring + 2) as usize;
        if addr + 2 <= mem_size {
            self.last_avail_idx = unsafe {
                (mem_ptr.add(addr) as *const u16).read_unaligned()
            };
        }
    }

    pub fn handles_mmio(&self, addr: u64) -> bool {
        addr >= self.queue.mmio_base && addr < self.queue.mmio_base + 0x1000
    }

    pub fn mmio_read(&self, addr: u64, data: &mut [u8]) {
        let offset = addr - self.queue.mmio_base;
        let val: u32 = match offset {
            MMIO_MAGIC => 0x74726976,
            MMIO_VERSION => 2,
            MMIO_DEVICE_ID => 2, // block device
            MMIO_VENDOR_ID => 0x554D4551,
            MMIO_DEVICE_FEATURES => 0,
            MMIO_QUEUE_NUM_MAX => self.queue.queue_size as u32,
            MMIO_QUEUE_READY => 1,
            MMIO_INTERRUPT_STATUS => self.interrupt_status,
            MMIO_STATUS => self.status,
            MMIO_CONFIG_GENERATION => 0,
            o if o >= MMIO_CONFIG => {
                // Block device config: capacity (u64) at offset 0
                let config_off = (o - MMIO_CONFIG) as usize;
                let cap_bytes = self.device.capacity_sectors.to_le_bytes();
                if config_off < 8 {
                    let end = (config_off + data.len()).min(8);
                    let mut v = 0u32;
                    for (i, &b) in cap_bytes[config_off..end].iter().enumerate() {
                        v |= (b as u32) << (i * 8);
                    }
                    v
                } else {
                    0
                }
            }
            _ => 0,
        };
        let bytes = val.to_le_bytes();
        let len = data.len().min(4);
        data[..len].copy_from_slice(&bytes[..len]);
    }

    pub fn mmio_write(
        &mut self,
        addr: u64,
        data: &[u8],
        mem_ptr: *mut u8,
        mem_size: usize,
        vm_fd: &VmFd,
    ) {
        let offset = addr - self.queue.mmio_base;
        let val = match data.len() {
            1 => data[0] as u32,
            2 => u16::from_le_bytes([data[0], data[1]]) as u32,
            4 => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            _ => return,
        };

        match offset {
            MMIO_QUEUE_NOTIFY => {
                self.process_queue(mem_ptr, mem_size, vm_fd);
            }
            MMIO_INTERRUPT_ACK => {
                self.interrupt_status &= !val;
                if self.interrupt_status == 0 {
                    let _ = vm_fd.set_irq_line(self.queue.gsi, false);
                }
            }
            MMIO_STATUS => {
                self.status = val;
            }
            _ => {}
        }
    }

    fn process_queue(&mut self, mem_ptr: *mut u8, mem_size: usize, vm_fd: &VmFd) {
        let avail_idx = read_u16(mem_ptr, mem_size, self.queue.avail_ring + 2);

        if avail_idx == self.last_avail_idx {
            return;
        }

        while self.last_avail_idx != avail_idx {
            let ring_idx = (self.last_avail_idx % self.queue.queue_size) as u64;
            let desc_idx = read_u16(
                mem_ptr,
                mem_size,
                self.queue.avail_ring + 4 + ring_idx * 2,
            );

            let bytes_written = self.process_request(mem_ptr, mem_size, desc_idx);

            // Update used ring
            let used_idx = read_u16(mem_ptr, mem_size, self.queue.used_ring + 2);
            let entry_addr =
                self.queue.used_ring + 4 + (used_idx % self.queue.queue_size) as u64 * 8;
            write_u32(mem_ptr, mem_size, entry_addr, desc_idx as u32);
            write_u32(mem_ptr, mem_size, entry_addr + 4, bytes_written);
            write_u16(
                mem_ptr,
                mem_size,
                self.queue.used_ring + 2,
                used_idx.wrapping_add(1),
            );

            self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
        }

        // Update avail_event in the used ring to request notification for the
        // next request. With VIRTIO_F_EVENT_IDX, the guest checks avail_event
        // to decide whether to write QUEUE_NOTIFY. Setting avail_event to the
        // current avail_idx ensures the guest notifies us for the very next request.
        let avail_event_addr =
            self.queue.used_ring + 4 + self.queue.queue_size as u64 * 8;
        write_u16(mem_ptr, mem_size, avail_event_addr, avail_idx);

        // Signal interrupt (level-triggered: assert IRQ line)
        self.interrupt_status |= 1;
        let _ = vm_fd.set_irq_line(self.queue.gsi, true);
    }

    fn process_request(&mut self, mem_ptr: *mut u8, mem_size: usize, first_desc: u16) -> u32 {
        // Collect descriptor chain
        let mut descs = Vec::new();
        let mut idx = first_desc;
        loop {
            let desc = read_desc(mem_ptr, mem_size, self.queue.desc_table, idx);
            let has_next = desc.flags & VIRTQ_DESC_F_NEXT != 0;
            let next = desc.next;
            descs.push(desc);
            if !has_next || descs.len() > 256 {
                break;
            }
            idx = next;
        }

        if descs.len() < 2 {
            return 0;
        }

        // First descriptor: request header (type:u32 + reserved:u32 + sector:u64)
        let req_type = read_u32(mem_ptr, mem_size, descs[0].addr);
        let sector = read_u64(mem_ptr, mem_size, descs[0].addr + 8);

        // Middle descriptors: data buffers
        let data_descs = &descs[1..descs.len().saturating_sub(1)];
        let mut total_bytes: u32 = 0;

        match req_type {
            VIRTIO_BLK_T_IN => {
                // Device read: copy from disk to guest memory
                let mut cur_sector = sector;
                for d in data_descs {
                    let addr = d.addr as usize;
                    let len = d.len as usize;
                    if addr + len <= mem_size && len > 0 {
                        let buf =
                            unsafe { std::slice::from_raw_parts_mut(mem_ptr.add(addr), len) };
                        self.device.read_sectors(cur_sector, buf);
                        cur_sector += len as u64 / SECTOR_SIZE;
                        total_bytes += d.len;
                    }
                }
            }
            VIRTIO_BLK_T_OUT => {
                // Device write: copy from guest memory to overlay
                let mut cur_sector = sector;
                for d in data_descs {
                    let addr = d.addr as usize;
                    let len = d.len as usize;
                    if addr + len <= mem_size && len > 0 {
                        let buf = unsafe {
                            std::slice::from_raw_parts(mem_ptr.add(addr) as *const u8, len)
                        };
                        self.device.write_sectors(cur_sector, buf);
                        cur_sector += len as u64 / SECTOR_SIZE;
                        total_bytes += d.len;
                    }
                }
            }
            VIRTIO_BLK_T_FLUSH => { /* no-op for overlay */ }
            _ => {}
        }

        // Last descriptor: status byte (write 0 = VIRTIO_BLK_S_OK)
        if let Some(status_desc) = descs.last() {
            let addr = status_desc.addr as usize;
            if addr < mem_size {
                unsafe { *mem_ptr.add(addr) = 0; }
                total_bytes += 1;
            }
        }

        total_bytes
    }
}

// Guest memory access helpers

fn read_desc(mem_ptr: *mut u8, mem_size: usize, desc_table: u64, idx: u16) -> VirtqDesc {
    let addr = desc_table + idx as u64 * 16;
    VirtqDesc {
        addr: read_u64(mem_ptr, mem_size, addr),
        len: read_u32(mem_ptr, mem_size, addr + 8),
        flags: read_u16(mem_ptr, mem_size, addr + 12),
        next: read_u16(mem_ptr, mem_size, addr + 14),
    }
}

fn read_u16(mem_ptr: *mut u8, mem_size: usize, gpa: u64) -> u16 {
    let off = gpa as usize;
    if off + 2 > mem_size {
        return 0;
    }
    unsafe { (mem_ptr.add(off) as *const u16).read_unaligned() }
}

fn read_u32(mem_ptr: *mut u8, mem_size: usize, gpa: u64) -> u32 {
    let off = gpa as usize;
    if off + 4 > mem_size {
        return 0;
    }
    unsafe { (mem_ptr.add(off) as *const u32).read_unaligned() }
}

fn read_u64(mem_ptr: *mut u8, mem_size: usize, gpa: u64) -> u64 {
    let off = gpa as usize;
    if off + 8 > mem_size {
        return 0;
    }
    unsafe { (mem_ptr.add(off) as *const u64).read_unaligned() }
}

fn write_u16(mem_ptr: *mut u8, mem_size: usize, gpa: u64, val: u16) {
    let off = gpa as usize;
    if off + 2 > mem_size {
        return;
    }
    unsafe { (mem_ptr.add(off) as *mut u16).write_unaligned(val); }
}

fn write_u32(mem_ptr: *mut u8, mem_size: usize, gpa: u64, val: u32) {
    let off = gpa as usize;
    if off + 4 > mem_size {
        return;
    }
    unsafe { (mem_ptr.add(off) as *mut u32).write_unaligned(val); }
}
