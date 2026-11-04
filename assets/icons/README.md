# Icons

1296 desktop icons, sorted by what they show. See LICENSING.md for licensing.

## Layout

Files are stored as `<category>/<name>-<width>.ico`. Each `.ico` holds one image.

| Category | Files |
|---|---|
| `apps` | 72 |
| `computers` | 28 |
| `devices` | 176 |
| `drives` | 208 |
| `files` | 136 |
| `folders` | 215 |
| `logos` | 27 |
| `media` | 30 |
| `misc` | 6 |
| `navigation` | 47 |
| `network` | 68 |
| `status` | 59 |
| `system` | 209 |
| `users` | 15 |
| Total | 1296 |

## Naming

Names are kebab-case and describe the picture. The last part is the image width,
or `<width>x<height>` for non-square images.

Suffixes before the size separate versions of one picture:

- none: the richest version available
- `-8bit`: 256-colour version of a picture that also has a 32-bit version
- `-classic`: 16-colour version with the flatter, older art
- `-mono`: monochrome version
- `-24bit`: 24-bit version
- `-padded`: drawn smaller with a margin around it (the 16 px files usually fill the frame)
- `-alt`: same pixels as another icon, stored as a separate file

`misc/unknown-*` marks a picture that could not be identified.

## Index

`index.tsv` is tab-separated and has one row per stored file:

```
path  width  height  bits  source_ids  description
```

`path` is relative to this directory. `bits` is the colour depth. `source_ids` is
a comma-separated list of the original file numbers; byte-identical duplicates are
stored once and share a row.
