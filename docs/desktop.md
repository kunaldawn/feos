# The feos desktop

![The feos desktop with the XP-style Start menu open](desktop.png)

`feos-desktop` is the session layer on top of `feos-gui`: wallpaper, desktop
shortcuts, an XP-style taskbar and Start menu, a window manager, and a small
init/service supervisor. It is configured with YAML and discovers applications
from an *application folder* of single-file bundles.

## Booting into the desktop

On a graphical boot the kernel draws the splash, waits 1.5 seconds, and then
starts the desktop automatically. QEMU supplies a virtio tablet, so the desktop
is driven with a real pointer. The serial console remains the keyboard:

| Input | Action |
|-------|--------|
| Mouse | Move the pointer; click Start, menus, taskbar buttons and window controls |
| Double-click | Open a desktop shortcut, or maximise/restore a window from its title bar |
| Drag | Move a window by its title bar, or resize it from any edge or corner |
| Cursor | Follows XP: arrow over the desktop, horizontal/vertical arrows on edges, diagonal arrows on corners |
| Right-click | Open the desktop menu (Refresh, Show Desktop, Task Manager, Properties) |
| Arrow keys | Move the Start-menu highlight, or the wallpaper selection when the menu is closed |
| `Enter` | Launch the highlighted application |
| `Space` | Open the Start menu; toggle All Programs while it is open |
| `Tab` | Toggle All Programs, or focus the next window when the menu is closed |
| `Esc` | Close the Start menu |
| `Ctrl-\` | Leave the desktop and return to the serial shell (`Ctrl-D` also works) |

The Start menu starts closed, like XP: click **start** (or press `Space`) to
open it. Hovering an item moves the highlight, a single click launches it, and
clicking outside closes the menu. **All Programs** opens the full application
list. **Run...** opens a command box that accepts either a bundle id
(`system-info`) or a shell command (`ps`). **Turn Off Computer** opens the XP
confirmation dialog with Turn Off, Restart and Cancel. Clicking the tray clock
opens Date and Time Properties, and the taskbar's Show Desktop button
minimises every window and restores them on a second click. The Start menu's
"Log Off" button asks for confirmation and then behaves like `Ctrl-\`.

The pointer is a `virtio-tablet-device` (absolute coordinates) or
`virtio-mouse-device` (relative coordinates) on QEMU's virtio MMIO bus. Both
are auto-detected by `feos-driver-virtio-input`; when neither is present the
desktop still works from the serial keyboard.

Run the same session from the shell:

```text
feos> desktop
[desktop] session started
[desktop] click Start, double-click desktop icons, drag windows; Ctrl-\ returns to the shell
[desktop] 7 application(s) discovered in the application folder (0 skipped)
```

Other forms are useful on a serial-only boot:

| Command | Description |
|---------|-------------|
| `desktop` | Run the interactive desktop session (`session` and `boot` are aliases) |
| `desktop list` | List discovered applications |
| `desktop status` | Print configuration, windows and service state |
| `desktop run <id>` | Open and launch an application by bundle id |
| `desktop menu` | Render with the Start menu open (demo/screenshots) |
| `desktop service <action> <id>` | Start, stop or restart one supervised service |
| `wm` | Alias for `desktop` |

## Layout

```
libs/desktop/src/
├── lib.rs          # Desktop session: frames, launching, init integration
├── config.rs       # YAML schema and parsing
├── theme.rs        # xp-blue, xp-olive, xp-silver, dark, light, high-contrast
├── paint.rs        # wallpaper, icon scaling, clock, Start flag, pointer
├── taskbar.rs      # Start button, quick launch, task buttons, tray
├── start_menu.rs   # pinned apps, All Programs, commands, Run dialog, power footer
└── window.rs       # z-order, focus, move/resize, minimize/maximize

libs/hal/           # Pointer trait shared by drivers and the desktop app
libs/init/          # restart policies and service supervision
libs/bundle/        # .fapp reader: manifest, icon, catalog scan
drivers/input/virtio/  # virtio MMIO tablet/mouse driver
apps/desktop/       # the `desktop` app, YAML file and embedded app folder
```

The desktop library is allocation-free, `no_std`, and has no dependency on the
kernel or the board. `Desktop::render` returns a `DesktopResponse`; the caller
decides how a launch request becomes a process. That is what lets the whole
session be tested on the host.

## Configuration

The shipped configuration lives in `apps/desktop/desktop.yaml` and is embedded
with `include_str!`. A board with a filesystem can read its own file and pass
the string to `DesktopConfig::parse` instead.

```yaml
theme: xp-blue            # xp-blue, xp-olive, xp-silver, dark, light, high-contrast

wallpaper:
  style: gradient         # gradient, solid, grid
  top: "#3f8cf3"
  bottom: "#245edb"
  accent: "#549cf6"       # grid lines

