# The feos desktop

![The feos desktop with the XP-style Start menu open](desktop.png)

`feos-desktop` is the session layer on top of `feos-gui`: wallpaper, desktop
shortcuts, an XP-style taskbar and Start menu, a window manager, and a small
init/service supervisor. It is configured with YAML and discovers applications
from an *application folder* of single-file bundles.

## Booting into the desktop

On a graphical boot the kernel draws the splash, waits 1.5 seconds, and then
starts the desktop automatically. The machine's pointer and keyboard (virtio,
PS/2 or USB) drive it, and the serial console remains a keyboard alongside them:

| Input | Action |
|-------|--------|
| Mouse | Move the pointer; click Start, menus, taskbar buttons and window controls |
| Keyboard | Type into the focused window, terminal or dialog |
| Double-click | Open a desktop shortcut, or maximise/restore a window from its title bar |
| Drag | Move a window by its title bar, or resize it from any edge or corner |
| Cursor | The XP cursor set: arrow over the desktop, horizontal/vertical arrows on edges, diagonal arrows on corners, a text beam over text fields and a hand over links in apps that ask for them, and the animated working pointer for a second after a launch |
| Right-click | Open the desktop menu (Refresh, Show Desktop, Task Manager, Properties) |
| Arrow keys | Move the Start-menu highlight, or the wallpaper selection when the menu is closed |
| `Enter` | Launch the highlighted application |
| `Space` | Open the Start menu from the desktop; toggle All Programs while it is open |
| `Ctrl+Esc` | Open the Start menu even while an application has focus |
| `Alt+Tab`, `Alt+Shift+Tab` | Focus the next or previous window |
| `Alt+F4` | Close the focused window, or open Turn Off Computer with none |
| `Alt+Space` | Open the focused window's menu |
| `Tab` | Toggle All Programs, or focus the next window when the menu is closed |
| `Esc` | Close the Start menu |
| `Ctrl-\` | Leave the desktop and return to the serial shell (in the session, `Ctrl-D` goes to the focused app) |

The Start menu starts closed, like XP: click **start** (or press `Space`) to
open it. Hovering an item moves the highlight, a single click launches it, and
clicking outside closes the menu. **All Programs** opens the full application
list. **Run...** opens a command box that accepts either a bundle id
(`system-info`) or a shell command (`ps`). **Turn Off Computer** opens the XP
confirmation dialog with Turn Off, Restart and Cancel. Clicking the tray clock
opens Date and Time Properties, and the taskbar's Show Desktop button
minimises every window and restores them on a second click. The Start menu's
"Log Off" button asks for confirmation and then behaves like `Ctrl-\`.

The pointer is a virtio tablet (absolute coordinates) or mouse (relative
coordinates), on the virtio MMIO bus on AArch64 and on PCI on q35, a PS/2 mouse
on q35, or a USB mouse or tablet. The keyboard is likewise virtio, PS/2 or USB;
virtio's Linux-evdev transitions are decoded by `feos-keys::EvdevKeys`. The
virtio devices are auto-detected by `feos-driver-virtio-input`. With none
present the desktop still works from the serial keyboard.

Run the same session from the shell:

```text
feos> desktop
[desktop] session started
[desktop] click Start, double-click desktop icons, drag windows; Ctrl-\ returns to the shell
[desktop] 13 application(s) discovered in the application folder (0 skipped)
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
├── window.rs       # z-order, focus, move/resize, minimize/maximize
├── keyboard.rs     # keyboard layouts
├── notify.rs       # tray notifications
├── volume.rs       # the tray's volume control
└── scripted.rs     # host tests that drive whole sessions

