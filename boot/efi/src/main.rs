//! A UEFI loader for the feos kernel.
//!
//! It reads `\feos.elf` from the volume it was loaded from, asks firmware for a
//! framebuffer, the ACPI and device tree pointers and the memory map, leaves boot
//! services, copies the image to the addresses it's linked at, and jumps in with a
//! [`Handoff`]. From there it's the kernel's usual path.
//!
//! Built with `scripts/cargo-efi.sh`. See `docs/architecture.md`.

#![no_std]
#![no_main]

use core::arch::asm;
use core::ffi::c_void;
use core::mem::size_of;
use core::ptr::{copy, null_mut, write_bytes};

use feos_bootinfo::{is_covered, is_usable_type, usable_ram, walk_memory_map, Elf, Framebuffer, Handoff, Range, MAX_RAM_RANGES};

type Status = usize;
type Handle = *mut c_void;
const SUCCESS: Status = 0;

const ALLOCATE_ANY_PAGES: u32 = 0;
const ALLOCATE_MAX_ADDRESS: u32 = 1;
/// `EfiLoaderData`.
const LOADER_DATA: u32 = 2;
/// `BootServices` function indexes, in the specification's order.
mod bs {
    pub const ALLOCATE_PAGES: usize = 2;
    pub const GET_MEMORY_MAP: usize = 4;
    pub const HANDLE_PROTOCOL: usize = 16;
    pub const EXIT_BOOT_SERVICES: usize = 26;
    pub const LOCATE_PROTOCOL: usize = 37;
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid(u32, u16, u16, [u8; 8]);

const GUID_LOADED_IMAGE: Guid = Guid(0x5b1b31a1, 0x9562, 0x11d2, [0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]);
const GUID_SIMPLE_FS: Guid = Guid(0x964e5b22, 0x6459, 0x11d2, [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]);
const GUID_GOP: Guid = Guid(0x9042a9de, 0x23dc, 0x4a38, [0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a]);
const GUID_ACPI2: Guid = Guid(0x8868e871, 0xe4f1, 0x11d3, [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81]);
const GUID_ACPI1: Guid = Guid(0xeb9d2d30, 0x2d88, 0x11d3, [0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d]);
const GUID_DTB: Guid = Guid(0xb1b621d5, 0xf19c, 0x41a5, [0x83, 0x0b, 0xd9, 0x15, 0x2c, 0x69, 0xaa, 0xe0]);

#[repr(C)]
struct TextOutput {
    reset: usize,
    output_string: unsafe extern "efiapi" fn(*mut TextOutput, *const u16) -> Status,
}

#[repr(C)]
struct BootServices {
    header: [u64; 3],
    functions: [usize; 44],
}

#[repr(C)]
struct ConfigurationEntry {
    guid: Guid,
    table: *mut c_void,
}

#[repr(C)]
struct SystemTable {
    header: [u64; 3],
    vendor: *const u16,
    revision: u32,
    console_in_handle: Handle,
    console_in: *mut c_void,
    console_out_handle: Handle,
    console_out: *mut TextOutput,
    error_handle: Handle,
    error: *mut c_void,
    runtime: *mut c_void,
    boot: *mut BootServices,
    table_count: usize,
    tables: *const ConfigurationEntry,
}

#[repr(C)]
struct LoadedImage {
    revision: u32,
    parent: Handle,
    system: *mut SystemTable,
    device: Handle,
    path: *mut c_void,
    reserved: *mut c_void,
    options_size: u32,
    options: *mut c_void,
    base: *mut c_void,
    size: u64,
}

#[repr(C)]
struct SimpleFileSystem {
    revision: u64,
    open_volume: unsafe extern "efiapi" fn(*mut SimpleFileSystem, *mut *mut File) -> Status,
}

#[repr(C)]
struct File {
    revision: u64,
    open: unsafe extern "efiapi" fn(*mut File, *mut *mut File, *const u16, u64, u64) -> Status,
    close: unsafe extern "efiapi" fn(*mut File) -> Status,
    delete: usize,
    read: unsafe extern "efiapi" fn(*mut File, *mut usize, *mut c_void) -> Status,
    write: usize,
    get_position: unsafe extern "efiapi" fn(*mut File, *mut u64) -> Status,
    set_position: unsafe extern "efiapi" fn(*mut File, u64) -> Status,
}

#[repr(C)]
struct GopModeInfo {
    version: u32,
    width: u32,
    height: u32,
    format: u32,
    masks: [u32; 4],
    stride: u32,
}

#[repr(C)]
struct GopMode {
    max_mode: u32,
    mode: u32,
    info: *mut GopModeInfo,
    info_size: usize,
    framebuffer: u64,
    framebuffer_size: usize,
}

#[repr(C)]
struct Gop {
    query_mode: unsafe extern "efiapi" fn(*mut Gop, u32, *mut usize, *mut *mut GopModeInfo) -> Status,
    set_mode: unsafe extern "efiapi" fn(*mut Gop, u32) -> Status,
    blt: usize,
    mode: *mut GopMode,
}

/// What the rest of the code needs of firmware.
struct Firmware {
    image: Handle,
    system: *mut SystemTable,
    boot: *mut BootServices,
}

/// Blue in the low byte, with a padding byte on top: what the kernel draws.
const PIXEL_BGRX: u32 = 1;
/// What the desktop is laid out for, if firmware has it.
const WANTED_WIDTH: u32 = 1024;
const WANTED_HEIGHT: u32 = 768;

#[cfg(target_arch = "x86_64")]
const ELF_MACHINE: u16 = 62;
#[cfg(target_arch = "aarch64")]
const ELF_MACHINE: u16 = 183;

impl Firmware {
    fn print(&self, text: &str) {
        let mut buffer = [0u16; 128];
        let mut used = 0;
        for byte in text.bytes() {
            if used + 3 >= buffer.len() {
                self.flush(&buffer, &mut used);
            }
            if byte == b'\n' {
                buffer[used] = u16::from(b'\r');
                used += 1;
            }
            buffer[used] = u16::from(byte);
            used += 1;
        }
        self.flush(&buffer, &mut used);
    }

