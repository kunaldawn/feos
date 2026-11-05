# DOS game library

This directory holds one ZIP per game plus the exFAT image that packages
them. Keeping one ZIP per title avoids thousands of loose files in the
repository.

## Sources

All archives were downloaded from [DOSGames.com](https://dosgames.com/) on
2026-11-03. The label column is that site's. The owner has decided to keep the game data
tracked here.

| ZIP | Title | Site label | Source page |
|-----|-------|---------|-------------|
| `doom.zip` | DOOM | Shareware | <https://dosgames.com/game/doom/> |
| `duke-nukem-3d.zip` | Duke Nukem 3D | Shareware | <https://dosgames.com/game/duke-nukem-3d/> |
| `wolfenstein-3d.zip` | Wolfenstein 3D | Shareware | <https://dosgames.com/game/wolfenstein-3d/> |
| `quake.zip` | Quake | Shareware | <https://dosgames.com/game/quake/> |
| `super-street-fighter-ii-turbo.zip` | Super Street Fighter II Turbo | Demo | <https://dosgames.com/game/super-street-fighter-ii-turbo/> |
| `tank-wars.zip` | Tank Wars | Shareware | <https://dosgames.com/game/tank-wars/> |
| `warcraft-2-tides-of-darkness.zip` | Warcraft II: Tides of Darkness | Demo | <https://dosgames.com/game/warcraft-2-tides-of-darkness/> |
| `champ-kong.zip` | Champ Kong | Shareware | <https://dosgames.com/game/champ-kong/> |
| `galacta-the-battle-for-saturn.zip` | Galacta: The Battle for Saturn | Shareware | <https://dosgames.com/game/galacta-the-battle-for-saturn/> |
| `mario-bros-vga.zip` | Mario Bros VGA | Shareware | <https://dosgames.com/game/mario-bros-vga/> |
| `simcity-2000.zip` | SimCity 2000 | Demo | <https://dosgames.com/game/simcity-2000/> |
| `snood.zip` | Snood | Shareware | <https://dosgames.com/game/snood/> |
| `digger-remastered.zip` | Digger Remastered | Freeware | <https://dosgames.com/game/digger-remastered/> |

## What each title's own files say

This is what was found by reading the text files inside each ZIP. **It isn't a legal
review, and no licence is asserted here beyond these quotes.** Whether shipping these files in
this repository and in a kernel image is allowed remains the owner's decision (DOS-7).

| Title | File in the ZIP | What it says |
|-------|-----------------|--------------|
| DOOM | `README.TXT`, `ORDER.FRM` | Calls itself the shareware version, and asks that its levels not be modified. No distribution terms were found in the files read |
| Duke Nukem 3D | `LICENSE.TXT` | "Individuals can -- and are encouraged! -- to give away copies of the Game without charge". Free online services may offer it. "*ALL* OTHER DISTRIBUTION, including ... by CD-ROM, shareware catalog, or retail rack REQUIRES: WRITTEN PERMISSION ... AND PRE-PAYMENT OF A LICENSE FEE". Dated 1996 |
| Wolfenstein 3D | `ORDER.FRM` only | No licence or distribution text found in the files read. Its executables weren't examined for any |
| Quake | `SLICNSE.TXT`, `LICINFO.TXT` | A "Limited Use Software License Agreement": it forbids distributing "for money or any other consideration" and commercial exploitation. The full terms are in that file |
| Super Street Fighter II Turbo | `README.TXT` | Says the demo "has been released". No licence or distribution terms found |
| Tank Wars | `BOMB.TXT`, `BOMB.DOC` | "You are free to try the game and make copies for others", with a registration fee for continued use, and files "may not be modified" without consent. Copyright Kenneth Morse |
| Warcraft II | `VENDOR.TXT` | "Everyone can -- and is encouraged! -- to copy, upload and generally pass around this Program electronically WITHOUT CHARGING FOR IT". On a rack, "as part of a hardware or software bundle, or on CD-ROM, you must get PRIOR signed written permission" |
| Champ Kong | `CKONGSW.TXT` | "It can be freely distributed to any other BBS or online service. It cannot be sold for profit either individually or as part of a bundle" |
| Galacta | none | Only `galacta.exe`; its title screen calls it shareware and asks for registration |
| Mario Bros VGA | `MARIO.DOC` | Not reviewed in detail: no distribution terms matched a search of it. Its title screen reads "For [UNREGISTERED]'s use only" |
| SimCity 2000 | `README.TXT` | Calls itself the demo. No distribution terms found |
| Snood | `readme.txt`, `Registration Info.rtf` | "Snood is shareware, so you can try it out". No distribution terms matched a search of the readme; the RTF wasn't read |
| Digger Remastered | `digger.txt` | The author says Windmill Software's copyright still applies to the original, that they "have not asked me to stop distributing it", and "I can assume they don't mind". That isn't a grant from the copyright holder |

The owner may want to look at Duke Nukem 3D's and Warcraft II's bundling terms first, as they
are the ones that speak directly to being part of something larger.

## Rebuilding the image

The image is 64 MiB with 32 KiB clusters. It is built with `mkfs.exfat` and
FEOS's own writable exFAT implementation:

```sh
truncate -s 64M data/games/dos.img
mkfs.exfat -n FEOSDOS -c 32K data/games/dos.img
cargo run -p feos-fs --example seed_games --target x86_64-unknown-linux-gnu -- \
    data/games/dos.img data/games/dos
cargo run -p feos-fs --example check_games --target x86_64-unknown-linux-gnu -- \
    data/games/dos.img
fsck.exfat -n data/games/dos.img
```

`seed_games` copies each ZIP and `GAMES.TXT` into the image. `check_games`
mounts the image through `feos-fs` and asserts that all 13 ZIPs and
`GAMES.TXT` are present.

## Running a title

```sh
make run-games       # link the image, build, and boot with 512 MiB
```

In FEOS-DOS:

```text
GAMES
MOUNT WOLF3D         (or MOUNT wolfenstein-3d)
CD WOLF3D
WOLF3D.EXE
```

`MOUNT` extracts the ZIP into `C:\<name>` using the FEOS ZIP reader. All 13 titles run:
the table in [`docs/dosbox.md`](../../docs/dosbox.md) says what each one reaches. The DOS/4GW,
DOS/4G and `32RTM` titles use their own extenders, and Quake and Snood use the machine's DPMI
host. Warcraft II and Super Street Fighter II Turbo need the default Sound Blaster 16.