libs/hal/           # Pointer and Keyboard traits shared by drivers and the desktop app
libs/init/          # restart policies and service supervision
libs/bundle/        # .fapp reader: manifest, icon, catalog scan, install checks
drivers/input/      # virtio and i8042 (PS/2) input; USB is drivers/usb/xhci
apps/desktop/       # the `desktop` app, session loop, YAML file and embedded app folder
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
├── icon.ico           # ICO, BMP, PNG, JPEG, PPM or QOI, up to 48x48
├── icon-32.ico        # optional: icon_medium, the 32-pixel picture
├── icon-16.ico        # optional: icon_small, the 16-pixel picture
└── ...                # additional resources
```

`manifest.yaml`:

```yaml
id: terminal
name: Terminal
version: 1.0.0
entry: terminal       # command in the shell registry
category: System
description: Bash-compatible shell with POSIX core utilities
icon: icon.ico
icon_medium: icon-32.ico
icon_small: icon-16.ico
args: ""
window:
  width: 760
  height: 520
```

`window.frameless: true` gives the app no title bar or border: the whole
window is its canvas, at exactly `width` by `height`, and it can't be resized
from its edges. Such an app draws its own chrome and asks for moves, resizes
and minimizing through `WindowContext::request` (`WindowRequest::Move`,
`Resize`, `Minimize`), which the desktop applies after the frame. FeAmp is the
example: see `docs/feamp.md`.

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

The bundles shipped with the system are embedded in the kernel image:
`apps/desktop/build.rs` turns every file in `apps/desktop/apps/` into a
`DirEntry` with `include_bytes!`. Adding a bundle there and rebuilding is enough
for it to appear in the Start menu — no code, registry entry or configuration
change.

Bundles can also be installed at run time: `appinstall FILE [SHA256]` copies a
package to `/apps` with its digest beside it, once the whole file matches. The
session scans `/apps` along with the embedded folder, loading only packages that
still match their digest, and scans again when it changes. A package carries
only a manifest and icons: its `entry` still names a command already in the
registry, so no code is loaded.

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
the manifest entry point through `AppRegistry`, and either hosts it in the
window or spawns it through `Processes`. An unknown entry leaves the window open
with an explanation instead of failing silently.

Most GUI apps implement `App::windowed` (File Explorer, Task Manager, Media
Player, Music, Image Viewer, Terminal, Settings, the gallery, FEOS-DOS and the
host for a user program's window), so the
desktop drives them one frame at a time through `App::open_window` and
`App::frame_window`. Their state persists in each app across frames, which
means the desktop keeps running normally while they are open: wallpaper icons
stay visible around the window, the taskbar and Start menu remain clickable,
and several windows can be open at once. `WindowContext` supplies the content
canvas, frame input, the content origin and platform services; pointer events
are routed only to the topmost content area, and keyboard events go to the
focused hosted app. Minimizing preserves the app state, so restoring from the
taskbar resumes the same window instead of restarting the application.

An app picks the pointer over its content by setting `WindowContext::cursor`
during the frame. Apps built on `feos_gui::Ui` pass on `Ui::cursor()`, which
text fields set to the text beam and links to the hand; File Explorer sets it
for its address and search fields and its task links. The desktop shows it
while the pointer is over that window's content and not on a resize edge.
`WindowContext::request` works the same way for a frameless window's moves,
resizes and minimizing.

Apps that return `true` from `App::foreground` but not `App::windowed` still use
the older synchronous path with `Context::viewport` and `Context::suspend_area`
for compatibility; a windowed app is still run that way from the serial shell.
The desktop app's session loop polls the pointer and keyboard through
`feos_hal::Pointer` and `feos_hal::Keyboard` and feeds `feos_gui::Input` events;
the same path forwards `DesktopResponse::launch`.

The boot session composes frames into the board's off-screen back buffer and
copies the finished frame to the display's scanout in one pass. It also redraws
only when input arrives, the clock minute changes, a service or window state
changes, a launch needs a new frame, or an animating windowed app (media
playback, live task-manager graphs) asks for another frame. An idle desktop
leaves the scanout buffer alone.

## Icons and cursors

Every picture the desktop and its apps draw comes from `assets/icons`, through
`feos-iconset` (`libs/iconset`), which embeds only the pictures and sizes in use
and decodes them when drawn. `feos_gui::Icon` draws an iconset picture where one
shows the same thing (`Icon::picture`), and a glyph otherwise: chevrons, arrows,
check marks, plus and minus, close, and symbols the iconset lacks such as the
warning triangle. Bundle icons are copies of iconset files.

The pointer is the Windows XP cursor set in `assets/cursors` (see its README, and
`LICENSING.md`, as it isn't open-licensed). `libs/gui/src/cursor.rs` embeds the
`xp-default` arrow, text beam, link hand, help, precision cross, move,
unavailable and four resize cursors, and the animated busy and working
hourglasses from `animated`; `feos_image::cur` and `feos_image::ani` decode
them. Cursors are 32x32 and placed by their hot spot, so the area the session
restores before drawing the pointer elsewhere is `cursor_bounds`, which starts
above and left of the pointer. An animated pointer is redrawn by the session
loop when its next frame is due, without composing the desktop again.

## Settings

The `settings` bundle (`apps/commands/src/settings.rs`) sets the colour scheme, keyboard layout,
wallpaper, clock and screen resolution: Up and Down pick a row, Left and Right change it, or the
arrows with the pointer. The app holds no settings. It posts `shell::request_setting`, the session
applies and saves them, and publishes the result with `shell::publish_settings`, which is what
the window shows, whoever changed it. Right-click, Properties opens the older Display Properties
dialog, for the scheme and keyboard, and it saves the same way.

They're saved in `/etc/desktop.conf` by `write_file_atomic`, and synced before the session says
so, as `[desktop] settings saved: ...`. So a setting survives the power going straight afterwards.
`FEOS_BOOT_TEST_ONLY=settings` changes settings in both places through the UI, kills QEMU, and looks at
the log and the screen.

## Screen resolution

`feos_hal::display` has the modes a display can be set to, from 640x480 to 1920x1080, and sets one
live: ramfb by writing its configuration again, virtio-gpu by creating a new resource, showing it and
destroying the old. A firmware framebuffer offers none, as boot services have gone, and Settings
then has no resolution row. The session's boot buffers stay static; each later mode's two buffers
come from the heap, page aligned, and the old ones are freed once the session has moved its canvases.

The session then fits itself (`Desktop::screen_changed`): maximised windows take the new work area,
the rest are moved and cut to stay on the screen, the wallpaper's redrawn, and the pointer learns the new
range (`Pointer::set_screen`).

In Settings, Left and Right step through sizes without trying them, and Enter applies one. A prompt
then counts down from 15 seconds: Enter or Keep keeps it, Esc or Revert or silence goes back. Only a
kept size is saved. At the next boot the session sets it again, if the display still can.
`FEOS_BOOT_TEST_ONLY=resolution` does all this on ramfb to 1600x1200 and virtio-gpu to 1920x1080, neither of
which fits the static buffers, with screenshots, 12 rounds of changes to catch a leak, and a kill.

## Init process manager

`libs/init` supplies `Init<N>`, a fixed-capacity supervisor:

- register services by id with an `App` and arguments,
- start, stop and restart by id,
- record ids that have no app as `missing`,
- restart according to `never`, `on-failure` or `always`, going by the exit status: `on-failure`
  leaves a process alone that exited with zero, and counts one killed from outside as failed,
- stop retrying after `max_restarts`,
- run one-shot services (`type: oneshot`), which end as `done` and aren't run again,
- start services in order: `after:` names what a service waits for, a simple service counting
  once it's running and a one-shot once it's done. A service whose dependency has failed, doesn't
  exist or waits for it in turn is `blocked`,
- expose `ServiceStatus` snapshots for a `ps`-style table.

The desktop creates one supervisor from the `init` section, registers
autostart entries in `start_session`, and calls `tick_session` once per frame.
The API is independent of the desktop, so a serial-only board can use it
directly.