    fn flush(&self, buffer: &[u16; 128], used: &mut usize) {
        let mut copy = [0u16; 128];
        copy[..*used].copy_from_slice(&buffer[..*used]);
        // SAFETY: firmware gave this table, and boot services are still running.
        unsafe {
            let console = (*self.system).console_out;
            if !console.is_null() {
                ((*console).output_string)(console, copy.as_ptr());
            }
        }
        *used = 0;
    }

    fn print_hex(&self, value: u64) {
        let mut text = [0u8; 18];
        text[0] = b'0';
        text[1] = b'x';
        for index in 0..16 {
            let digit = (value >> (60 - index * 4)) as u8 & 0xf;
            text[2 + index] = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
        }
        if let Ok(text) = core::str::from_utf8(&text) {
            self.print(text);
        }
    }

    /// A function from the boot services table.
    unsafe fn function<T: Copy>(&self, index: usize) -> T {
        debug_assert_eq!(size_of::<T>(), size_of::<usize>());
        // SAFETY: as promised by the caller: `T` is that function's signature.
        unsafe { core::mem::transmute_copy(&(*self.boot).functions[index]) }
    }

    fn handle_protocol<T>(&self, handle: Handle, guid: &Guid) -> Option<*mut T> {
        let mut out: *mut T = null_mut();
        // SAFETY: the signature is the specification's.
        let status = unsafe {
            let call: unsafe extern "efiapi" fn(Handle, *const Guid, *mut *mut T) -> Status =
                self.function(bs::HANDLE_PROTOCOL);
            call(handle, guid, &mut out)
        };
        (status == SUCCESS && !out.is_null()).then_some(out)
    }

