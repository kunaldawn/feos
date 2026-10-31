//! `files`: a graphical file explorer plus text-mode filesystem commands.
//!
//! `files` with no arguments opens a full-screen explorer. It is a foreground
//! app (`App::foreground`), so launching it from the desktop hands over the
//! framebuffer until the user presses Esc.
//!
//! Text-mode forms are useful from the serial shell:
//!
//! ```text
//! files ls [path]
//! files cat <path>
//! files tree [path]
//! files stat <path>
//! ```

use core::fmt::Write;

use feos_app::{App, Context};
use feos_console::{print, println};
use feos_fs::{EntryKind, FileSystem, NodeId};
use feos_gfx::{Canvas, Rect};
use feos_gui::{
    paint_icon, Align, Icon as GuiIcon, Input, Key, Painter, RectExt, Theme, VerticalAlign,
};
use feos_keys::SerialKeys;
use feos_text::Text;

/// Maximum number of bytes shown in the explorer's preview pane.
const PREVIEW_MAX: usize = 4096;
/// Maximum absolute path length the explorer displays.
const PATH_MAX: usize = 256;
/// Row height in the file list.
const ROW_HEIGHT: i32 = 30;

/// The `files` application.
pub struct Files;

impl App for Files {
    fn name(&self) -> &'static str {
        "files"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["explorer"]
    }

    fn usage(&self) -> &'static str {
        "files [ls|cat|tree|stat] [path]"
    }

    fn description(&self) -> &'static str {
        "browse the read-only filesystem"
    }

    fn foreground(&self) -> bool {
        true
    }

    fn run(&self, context: &mut Context<'_>) {
        let Some(fs) = context.platform.filesystem else {
            println!("files: no filesystem is available on this board");
            return;
        };

        let args = context.args.trim();
        if args.is_empty() {
            run_explorer(fs, context);
        } else {
            run_command(fs, args);
        }
    }
}

fn run_command(fs: &dyn FileSystem, args: &str) {
    let mut parts = args.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("").trim();

    match command {
        "ls" | "list" => list(fs, if path.is_empty() { "/" } else { path }),
        "cat" | "read" => cat(fs, path),
        "tree" => tree(fs, if path.is_empty() { "/" } else { path }),
        "stat" | "info" => stat(fs, path),
        other => println!("files: unknown command '{other}' (try ls, cat, tree or stat)"),
    }
}

fn list(fs: &dyn FileSystem, path: &str) {
    let Some(node) = fs.resolve(path) else {
        println!("files: {path}: not found");
        return;
    };
    if fs.is_file(node) {
        print_entry(fs, node);
        return;
    }

    let count = fs.child_count(node);
    println!("{count} item(s) in {path}");
    for index in 0..count {
        let Some(child) = fs.child_at(node, index) else {
            continue;
        };
        print_entry(fs, child);
    }
}

fn print_entry(fs: &dyn FileSystem, node: NodeId) {
    let kind = fs.kind(node).map(EntryKind::short).unwrap_or('?');
    let name = fs.name(node).unwrap_or("?");
    if fs.is_dir(node) {
        println!("  {kind}  {:<10}  {name}/", format_size(fs.size(node)));
    } else {
        println!("  {kind}  {:<10}  {name}", format_size(fs.size(node)));
    }
}

fn cat(fs: &dyn FileSystem, path: &str) {
    if path.is_empty() {
        println!("files: usage: files cat <path>");
        return;
    }
    let Some(node) = fs.resolve(path) else {
        println!("files: {path}: not found");
        return;
    };
    if !fs.is_file(node) {
        println!("files: {path}: is a directory");
        return;
    }

    let mut buffer = [0u8; 8192];
    let read = fs.read(node, 0, &mut buffer);
    match core::str::from_utf8(&buffer[..read]) {
        Ok(text) => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
        }
        Err(_) => println!("files: {path}: binary file ({} bytes)", fs.size(node)),
    }
    if read < fs.size(node) {
        println!("files: {path}: output truncated to {read} bytes");
    }
}

