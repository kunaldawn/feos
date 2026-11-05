//! Builds the tree a board boots with, from whatever storage it found.
//!
//! The board finds devices; this decides what they mean. The first disk with a
//! filesystem becomes the root and later ones appear under `/disk1` and so on. A
//! disk that's blank from its first sector is formatted and seeded from the
//! embedded image: anything else unrecognised is left strictly alone. With no
//! usable disk, the root is a RAM copy of the embedded image, which is what the
//! system has always done.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use feos_hal::BlockIo;

use super::{join_path, DeviceKind, RamFs, Vfs, VfsTree, DEFAULT_QUOTA};
use crate::block::{BlockDevice, DriverDevice};
use crate::cache::BufferCache;
use crate::partition::{read_partitions, Partition, PartitionInfo};
use crate::volume::{detect, VolumeKind};
use crate::{attr, mount, EntryKind, FileSystem, FsError, NodeId, WritableFileSystem};

/// Cache in front of each mounted volume. The heap is 64 MiB.
const CACHE_BYTES: usize = 4 * 1024 * 1024;
/// RAM for `/tmp`.
const TMP_QUOTA: usize = 16 * 1024 * 1024;
/// How much of a disk must read as zeros for it to count as blank.
const BLANK_PROBE_BLOCKS: u64 = 64;
/// Marks a root as seeded, so that deleting everything isn't undone next boot.
const SEED_MARKER: &str = "/.feos-seeded";
/// Written for people who find the marker.
const SEED_MARKER_TEXT: &[u8] = b"feos copied its sample files here once. Remove this to do it again.\n";
/// A disk's name in logs and `mount`.
const MAX_DISKS: usize = 8;

/// What's linked into the kernel.
pub struct Embedded {
    /// The compiled-in tree.
    pub tree: &'static dyn FileSystem,
    /// A disk image that replaces the tree as the root, when present.
    pub root_image: Option<&'static [u8]>,
    /// Further images, each mounted at `/<name>`.
    pub images: &'static [(&'static str, Option<&'static [u8]>)],
}

/// Messages for the board to print.
pub type BootLog = Vec<String>;

fn leak_image(image: &'static [u8]) -> Result<&'static dyn FileSystem, FsError> {
    let fs = mount(crate::MemoryBlockDevice::new(image))?;
    Ok(Box::leak(fs))
}

/// One filesystem found on a disk.
struct Found {
    fs: Box<dyn WritableFileSystem>,
    source: String,
    fresh: bool,
}

fn is_blank<D: BlockDevice>(device: &D) -> Result<bool, FsError> {
    let blocks = BLANK_PROBE_BLOCKS.min(device.block_count());
    if blocks == 0 {
        return Ok(false);
    }
    let mut buffer = crate::tree::try_zeroed(device.block_size())?;
    for block in 0..blocks {
        device.read_block(block, &mut buffer)?;
        if buffer.iter().any(|&byte| byte != 0) {
            return Ok(false);
        }
    }
    // A GPT also keeps a copy at the far end: the front being blank isn't enough.
    device.read_block(device.block_count() - 1, &mut buffer)?;
    Ok(buffer.iter().all(|&byte| byte == 0))
}

fn mount_volume<T: BlockIo + 'static>(
    device: &'static DriverDevice<T>,
    start: u64,
    count: u64,
    source: String,
    fresh: bool,
    log: &mut BootLog,
) -> Result<Found, FsError> {
    let partition = Partition::new(device, start, count)?;
    let cache = BufferCache::new(partition, CACHE_BYTES)?;
    let mut fs = crate::mount_writable(cache)?;
    match fs.check() {
        Ok(report) if report.was_dirty => {
            if report.found_nothing() {
                log.push(format!("[fs] {source}: was not cleanly unmounted, found no damage"));
            } else {
                log.push(format!(
                    "[fs] {source}: repaired after a crash: {} lost clusters, {} files shortened, \
                     {} duplicate entries, {} orphan names",
                    report.lost_clusters,
                    report.truncated_files,
                    report.removed_entries,
                    report.orphan_names
                ));
            }
        }
        Ok(_) => {}
        // Still readable; the volume refuses writes from here on.
        Err(error) => log.push(format!("[fs] {source}: check failed ({error}), mounted read-only")),
    }
    if fs.is_damaged() {
        log.push(format!("[fs] {source}: damaged, mounted read-only"));
    }
    Ok(Found { fs, source, fresh })
}

/// A disk that isn't the system's own, whatever kind of device it is.
struct BoxedDisk(Box<dyn BlockIo>);

impl BlockIo for BoxedDisk {
    fn block_size(&self) -> usize {
        self.0.block_size()
    }
    fn block_count(&self) -> u64 {
        self.0.block_count()
    }
    fn is_read_only(&self) -> bool {
        self.0.is_read_only()
    }
    fn read(&mut self, start: u64, out: &mut [u8]) -> Result<(), feos_hal::BlockIoError> {
        self.0.read(start, out)
    }
    fn write(&mut self, start: u64, data: &[u8]) -> Result<(), feos_hal::BlockIoError> {
        self.0.write(start, data)
    }
    fn flush(&mut self) -> Result<(), feos_hal::BlockIoError> {
        self.0.flush()
    }
}