    fn locate_protocol<T>(&self, guid: &Guid) -> Option<*mut T> {
        let mut out: *mut T = null_mut();
        // SAFETY: the signature is the specification's.
        let status = unsafe {
            let call: unsafe extern "efiapi" fn(*const Guid, *mut c_void, *mut *mut T) -> Status =
                self.function(bs::LOCATE_PROTOCOL);
            call(guid, null_mut(), &mut out)
        };
        (status == SUCCESS && !out.is_null()).then_some(out)
    }

    /// Pages under 4 GiB, which is all the kernel's early map reaches.
    fn alloc_low(&self, bytes: usize) -> Option<*mut u8> {
        self.alloc_pages(ALLOCATE_MAX_ADDRESS, 0xffff_ffff, bytes)
    }

    /// Pages anywhere.
    fn alloc_any(&self, bytes: usize) -> Option<*mut u8> {
        self.alloc_pages(ALLOCATE_ANY_PAGES, 0, bytes)
    }

    fn alloc_pages(&self, kind: u32, limit: u64, bytes: usize) -> Option<*mut u8> {
        let mut address = limit;
        // SAFETY: the signature is the specification's.
        let status = unsafe {
            let call: unsafe extern "efiapi" fn(u32, u32, usize, *mut u64) -> Status =
                self.function(bs::ALLOCATE_PAGES);
            call(kind, LOADER_DATA, bytes.div_ceil(4096), &mut address)
        };
        (status == SUCCESS).then_some(address as *mut u8)
    }

    /// Fills `map` with the memory map, and returns its size, the stride and its key.
    fn memory_map_with_key(&self, map: &mut [u8]) -> Option<(usize, usize, usize)> {
        let mut size = map.len();
        let mut key = 0usize;
        let mut stride = 0usize;
        let mut version = 0u32;
        // SAFETY: the signature is the specification's, and `map` is writable.
        let status = unsafe {
            let call: unsafe extern "efiapi" fn(*mut usize, *mut u8, *mut usize, *mut usize, *mut u32) -> Status =
                self.function(bs::GET_MEMORY_MAP);
            call(&mut size, map.as_mut_ptr(), &mut key, &mut stride, &mut version)
        };
        (status == SUCCESS).then_some((size, stride, key))
    }

    /// Fills `map` with the memory map, and returns its size and the stride.
    fn memory_map(&self, map: &mut [u8]) -> Option<(usize, usize)> {
        self.memory_map_with_key(map).map(|(size, stride, _)| (size, stride))
    }

    /// Reads `\feos.elf` from the volume this was loaded from.
    fn read_kernel(&self) -> Option<&'static [u8]> {
        let loaded: *mut LoadedImage = self.handle_protocol(self.image, &GUID_LOADED_IMAGE)?;
        // SAFETY: firmware's structures, while boot services run.
        unsafe {
            let volume: *mut SimpleFileSystem = self.handle_protocol((*loaded).device, &GUID_SIMPLE_FS)?;
            let mut root: *mut File = null_mut();
            if ((*volume).open_volume)(volume, &mut root) != SUCCESS {
                return None;
            }
            let name: [u16; 10] = [
                b'\\' as u16, b'f' as u16, b'e' as u16, b'o' as u16, b's' as u16, b'.' as u16, b'e' as u16,
                b'l' as u16, b'f' as u16, 0,
            ];
            let mut file: *mut File = null_mut();
            // Read only.
            if ((*root).open)(root, &mut file, name.as_ptr(), 1, 0) != SUCCESS {
                return None;
            }
            // The end of the file, which is how EFI says "how long".
            ((*file).set_position)(file, u64::MAX);
            let mut size = 0u64;
            ((*file).get_position)(file, &mut size);
            ((*file).set_position)(file, 0);
            let buffer = self.alloc_any(size as usize)?;
            let mut left = size as usize;
            let mut at = buffer;
            while left != 0 {
                let mut piece = left;
                if ((*file).read)(file, &mut piece, at.cast()) != SUCCESS || piece == 0 {
                    return None;
                }
                left -= piece;
                at = at.add(piece);
            }
            ((*file).close)(file);
            Some(core::slice::from_raw_parts(buffer, size as usize))
        }
    }