fn tree(fs: &dyn FileSystem, path: &str) {
    let Some(node) = fs.resolve(path) else {
        println!("files: {path}: not found");
        return;
    };
    let mut buffer = [0u8; PATH_MAX];
    println!("{}", fs.path(node, &mut buffer).unwrap_or(path));
    tree_children(fs, node, 1);
}

fn tree_children(fs: &dyn FileSystem, node: NodeId, depth: usize) {
    if depth > 8 {
        return;
    }
    for index in 0..fs.child_count(node) {
        let Some(child) = fs.child_at(node, index) else {
            continue;
        };
        for _ in 0..depth {
            print!("  ");
        }
        if fs.is_dir(child) {
            println!("{}/", fs.name(child).unwrap_or("?"));
            tree_children(fs, child, depth + 1);
        } else {
            println!(
                "{}  ({})",
                fs.name(child).unwrap_or("?"),
                format_size(fs.size(child))
            );
        }
    }
}

fn stat(fs: &dyn FileSystem, path: &str) {
    if path.is_empty() {
        println!("files: usage: files stat <path>");
        return;
    }
    let Some(node) = fs.resolve(path) else {
        println!("files: {path}: not found");
        return;
    };
    let mut buffer = [0u8; PATH_MAX];
    println!("Path:     {}", fs.path(node, &mut buffer).unwrap_or(path));
    println!(
        "Kind:     {}",
        fs.kind(node).map(EntryKind::name).unwrap_or("?")
    );
    println!("Size:     {} bytes", fs.size(node));
    if fs.is_dir(node) {
        println!("Children: {}", fs.child_count(node));
    }
}