/// Finds and mounts every filesystem on one disk. A blank one is formatted only if
/// `may_format`: it's someone's own disk that this kernel just happens to be next to.
fn open_disk<T: BlockIo + 'static>(driver: T, name: String, may_format: bool, log: &mut BootLog) -> Vec<Found> {
    let device: &'static DriverDevice<T> = Box::leak(Box::new(DriverDevice::new(driver)));
    let blocks = device.block_count();
    let mut found = Vec::new();

    let mut ranges: Vec<(PartitionInfo, String)> = Vec::new();
    match detect(&device) {
        Ok(_) => {
            ranges.push((
                PartitionInfo {
                    start: 0,
                    count: blocks,
                    hint: crate::PartitionHint::WholeDisk,
                },
                name.clone(),
            ));
        }
        Err(_) => match read_partitions(&device) {
            Ok(partitions) => {
                for (number, info) in partitions.into_iter().enumerate() {
                    ranges.push((info, format!("{name}p{}", number + 1)));
                }
            }
            Err(error) => log.push(format!("[fs] {name}: partition table unreadable: {error}")),
        },
    }

    if ranges.is_empty() {
        match is_blank(&device) {
            Ok(true) if may_format && !device.is_read_only() => match crate::format(&mut &*device, "FEOS") {
                Ok(()) => {
                    let kind = match detect(&device) {
                        Ok(VolumeKind::Exfat) => "exFAT",
                        _ => "FAT32",
                    };
                    log.push(format!("[fs] {name}: blank disk, formatted as {kind}"));
                    match mount_volume(device, 0, blocks, name.clone(), true, log) {
                        Ok(volume) => found.push(volume),
                        Err(error) => log.push(format!("[fs] {name}: {error}")),
                    }
                }
                Err(error) => log.push(format!("[fs] {name}: can't format: {error}")),
            },
            Ok(_) => log.push(format!("[fs] {name}: no filesystem recognised, left alone")),
            Err(error) => log.push(format!("[fs] {name}: {error}")),
        }
        return found;
    }

    for (info, source) in ranges {
        // A partition can hold anything, and this crate reads only some of it.
        let view = match Partition::new(device, info.start, info.count) {
            Ok(view) => view,
            Err(_) => continue,
        };
        match detect(&view) {
            Ok(VolumeKind::Fat32 | VolumeKind::Exfat) => {
                match mount_volume(device, info.start, info.count, source.clone(), false, log) {
                    Ok(volume) => found.push(volume),
                    Err(error) => log.push(format!("[fs] {source}: not mounted: {error}")),
                }
            }
            Ok(VolumeKind::Fat16) => match crate::Fat16::mount(view) {
                Ok(fs) => {
                    // Read-only by nature: seeded into RAM like an embedded image.
                    let fs: &'static dyn FileSystem = Box::leak(Box::new(fs));
                    match RamFs::seeded(fs, DEFAULT_QUOTA) {
                        Ok(ram) => found.push(Found {
                            fs: Box::new(ram),
                            source,
                            fresh: false,
                        }),
                        Err(error) => log.push(format!("[fs] {source}: not mounted: {error}")),
                    }
                }
                Err(error) => log.push(format!("[fs] {source}: not mounted: {error}")),
            },
            Err(_) => log.push(format!("[fs] {source}: no filesystem recognised")),
        }
    }
    found
}

/// Copies an embedded tree onto a real disk.
fn seed(vfs: &Vfs, from: &dyn FileSystem, node: NodeId, path: &str, depth: usize) -> Result<(), FsError> {
    if depth > 32 {
        return Err(FsError::TooDeep);
    }
    for index in 0..from.child_count(node) {
        let Some(child) = from.child_at(node, index) else {
            continue;
        };
        let Some(name) = from.name(child) else {
            continue;
        };
        let target = join_path(path, name);
        match from.kind(child) {
            Some(EntryKind::Directory) => {
                match vfs.create_dir(&target) {
                    Ok(()) | Err(FsError::NameExists) => {}
                    Err(error) => return Err(error),
                }
                seed(vfs, from, child, &target, depth + 1)?;
            }
            Some(EntryKind::File) => {
                let data = from.read_to_vec(child)?;
                vfs.write_file(&target, &data, false)?;
            }
            None => {}
        }
    }
    Ok(())
}

