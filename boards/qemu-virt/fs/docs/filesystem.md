# How the filesystem is put together

`libs/fs` defines the read-only `FileSystem` trait, the writable
`WritableFileSystem` trait, and three backends:

* `StaticFs` — the compiled-in tree used by this image.
* `Fat32` — FAT32 with long file names, cluster chains and read/write support.
* `Exfat` — exFAT with UTF-16 names, allocation bitmaps and read/write support.

`boards/qemu-virt/build.rs` walks `boards/qemu-virt/fs/` at build time and
generates a flat `StaticNode` table. Each node stores its name, kind, parent
index, child indices and (for a file) a borrowed byte slice.

Adding content:

1. Drop a file into `boards/qemu-virt/fs/`.
2. Rebuild the kernel.

The file appears in the explorer and in `files ls` automatically.

A FAT32 or exFAT image placed at `boards/qemu-virt/disk.img` is probed at boot
and mounted instead of the compiled-in tree. The board has no block-device
driver yet, so that optional mount is read-only; the `feos-fs` read/write
backends are available to any board that supplies a `WritableBlockDevice`.

`make docker-run` creates `disk-fat32.img` and `disk-exfat.img` inside the
container, seeds them with the `feos-fs` write API, and mounts both together.
They appear as `/fat32` and `/exfat`:

```text
files tree /
files cat /fat32/hello.txt
files cat /exfat/café.txt
```
