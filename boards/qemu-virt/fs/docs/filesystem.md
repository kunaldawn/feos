# How the filesystem is put together

`libs/fs` defines a read-only `FileSystem` trait and a `StaticFs` backend.
`boards/qemu-virt/build.rs` walks `boards/qemu-virt/fs/` at build time and
generates a flat `StaticNode` table. Each node stores its name, kind, parent
index, child indices and (for a file) a borrowed byte slice.

Adding content:

1. Drop a file into `boards/qemu-virt/fs/`.
2. Rebuild the kernel.

The file appears in the explorer and in `files ls` automatically. There is no
on-disk format to mount and no block-device driver yet.