/// Builds the system's tree.
///
/// `disks` come in probe order. `removable` are the likes of USB sticks, which are
/// never the root and never formatted: each filesystem on them goes at `/usb<n>`.
/// `now` is the clock for timestamps. Returns the tree and what's worth telling the
/// user.
pub fn assemble<T: BlockIo + 'static>(
    embedded: Embedded,
    disks: Vec<T>,
    removable: Vec<Box<dyn BlockIo>>,
    now: Option<crate::TimeSource>,
) -> (&'static Vfs, BootLog) {
    let mut log = BootLog::new();

    // The image the root starts from, or that `/rom` shows when a disk is root.
    let mut lower: &'static dyn FileSystem = embedded.tree;
    if let Some(image) = embedded.root_image {
        match leak_image(image) {
            Ok(fs) => lower = fs,
            Err(error) => log.push(format!("[fs] disk image rejected: {error}")),
        }
    }

    let mut volumes: Vec<Found> = Vec::new();
    for (index, driver) in disks.into_iter().enumerate().take(MAX_DISKS) {
        volumes.extend(open_disk(driver, format!("virtio{index}"), true, &mut log));
    }

    let mut volumes = volumes.into_iter();
    let vfs: Vfs = match volumes.next() {
        Some(root) => {
            log.push(format!("[fs] {}: mounted at /", root.source));
            let fresh = root.fresh;
            let vfs = Vfs::new(root.fs, false, &root.source);
            if let Some(now) = now {
                vfs.set_time_source(now);
            }
            seed_root(&vfs, lower, fresh, &mut log);
            vfs
        }
        None => {
            let ram = match RamFs::seeded(lower, DEFAULT_QUOTA) {
                Ok(ram) => ram,
                Err(error) => {
                    log.push(format!("[fs] embedded image not usable: {error}"));
                    RamFs::new(DEFAULT_QUOTA)
                }
            };
            log.push(String::from("[fs] no disk: root is in RAM and changes are lost at reboot"));
            Vfs::new(Box::new(ram), false, "ram")
        }
    };
    if let Some(now) = now {
        vfs.set_time_source(now);
    }

    let rooted_on_disk = vfs.mounts().first().is_some_and(|root| root.source != "ram");
    if rooted_on_disk {
        match RamFs::seeded(lower, 0) {
            Ok(rom) => {
                let _ = vfs.mount("/rom", Box::new(rom), true, "embedded");
            }
            Err(error) => log.push(format!("[fs] /rom not available: {error}")),
        }
    }

    for (number, volume) in volumes.enumerate() {
        let path = format!("/disk{}", number + 1);
        match vfs.mount(&path, volume.fs, false, &volume.source) {
            Ok(()) => log.push(format!("[fs] {}: mounted at {path}", volume.source)),
            Err(error) => log.push(format!("[fs] {}: not mounted: {error}", volume.source)),
        }
    }

    let mut number = 0;
    for (index, driver) in removable.into_iter().enumerate().take(MAX_DISKS) {
        for volume in open_disk(BoxedDisk(driver), format!("usb{index}"), false, &mut log) {
            let path = format!("/usb{number}");
            number += 1;
            match vfs.mount(&path, volume.fs, false, &volume.source) {
                Ok(()) => log.push(format!("[fs] {}: mounted at {path}", volume.source)),
                Err(error) => log.push(format!("[fs] {}: not mounted: {error}", volume.source)),
            }
        }
    }

    for (name, image) in embedded.images {
        let Some(image) = image else {
            continue;
        };
        let mounted = leak_image(image)
            .and_then(|fs| RamFs::seeded(fs, DEFAULT_QUOTA))
            .and_then(|ram| {
                vfs.mount(&format!("/{name}"), Box::new(ram), false, name)
            });
        match mounted {
            Ok(()) => log.push(format!("[fs] {name} image mounted at /{name}")),
            Err(error) => log.push(format!("[fs] {name} image rejected: {error}")),
        }
    }

    let _ = vfs.mount("/tmp", Box::new(RamFs::new(TMP_QUOTA)), false, "tmp");
    let mut dev = RamFs::new(0);
    let _ = dev.add_device(NodeId::ROOT, "null", DeviceKind::Null);
    let _ = dev.add_device(NodeId::ROOT, "zero", DeviceKind::Zero);
    let _ = vfs.mount("/dev", Box::new(dev), false, "dev");

    (Box::leak(Box::new(vfs)), log)
}

fn seed_root(vfs: &Vfs, lower: &dyn FileSystem, fresh: bool, log: &mut BootLog) {
    if vfs.lstat(SEED_MARKER).is_ok() {
        return;
    }
    let empty = vfs.read_dir("/").is_ok_and(|entries| entries.is_empty());
    if !fresh && !empty {
        return;
    }
    let result = seed(vfs, lower, lower.root(), "/", 0).and_then(|()| {
        vfs.write_file(SEED_MARKER, SEED_MARKER_TEXT, false)?;
        vfs.set_attributes(SEED_MARKER, attr::HIDDEN)?;
        vfs.sync()
    });
    match result {
        Ok(()) => log.push(String::from("[fs] copied the sample files to the new disk")),
        // A disk without the samples is still a good disk.
        Err(error) => log.push(format!("[fs] couldn't copy the sample files: {error}")),
    }
}
