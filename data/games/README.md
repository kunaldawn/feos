# DOS game library

This directory holds one ZIP per game plus the exFAT image that packages
them. Keeping one ZIP per title avoids thousands of loose files in the
repository.

## Sources

All archives were downloaded from [DOSGames.com](https://dosgames.com/) on
2026-11-03. The site labels each title; the licence column below is the site's
label. Each game's own readme/licence file is inside its ZIP.

| ZIP | Title | Licence | Source page |
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

Shareware may be copied and shared; demos are freely distributable; Digger
Remastered is freeware. The full versions are available from the publishers or
GOG. All copyrights and trademarks remain with their respective owners.

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

`MOUNT` extracts the ZIP into `C:\<name>` using the FEOS ZIP reader. The
current FEOS-DOS release is an 8086/80186 real-mode machine; the DOS/4GW,
DOS/4G, DJGPP and CWSDPMI titles need protected mode and DOS extenders and
will not run yet. See [`docs/dosbox.md`](../../docs/dosbox.md) for the detailed
compatibility status.