    /// The framebuffer: at the mode the desktop wants, if there is one.
    fn framebuffer(&self) -> Framebuffer {
        let Some(gop) = self.locate_protocol::<Gop>(&GUID_GOP) else {
            self.print("feos-efi: no graphics output protocol\n");
            return Framebuffer::default();
        };
        // SAFETY: firmware's structures, while boot services run.
        unsafe {
            let mode = (*gop).mode;
            for number in 0..(*mode).max_mode {
                let mut info: *mut GopModeInfo = null_mut();
                let mut size = 0;
                if ((*gop).query_mode)(gop, number, &mut size, &mut info) == SUCCESS
                    && (*info).width == WANTED_WIDTH
                    && (*info).height == WANTED_HEIGHT
                    && (*info).format == PIXEL_BGRX
                {
                    ((*gop).set_mode)(gop, number);
                    break;
                }
            }
            let info = (*mode).info;
            if (*info).format != PIXEL_BGRX || (*mode).framebuffer == 0 {
                self.print("feos-efi: the framebuffer's not one the kernel can draw on\n");
                return Framebuffer::default();
            }
            Framebuffer {
                base: (*mode).framebuffer,
                size: (*mode).framebuffer_size as u64,
                width: (*info).width,
                height: (*info).height,
                stride: (*info).stride,
                reserved: 0,
            }
        }
    }

    fn find_tables(&self, handoff: &mut Handoff) {
        // SAFETY: firmware's structures, while boot services run.
        let tables = unsafe {
            core::slice::from_raw_parts((*self.system).tables, (*self.system).table_count)
        };
        let mut acpi1 = 0;
        for table in tables {
            let address = table.table as u64;
            if table.guid == GUID_ACPI2 {
                handoff.rsdp = address;
            } else if table.guid == GUID_ACPI1 {
                acpi1 = address;
            } else if table.guid == GUID_DTB {
                handoff.device_tree = address;
            }
        }
        if handoff.rsdp == 0 {
            handoff.rsdp = acpi1;
        }
    }
}

/// Stops with a message, for firmware to do what it likes with.
fn fail(firmware: &Firmware, message: &str) -> Status {
    firmware.print("feos-efi: ");
    firmware.print(message);
    firmware.print("\n");
    1
}

/// Leaves boot services, with `map` holding the memory map that was final.
fn exit_boot_services(firmware: &Firmware, map: &mut [u8]) -> Option<(usize, usize)> {
    for _ in 0..8 {
        let (size, stride, key) = firmware.memory_map_with_key(map)?;
        // SAFETY: as above.
        let status = unsafe {
            let call: unsafe extern "efiapi" fn(Handle, usize) -> Status =
                firmware.function(bs::EXIT_BOOT_SERVICES);
            call(firmware.image, key)
        };
        // Firmware's allowed to change the map in between, and ask for another go.
        if status == SUCCESS {
            return Some((size, stride));
        }
    }
    None
}

/// The current stack pointer.
fn stack_pointer() -> u64 {
    let value: u64;
    // SAFETY: reads a register.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        asm!("mov {}, rsp", out(reg) value, options(nomem, nostack));
        #[cfg(target_arch = "aarch64")]
        asm!("mov {}, sp", out(reg) value, options(nomem, nostack));
    }
    value
}