taskbar:
  position: bottom        # bottom or top
  height: 40
  start_label: start
  show_clock: true
  clock_24h: true
  show_quick_launch: true
  quick_launch_count: 3
  show_desktop: true

start_menu:
  width: 448
  height: 470
  columns: 2
  show_descriptions: true
  initially_open: false

desktop:
  show_icons: true
  icon_size: 48
  show_labels: true

windows:
  default_width: 560
  default_height: 380
  cascade: 24

apps:
  directories:
    - apps/desktop/apps
  extensions:
    - .fapp

init:
  restart: on-failure     # never, on-failure, always
  max_restarts: 3
  autostart:
    - terminal
```

Unknown keys and invalid values keep their defaults, so a newer configuration
still boots on an older build. Malformed YAML is reported by
`DesktopConfig::parse`.

`init.autostart` ids are resolved through the catalog first (a bundle id such
as `terminal`) and then through the shell registry (a command such as `ps`).
Ids that resolve to nothing appear in `desktop status` as `missing`.

## Application bundles

An application is one compressed file — a ZIP or TAR archive — with everything
it needs inside:

```text
my-app.fapp
├── manifest.yaml
├── icon.bmp           # BMP, PNG, JPEG, PPM or QOI, up to 48x48
└── ...                # additional resources
```

`manifest.yaml`:

```yaml
id: terminal
name: Terminal
version: 1.0.0
entry: gui            # command in the shell registry
category: System
description: GUI dashboard and component gallery
icon: icon.bmp
args: ""
window:
  width: 760
  height: 520
```

The scanner detects bundles from their archive magic, not from the file
suffix, so a renamed bundle still shows up. Files that are not archives are
ignored; malformed bundles are counted as skipped. Duplicate ids are rejected
after the first bundle wins.

Pack a directory with:

```sh
scripts/pack-app.py apps/desktop/bundle-src/terminal apps/desktop/apps/terminal.fapp
```

or repack every source directory:

```sh
make bundles
```

### The application folder

On the QEMU port there is no block device yet, so the application folder is
embedded in the kernel image. `apps/desktop/build.rs` turns every file in
`apps/desktop/apps/` into a `DirEntry` with `include_bytes!`. Adding a bundle
there and rebuilding is enough for it to appear in the Start menu — no code,
registry entry or configuration change.

A board with a filesystem passes the same `&[DirEntry]` list to
`Desktop::scan`, and can call it again whenever the folder changes. Detection
is dynamic in both cases; only the source of the bytes differs.

## Window manager and taskbar

`WindowManager` is a fixed-capacity table (`MAX_WINDOWS`, default 8). Windows
cascade from the top-left of the work area, are clamped so they stay reachable,
and support focus, z-order, drag, resize, minimize, maximize and restore. A
title-bar click focuses, a drag moves, and a double-click maximises or restores.
Every side and corner resizes (the lower-right grip is just the visible one),
and resized windows are kept inside the work area so they never cover the
taskbar. The taskbar reflows to a bottom or top edge and sizes its buttons from
the open window count; clicking the focused task button minimises it (normal or
maximised), clicking a minimised one restores it to its previous normal or
maximised state, and clicking another window raises it.

`Desktop::launch(index, registry, processes)` opens the window first, resolves
the manifest entry point through `AppRegistry`, and spawns it through
`Processes`. An unknown entry leaves the window open with an explanation
instead of failing silently. An app that returns `true` from
`App::foreground` (currently the File Explorer, Task Manager and Media Player)
is run synchronously by the desktop session so it can draw and read the keyboard
and pointer. The desktop passes its work area as `Context::viewport`, so the app
draws above the taskbar instead of covering it. It also passes the app's
taskbar button as `Context::suspend_area`: clicking that button makes the app
return, the desktop minimises its window, and clicking the button again restores
the window and runs the app. A normal close still closes the window. The
File Explorer and Task Manager windows resize from every edge and corner as
well, and use the same resize cursors as the desktop window manager. The
desktop app's session
loop polls the virtio pointer through `feos_hal::Pointer` and feeds
`feos_gui::Input` events; the same path forwards `DesktopResponse::launch`.

The boot session composes frames into the board's off-screen back buffer and
copies the finished frame to the ramfb scanout in one pass. It also redraws
only when input arrives, the clock minute changes, a service or window state
changes, or a launch needs a new frame, so an idle desktop leaves the scanout
buffer alone.

## Init process manager

`libs/init` supplies `Init<N>`, a fixed-capacity supervisor:

- register services by id with an `App` and arguments,
- start, stop and restart by id,
- record ids that have no app as `missing`,
- restart according to `never`, `on-failure` or `always`,
- stop retrying after `max_restarts`,
- expose `ServiceStatus` snapshots for a `ps`-style table.

The desktop creates one supervisor from the `init` section, registers
autostart entries in `start_session`, and calls `tick_session` once per frame.
The API is independent of the desktop, so a serial-only board can use it
directly.
