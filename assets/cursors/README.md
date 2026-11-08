# Cursors

284 mouse pointers from the "Windows XP" cursor set by nibbler
(<http://www.rw-designer.com/user/55973>), downloaded from
<http://www.rw-designer.com/cursor-set/windows-xp-1>. `readme.txt` is the set's
own readme, kept verbatim; `windows-xp-1.crs` is its scheme file, which still
names the original files. These cursors are **not** open-licensed: see the
Cursors section of `LICENSING.md`.

Every image is 32x32. 216 are static `.cur` files and 68 animated `.ani` files.
The images are BMP-style (DIB) payloads at 1, 4, 8 or 32 bits with an AND mask;
none holds a PNG.

## Layout

Files are stored as `<theme>/<role>[-<variant>].cur|ani`.

| Theme | Files | What it is | Original names |
|---|---|---|---|
| `xp-default` | 30 | The set's own scheme (`windows-xp-1.crs`), plus the wheel-pan cursors and two extras | `default_*`, `cross`, `pin`, `person` |
| `standard` | 30 | White Windows Standard pointers, large and extra large | `*_m`, `*_l` |
| `black` | 45 | Windows Black, at three sizes | `*_r`, `*_rm`, `*_rl` |
| `inverted` | 45 | Windows Inverted, at three sizes; they invert the screen under them | `*_i`, `*_im`, `*_il` |
| `magnified` | 15 | Large outlined pointers | `l*` |
| `3d-gold` | 23 | 3D pointers, gold | `3dg*` |
| `3d-blue` | 23 | 3D pointers, light blue | `3ds*` |
| `3d-silver` | 23 | 3D pointers, white and grey | `3dw*` |
| `animated` | 17 | Animated hourglasses and resize arrows, with still versions | `appstar*`, `appstart`, `hourgla*`, `size*.ani` |
| `hands` | 9 | Gloved hands | `h*.cur` |
| `hands-animated` | 8 | Animated gloved hands | `hand*.ani` |
| `novelty` | 16 | Animated novelty pointers, named after what they show | `banana`, `horse`, `metronom`, ... |

The 3D themes are named after the colours they show. The readme calls them gold,
silver and blue, and the file prefixes are `3dg`, `3ds` and `3dw`.

## Naming

Roles follow the names in the Windows Mouse Properties dialog:

| Role | Windows name |
|---|---|
| `arrow` | Normal Select |
| `help` | Help Select |
| `working` | Working In Background |
| `busy` | Busy |
| `precision` | Precision Select |
| `text` | Text Select |
| `handwriting` | Handwriting |
| `unavailable` | Unavailable |
| `size-ns`, `size-we` | Vertical and Horizontal Resize |
| `size-nwse`, `size-nesw` | Diagonal Resize 1 and 2 |
| `move` | Move |
| `alternate` | Alternate Select |
| `link` | Link Select |
| `location`, `person` | Location Select, Person Select |
| `pan-<direction>`, `pan-all` | Wheel-button panning (`n`, `ne`, `e`, ..., `ns`, `ew`) |

Other names describe the picture: `no-drop` (a slashed circle with no arrow),
`link-pixel` (a pixellated link hand), `arrow-twist`, `arrow-wagtail`, `pinch`,
`box`, `dvd`, and the novelty names.

Suffixes after the role:

- `-large`, `-extra-large`: the larger sizes of a Windows scheme (`_m` and `_l` in the original names, `_rm`/`_rl` and `_im`/`_il` for black and inverted)
- `-animated`: an animated version of a cursor that also has a still one
- `-gold`, `-silver`, `-cyan`: colour versions in `animated`
- `-static`: a still picture of an animated cursor

## Index

`index.tsv` is tab-separated and has one row per file:

```
path  original  role  size  hotspot  frames  steps  bits  description
```

`path` is relative to this directory and `original` is the file name in the
download. `size` and `hotspot` are those of the first image. `frames` is how many
images an `.ani` holds and `steps` how many it shows per cycle (a `seq ` chunk can
repeat frames). `bits` lists the colour depths used.

## Use in feos

`libs/gui/src/cursor.rs` embeds only the cursors the desktop draws, all from
`xp-default` except the two animated ones from `animated` (`busy.ani` and
`working.ani`). `feos_image::cur` and `feos_image::ani` decode them.