/// Sets out the kernel where it's linked. Nothing here may be in the way, which
/// `efi_main` checked.
fn place_segments(elf: &Elf<'_>) {
    let mut index = 0;
    while let Ok(Some(segment)) = elf.segment(index) {
        // SAFETY: the range was checked against the map, and nothing else is running.
        unsafe {
            copy(segment.data.as_ptr(), segment.address as *mut u8, segment.data.len());
            write_bytes(
                (segment.address as *mut u8).add(segment.data.len()),
                0,
                segment.memory_size as usize - segment.data.len(),
            );
        }
        index += 1;
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn enter_kernel(entry: u64, handoff: u64) -> ! {
    // SAFETY: as promised by the caller. The kernel's entry takes it from here.
    unsafe {
        asm!(
            "cli",
            "jmp {entry}",
            entry = in(reg) entry,
            in("rdi") handoff,
            options(noreturn)
        )
    }
}

#[cfg(target_arch = "aarch64")]
fn clean_to_memory(base: u64, size: u64) {
    let ctr: u64;
    // SAFETY: reads a register.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let line = 4u64 << ((ctr >> 16) & 0xf);
    let mut at = base & !(line - 1);
    while at < base.saturating_add(size) {
        // SAFETY: cleaning a cache line changes nothing a program can see.
        unsafe { asm!("dc civac, {}", in(reg) at, options(nostack, preserves_flags)) };
        at += line;
    }
    // SAFETY: a barrier.
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}

/// Hands over to the kernel with the MMU and caches off, as the boot protocol wants.
#[cfg(target_arch = "aarch64")]
unsafe fn enter_kernel(entry: u64, handoff: u64) -> ! {
    // SAFETY: firmware's map is an identity map, so turning it off changes no address
    // this is running from. Everything the kernel reads has been cleaned to memory.
    unsafe {
        asm!(
            "msr daifset, #0xf",
            "mrs x3, CurrentEL",
            "cmp x3, #8",
            "b.eq 2f",
            "mrs x3, sctlr_el1",
            "bic x3, x3, #1",
            "bic x3, x3, #4",
            "bic x3, x3, #0x1000",
            "msr sctlr_el1, x3",
            "b 3f",
            "2:",
            "mrs x3, sctlr_el2",
            "bic x3, x3, #1",
            "bic x3, x3, #4",
            "bic x3, x3, #0x1000",
            "msr sctlr_el2, x3",
            "3:",
            "isb",
            "ic iallu",
            "dsb sy",
            "isb",
            "br x1",
            in("x0") handoff,
            in("x1") entry,
            options(noreturn)
        )
    }
}

/// Where the kernel wants to be entered from a firmware's state.
fn kernel_entry(elf: &Elf<'_>) -> Option<u64> {
    #[cfg(target_arch = "x86_64")]
    {
        elf.marked_word(feos_bootinfo::X86_ENTRY_MARK)
    }
    #[cfg(target_arch = "aarch64")]
    {
        Some(elf.entry)
    }
}

/// Firmware's entry point.
#[export_name = "efi_main"]
extern "efiapi" fn efi_main(image: Handle, system: *mut SystemTable) -> Status {
    // SAFETY: firmware passes a valid table, and it's used while boot services run.
    let firmware = Firmware { image, system, boot: unsafe { (*system).boot } };
    firmware.print("feos-efi: loading \\feos.elf\n");

    let Some(file) = firmware.read_kernel() else {
        return fail(&firmware, "can't read \\feos.elf from this volume");
    };
    let Some(elf) = Elf::parse(file, ELF_MACHINE) else {
        return fail(&firmware, "\\feos.elf is not a kernel for this machine");
    };
    let Ok(extent) = elf.extent() else {
        return fail(&firmware, "\\feos.elf's segments make no sense");
    };
    let Some(entry) = kernel_entry(&elf) else {
        return fail(&firmware, "\\feos.elf has no entry point for firmware");
    };

    let mut handoff = Handoff::new();
    handoff.framebuffer = firmware.framebuffer();
    firmware.find_tables(&mut handoff);
    firmware.print("feos-efi: framebuffer ");
    firmware.print_hex(handoff.framebuffer.base);
    firmware.print(" rsdp ");
    firmware.print_hex(handoff.rsdp);
    firmware.print(" device tree ");
    firmware.print_hex(handoff.device_tree);
    firmware.print("\n");

    // Below 4 GiB, because the kernel reads this before it can map anything else.
    let Some(place) = firmware.alloc_low(size_of::<Handoff>()) else {
        return fail(&firmware, "no memory for what the kernel needs to know");
    };
    let map_bytes = 256 * 1024;
    let Some(map_memory) = firmware.alloc_any(map_bytes) else {
        return fail(&firmware, "no memory for the memory map");
    };
    // SAFETY: just allocated, and it's only for this.
    let map = unsafe { core::slice::from_raw_parts_mut(map_memory, map_bytes) };

    // Everything that has to survive the copy has to be out of its way. Firmware
    // allocates from the top, so this holds unless RAM's very short.
    let handoff_range = Range { base: place as u64, size: size_of::<Handoff>() as u64 };
    let file_range = Range { base: file.as_ptr() as u64, size: file.len() as u64 };
    let stack = Range { base: stack_pointer().saturating_sub(256 * 1024), size: 512 * 1024 };
    let code = efi_main as *const () as u64;
    let code = Range { base: code.saturating_sub(1024 * 1024), size: 2 * 1024 * 1024 };
    for (what, range) in [
        ("the kernel's information", handoff_range),
        ("the kernel file", file_range),
        ("the stack", stack),
        ("this loader", code),
    ] {
        if extent.overlaps(&range) {
            firmware.print("feos-efi: ");
            firmware.print(what);
            firmware.print(" is in the way of the kernel at ");
            firmware.print_hex(range.base);
            firmware.print("\n");
            return 1;
        }
    }

    // A look at the map while there's still a way to say what's wrong with it.
    let Some((size, stride)) = firmware.memory_map(map) else {
        return fail(&firmware, "no memory map");
    };
    let mut ram = [Range::default(); MAX_RAM_RANGES];
    handoff.ram_count = usable_ram(&map[..size], stride, &mut ram);
    handoff.ram = ram;
    if !is_covered(handoff.ram(), &extent) {
        firmware.print("feos-efi: the kernel's range isn't all free memory: it needs ");
        firmware.print_hex(extent.base);
        firmware.print(" to ");
        firmware.print_hex(extent.end());
        firmware.print("\n");
        walk_memory_map(&map[..size], stride, |kind, base, length| {
            let entry = Range { base, size: length };
            if entry.overlaps(&extent) && !is_usable_type(kind) {
                firmware.print("feos-efi: in the way, type ");
                firmware.print_hex(u64::from(kind));
                firmware.print(" at ");
                firmware.print_hex(base);
                firmware.print(" for ");
                firmware.print_hex(length);
                firmware.print("\n");
            }
        });
        return 1;
    }

    // The last output of this program. Firmware's gone after this.
    firmware.print("feos-efi: leaving boot services\n");
    let Some((size, stride)) = exit_boot_services(&firmware, map) else {
        return fail(&firmware, "firmware won't let go");
    };
    let mut ram = [Range::default(); MAX_RAM_RANGES];
    handoff.ram_count = usable_ram(&map[..size], stride, &mut ram);
    handoff.ram = ram;
    if !is_covered(handoff.ram(), &extent) {
        // No console now, and this is where the image has to go. Stop here: writing
        // over reserved memory can't end well.
        loop {
            halt();
        }
    }

    place_segments(&elf);
    // SAFETY: the page is the handoff's, and is out of the kernel's way.
    unsafe { core::ptr::write_volatile(place.cast::<Handoff>(), handoff) };

    #[cfg(target_arch = "aarch64")]
    {
        clean_to_memory(extent.base, extent.size);
        clean_to_memory(place as u64, size_of::<Handoff>() as u64);
        if handoff.device_tree != 0 {
            // Its size is at offset 4, big-endian.
            // SAFETY: firmware's table, which stays where it is.
            let size = unsafe { core::ptr::read_volatile((handoff.device_tree + 4) as *const u32) };
            clean_to_memory(handoff.device_tree, u64::from(u32::from_be(size)));
        }
    }

    // SAFETY: the image is in place, and this is what it's expecting.
    unsafe { enter_kernel(entry, place as u64) }
}

fn halt() {
    // SAFETY: waiting for an interrupt that won't come.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        asm!("hlt", options(nomem, nostack));
        #[cfg(target_arch = "aarch64")]
        asm!("wfi", options(nomem, nostack));
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        halt();
    }
}
