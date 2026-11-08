# How the filesystem is put together

`libs/fs` defines the read-only `FileSystem` trait, the writable
`WritableFileSystem` trait, and these backends:

* `StaticFs` — the compiled-in tree this file comes from.
* `Fat32` and `Exfat` — read and write, with long or UTF-16 names.
* `Fat16` — FAT12 and FAT16, read-only.
* `Native` — FEOS's own journalled format, with owners, modes and links.
* `RamFs` — a writable tree in RAM.

`boards/common/fs_image_build.rs`, run by each board's `build.rs`, walks
`boards/qemu-virt/fs/` at build time and generates a flat `StaticNode` table,
adding the user programs as `/bin`. To add a file to the image, drop it into
`boards/qemu-virt/fs/` and rebuild the kernel.

At boot, the first disk with a filesystem becomes the root and later ones appear
as `/disk1` and so on (`/native<n>` for native disks, `/usb<n>` for USB sticks).
A blank disk is formatted and seeded from this image. With no disk, the root is a
RAM copy of the image: you can write to it, and the changes last until power-off.
`/tmp` and `/dev` are always in RAM.

```text
df
ls -l /
mount
```
