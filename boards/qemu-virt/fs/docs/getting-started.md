# Getting started

The feos filesystem is a small read-only tree compiled into the kernel. Nodes
are files and directories; file contents are borrowed from the image, so no
allocator is needed.

Try the explorer:

1. Press Space to open the Start menu.
2. Choose **File Explorer** and press Enter.
3. Use the arrow keys to move, Enter or Right to open a directory, and
   Backspace or Left to go back up.
4. Press Esc to return to the desktop.

The same data is available from the shell:

```text
feos> files ls /docs
feos> files cat /README.txt
feos> files tree /
```