fn format_size(bytes: usize) -> Text<16> {
    let mut text = Text::<16>::new();
    if bytes < 1024 {
        let _ = write!(text, "{bytes} B");
    } else if bytes < 1024 * 1024 {
        let _ = write!(text, "{} KB", (bytes + 512) / 1024);
    } else {
        let _ = write!(text, "{} MB", (bytes + 512 * 1024) / (1024 * 1024));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_formatted_for_the_list_columns() {
        assert_eq!(format_size(0).as_str(), "0 B");
        assert_eq!(format_size(1023).as_str(), "1023 B");
        assert_eq!(format_size(1024).as_str(), "1 KB");
        assert_eq!(format_size(1536).as_str(), "2 KB");
        assert_eq!(format_size(2 * 1024 * 1024).as_str(), "2 MB");
    }
}

struct ExplorerState {
    current: NodeId,
    selected: usize,
    scroll: usize,
    preview: [u8; PREVIEW_MAX],
    preview_len: usize,
    preview_node: Option<NodeId>,
    path: [u8; PATH_MAX],
    path_len: usize,
}

impl ExplorerState {
    fn new(fs: &dyn FileSystem) -> Self {
        let mut state = Self {
            current: fs.root(),
            selected: 0,
            scroll: 0,
            preview: [0; PREVIEW_MAX],
            preview_len: 0,
            preview_node: None,
            path: [0; PATH_MAX],
            path_len: 0,
        };
        state.refresh_path(fs);
        state
    }

    fn refresh_path(&mut self, fs: &dyn FileSystem) {
        let path = fs.path(self.current, &mut self.path).unwrap_or("/");
        self.path_len = path.len();
    }

    fn path(&self) -> &str {
        core::str::from_utf8(&self.path[..self.path_len]).unwrap_or("/")
    }

    fn selected_node(&self, fs: &dyn FileSystem) -> Option<NodeId> {
        fs.child_at(self.current, self.selected)
    }

    fn reset_for_directory(&mut self, fs: &dyn FileSystem, directory: NodeId, selected: usize) {
        self.current = directory;
        self.selected = selected;
        self.scroll = 0;
        self.preview_node = None;
        self.refresh_path(fs);
    }
}

fn run_explorer(fs: &dyn FileSystem, context: &mut Context<'_>) {
    let platform = context.platform;
    let Some(canvas) = context.display.as_mut() else {
        println!("files: no framebuffer; try 'files ls /' instead");
        return;
    };

    let mut state = ExplorerState::new(fs);
    let mut input = Input::new();
    let mut keys = SerialKeys::new();
    render(canvas, fs, &mut state);

    loop {
        let now = platform.clock.uptime_ms();
        if keys.poll(now, &mut input) {
            break;
        }

        if !input.is_empty() {
            let mut quit = false;
            for (key, _) in input.keys() {
                if handle_key(fs, &mut state, key) {
                    quit = true;
                }
            }
            render(canvas, fs, &mut state);
            input.clear_events();
            if quit {
                break;
            }
        }

        platform.clock.spin_ms(16);
    }

    println!("[files] explorer closed");
}

fn handle_key(fs: &dyn FileSystem, state: &mut ExplorerState, key: feos_gui::Key) -> bool {
    let count = fs.child_count(state.current);
    match key {
        Key::Up => state.selected = state.selected.saturating_sub(1),
        Key::Down => {
            if state.selected + 1 < count {
                state.selected += 1;
            }
        }
        Key::Home => state.selected = 0,
        Key::End => state.selected = count.saturating_sub(1),
        Key::PageUp => state.selected = state.selected.saturating_sub(8),
        Key::PageDown => {
            state.selected = (state.selected + 8).min(count.saturating_sub(1));
        }
        Key::Enter | Key::Right => {
            if let Some(node) = state.selected_node(fs) {
                if fs.is_dir(node) {
                    state.reset_for_directory(fs, node, 0);
                }
            }
        }
        Key::Left | Key::Backspace => {
            if state.current != fs.root() {
                if let Some(parent) = fs.parent(state.current) {
                    let previous = state.current;
                    let selected = (0..fs.child_count(parent))
                        .position(|index| fs.child_at(parent, index) == Some(previous))
                        .unwrap_or(0);
                    state.reset_for_directory(fs, parent, selected);
                }
            }
        }
        Key::Escape | Key::Character('q') => return true,
        _ => {}
    }
    false
}

fn render(canvas: &mut Canvas<'_>, fs: &dyn FileSystem, state: &mut ExplorerState) {
    let theme = Theme::dark();
    let palette = theme.palette;
    let mut painter = Painter::new(canvas);
    let full = painter.canvas_rect();
    painter.fill_rect(full, palette.background);

    // Header ------------------------------------------------------------
    painter.fill_rect(Rect::new(0, 0, full.width, 66), palette.surface);
    painter.fill_rect(Rect::new(0, 65, full.width, 1), palette.border);
    paint_icon(
        &mut painter,
        Rect::new(18, 18, 28, 28),
        GuiIcon::Folder,
        palette.accent,
    );
    painter.draw_text(56, 16, "File Explorer", palette.text, 2);
    painter.draw_text_ellipsis(
        Rect::new(56, 40, full.width.saturating_sub(76), 18),
        state.path(),
        palette.text_muted,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );

    // Panes -------------------------------------------------------------
    let footer_height = 32u32;
    let top = 82i32;
    let list_width = (full.width as i32 * 58 / 100).max(320) as u32;
    let list = Rect::new(
        16,
        top,
        list_width.saturating_sub(24),
        (full.height as i32 - top - footer_height as i32 - 12).max(1) as u32,
    );
    let preview = Rect::new(
        list.right_i32() + 12,
        top,
        (full.right_i32() - list.right_i32() - 28).max(1) as u32,
        list.height,
    );

    draw_list(&mut painter, fs, state, list, &palette);
    draw_preview(&mut painter, fs, state, preview, &palette);

    // Footer ------------------------------------------------------------
    let footer = Rect::new(
        0,
        full.height as i32 - footer_height as i32,
        full.width,
        footer_height,
    );
    painter.fill_rect(footer, palette.surface);
    painter.fill_rect(Rect::new(0, footer.y, full.width, 1), palette.border);

    let mut status = Text::<128>::new();
    let count = fs.child_count(state.current);
    let _ = write!(status, "{count} item(s)");
    if let Some(node) = state.selected_node(fs) {
        let _ = write!(
            status,
            "   selected: {} ({})",
            fs.name(node).unwrap_or("?"),
            fs.kind(node).map(EntryKind::name).unwrap_or("?")
        );
    }
    painter.draw_text_ellipsis(
        Rect::new(16, footer.y, full.width / 2, footer_height),
        status.as_str(),
        palette.text_muted,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    painter.draw_text_ellipsis(
        Rect::new(
            full.width as i32 / 2,
            footer.y,
            full.width / 2 - 16,
            footer_height,
        ),
        "Up/Down move   Enter open   Backspace up   Esc quit",
        palette.text_muted,
        1,
        Align::End,
        VerticalAlign::Middle,
    );
}

fn draw_list(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    rect: Rect,
    palette: &feos_gui::Palette,
) {
    painter.fill_rounded_rect(rect, 8, palette.surface);
    painter.stroke_rounded_rect(rect, 8, 1, palette.border);

    let inner = Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2);
    let visible = (inner.height as i32 / ROW_HEIGHT).max(1) as usize;
    let count = fs.child_count(state.current);

    if state.selected < state.scroll {
        state.scroll = state.selected;
    } else if state.selected >= state.scroll + visible {
        state.scroll = state.selected + 1 - visible;
    }
    if count == 0 {
        state.selected = 0;
        state.scroll = 0;
    } else if state.selected >= count {
        state.selected = count - 1;
    }

    let clip = painter.push_clip(inner);
    for index in state.scroll..(state.scroll + visible).min(count) {
        let row = Rect::new(
            inner.x,
            inner.y + (index - state.scroll) as i32 * ROW_HEIGHT,
            inner.width,
            ROW_HEIGHT as u32,
        );
        let selected = index == state.selected;
        if selected {
            painter.fill_rect(row, palette.selection);
        }

        let Some(node) = fs.child_at(state.current, index) else {
            continue;
        };
        let icon = if fs.is_dir(node) {
            GuiIcon::Folder
        } else {
            GuiIcon::File
        };
        paint_icon(
            painter,
            Rect::new(row.x + 8, row.y + 7, 16, 16),
            icon,
            if selected {
                palette.text
            } else {
                palette.text_muted
            },
        );
        painter.draw_text_ellipsis(
            Rect::new(
                row.x + 32,
                row.y,
                row.width.saturating_sub(116),
                ROW_HEIGHT as u32,
            ),
            fs.name(node).unwrap_or("?"),
            palette.text,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        let detail = if fs.is_dir(node) {
            let mut text = Text::<16>::new();
            let _ = write!(text, "{} item(s)", fs.child_count(node));
            text
        } else {
            format_size(fs.size(node))
        };
        painter.draw_text_ellipsis(
            Rect::new(row.right_i32() - 88, row.y, 80, ROW_HEIGHT as u32),
            detail.as_str(),
            palette.text_muted,
            1,
            Align::End,
            VerticalAlign::Middle,
        );
    }
    painter.restore_clip(clip);

    if count > visible {
        let track = Rect::new(
            rect.right_i32() - 7,
            rect.y + 6,
            3,
            rect.height.saturating_sub(12),
        );
        painter.fill_rounded_rect(track, 2, palette.scrollbar_track);
        let height = (track.height as f32 * visible as f32 / count as f32).max(18.0) as u32;
        let max_offset = track.height.saturating_sub(height) as f32;
        let progress = if count > visible {
            state.scroll as f32 / (count - visible) as f32
        } else {
            0.0
        };
        painter.fill_rounded_rect(
            Rect::new(
                track.x,
                track.y + (progress * max_offset) as i32,
                track.width,
                height,
            ),
            2,
            palette.scrollbar_thumb,
        );
    }
}

fn draw_preview(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    rect: Rect,
    palette: &feos_gui::Palette,
) {
    painter.fill_rounded_rect(rect, 8, palette.surface);
    painter.stroke_rounded_rect(rect, 8, 1, palette.border);

    let inner = Rect::new(
        rect.x + 12,
        rect.y + 12,
        rect.width.saturating_sub(24),
        rect.height.saturating_sub(24),
    );
    let Some(node) = state.selected_node(fs) else {
        painter.draw_text(
            inner.x,
            inner.y,
            "This directory is empty.",
            palette.text_muted,
            1,
        );
        return;
    };

    painter.draw_text_ellipsis(
        Rect::new(inner.x, inner.y, inner.width, 22),
        fs.name(node).unwrap_or("?"),
        palette.text,
        2,
        Align::Start,
        VerticalAlign::Middle,
    );

    let mut meta = Text::<96>::new();
    let _ = write!(
        meta,
        "{}   {} bytes",
        fs.kind(node).map(EntryKind::name).unwrap_or("?"),
        fs.size(node)
    );
    painter.draw_text(inner.x, inner.y + 28, meta.as_str(), palette.text_muted, 1);
    painter.fill_rect(
        Rect::new(inner.x, inner.y + 46, inner.width, 1),
        palette.border,
    );

    let body = Rect::new(
        inner.x,
        inner.y + 58,
        inner.width,
        inner.height.saturating_sub(58),
    );
    let clip = painter.push_clip(body);

    if fs.is_dir(node) {
        let count = fs.child_count(node);
        let mut line = Text::<96>::new();
        let _ = write!(line, "Directory with {count} item(s)");
        painter.draw_text(body.x, body.y, line.as_str(), palette.text, 1);

        let mut y = body.y + 24;
        for index in 0..count.min(32) {
            let Some(child) = fs.child_at(node, index) else {
                continue;
            };
            if y + 18 > body.bottom_i32() {
                break;
            }
            painter.draw_text_ellipsis(
                Rect::new(body.x, y, 20, 18),
                if fs.is_dir(child) { "d" } else { "-" },
                palette.text_muted,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
            painter.draw_text_ellipsis(
                Rect::new(body.x + 20, y, body.width.saturating_sub(20), 18),
                fs.name(child).unwrap_or("?"),
                palette.text,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
            y += 18;
        }
    } else {
        if state.preview_node != Some(node) {
            state.preview_len = fs.read(node, 0, &mut state.preview);
            state.preview_node = Some(node);
        }
        if state.preview_len == 0 {
            painter.draw_text(body.x, body.y, "(empty file)", palette.text_muted, 1);
        } else {
            match core::str::from_utf8(&state.preview[..state.preview_len]) {
                Ok(text) => {
                    draw_wrapped_text(painter, body, text, palette.text);
                }
                Err(_) => {
                    painter.draw_text(body.x, body.y, "Binary file", palette.text_muted, 1);
                }
            }
            if fs.size(node) > state.preview_len {
                painter.draw_text(
                    body.x,
                    body.bottom_i32() - 18,
                    "... preview truncated ...",
                    palette.warning,
                    1,
                );
            }
        }
    }

    painter.restore_clip(clip);
}

/// Draws word-wrapped text with a little more leading than the toolkit's
/// default 8-pixel line height, which is too tight for a preview pane.
fn draw_wrapped_text(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    text: &str,
    color: feos_gfx::Color,
) {
    const LINE_HEIGHT: i32 = 11;
    let space = painter.text_width(" ", 1).max(2);
    let mut y = rect.y;

    for paragraph in text.split('\n') {
        let mut x = rect.x;
        let mut started = false;
        for word in paragraph.split_whitespace() {
            let width = painter.text_width(word, 1);
            if started && x + width > rect.right_i32() {
                y += LINE_HEIGHT;
                x = rect.x;
            }
            if y + LINE_HEIGHT > rect.bottom_i32() {
                return;
            }
            painter.draw_text(x, y, word, color, 1);
            x += width + space;
            started = true;
        }
        y += LINE_HEIGHT;
    }
}
