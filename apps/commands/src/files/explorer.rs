//! The Windows XP-style File Explorer.
//!
//! The explorer is deliberately self-contained: it owns its menus, toolbar,
//! folder tree, address bar, search pane, four view modes, preview pane,
//! dialogs, pointer handling and keyboard shortcuts. It is still
//! allocation-free and works without a mouse, because the serial console is
//! the only keyboard on the QEMU board.

use core::cmp::Ordering;
use core::fmt::Write;

use alloc::vec;

use feos_app::Context;
use feos_fs::{FileSystem, NodeId};
use feos_gfx::{Canvas, Color, Point, Rect};
use feos_gui::{
    paint_icon, Align, Event, Icon, Input, Key, Modifiers, Painter, RectExt, TextBuffer,
    VerticalAlign,
};
use feos_hal::PointerEvent;
use feos_keys::SerialKeys;
use feos_text::Text;

use super::{format_size, PATH_MAX};

const MAX_ENTRIES: usize = 256;
const MAX_SEARCH: usize = 128;
const MAX_TREE: usize = 256;
const MAX_HISTORY: usize = 32;
const SELECTION_WORDS: usize = 16;
const PREVIEW_MAX: usize = 4096;
const VIEWER_MAX: usize = 8192;

const ROW_HEIGHT: i32 = 20;
const HEADER_HEIGHT: i32 = 21;
const TREE_ROW: i32 = 18;
const SIDEBAR_WIDTH: i32 = 214;
const PREVIEW_WIDTH: i32 = 268;
const MENU_BAR_HEIGHT: i32 = 22;
const TOOLBAR_HEIGHT: i32 = 46;
const ADDRESS_HEIGHT: i32 = 30;
const TITLE_HEIGHT: i32 = 28;
const STATUS_HEIGHT: i32 = 22;

const FACE: Color = Color::rgb(236, 233, 216);
const FACE_LIGHT: Color = Color::rgb(244, 241, 232);
const FACE_DARK: Color = Color::rgb(214, 208, 190);
const BORDER: Color = Color::rgb(172, 168, 153);
const BORDER_DARK: Color = Color::rgb(128, 124, 111);
const TITLE_TOP: Color = Color::rgb(0, 88, 238);
const TITLE_BOTTOM: Color = Color::rgb(58, 145, 255);
const SELECT: Color = Color::rgb(49, 106, 197);
const SELECT_SOFT: Color = Color::rgb(214, 228, 250);
const LINK: Color = Color::rgb(0, 0, 160);
const FIELD: Color = Color::WHITE;
const FIELD_BORDER: Color = Color::rgb(127, 157, 185);
const PANEL_BLUE: Color = Color::rgb(214, 223, 247);
const HEADER_TOP: Color = Color::rgb(123, 162, 231);
const HEADER_BOTTOM: Color = Color::rgb(99, 117, 214);
const TEXT: Color = Color::rgb(24, 24, 24);
const MUTED: Color = Color::rgb(92, 92, 92);
const DISABLED: Color = Color::rgb(160, 160, 160);
const TOOLBAR_TOP: Color = Color::rgb(250, 249, 245);
const TOOLBAR_BOTTOM: Color = Color::rgb(236, 233, 216);
const HOVER_LIGHT: Color = Color::rgb(232, 240, 254);
const FOLDER_FRONT: Color = Color::rgb(255, 207, 83);
const FOLDER_BACK: Color = Color::rgb(226, 166, 36);
const FOLDER_EDGE: Color = Color::rgb(172, 120, 22);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Details,
    Icons,
    List,
    Tiles,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExplorerBar {
    None,
    Folders,
    Search,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Tree,
    Address,
    Search,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Size,
    Type,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialog {
    None,
    About,
    Options,
    Properties(NodeId),
    Viewer(NodeId),
    Notice,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuKind {
    File,
    Edit,
    View,
    Favorites,
    Tools,
    Help,
}

impl MenuKind {
    const ALL: [Self; 6] = [
        Self::File,
        Self::Edit,
        Self::View,
        Self::Favorites,
        Self::Tools,
        Self::Help,
    ];

    const fn index(self) -> usize {
        match self {
            Self::File => 0,
            Self::Edit => 1,
            Self::View => 2,
            Self::Favorites => 3,
            Self::Tools => 4,
            Self::Help => 5,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    Open,
    Explore,
    Search,
    Properties,
    Close,
    SelectAll,
    InvertSelection,
    Back,
    Forward,
    Up,
    Refresh,
    ViewDetails,
    ViewIcons,
    ViewList,
    ViewTiles,
    ToggleFolders,
    ToggleSearch,
    HideExplorerBar,
    TogglePreview,
    ToggleStatus,
    ToggleToolbar,
    ToggleAddress,
    SortName,
    SortSize,
    SortType,
    SortAscending,
    SortDescending,
    CopyPath,
    FolderOptions,
    About,
    Delete,
    NewFolder,
    Cut,
    Copy,
    Paste,
    MapDrive,
    Help,
    Home,
    MyComputer,
    MyDocuments,
    Desktop,
}

#[derive(Clone, Copy)]
struct MenuItem {
    label: &'static str,
    shortcut: &'static str,
    action: Action,
    separator: bool,
    enabled: bool,
}

const fn item(label: &'static str, shortcut: &'static str, action: Action) -> MenuItem {
    MenuItem {
        label,
        shortcut,
        action,
        separator: false,
        enabled: true,
    }
}

const fn disabled(label: &'static str, shortcut: &'static str) -> MenuItem {
    MenuItem {
        label,
        shortcut,
        action: Action::None,
        separator: false,
        enabled: false,
    }
}

const fn sep() -> MenuItem {
    MenuItem {
        label: "",
        shortcut: "",
        action: Action::None,
        separator: true,
        enabled: false,
    }
}

const FILE_MENU: &[MenuItem] = &[
    item("Open", "Enter", Action::Open),
    item("Explore", "", Action::Explore),
    item("Search...", "F3", Action::Search),
    sep(),
    disabled("Sharing and Security...", ""),
    sep(),
    item("Properties", "Alt+Enter", Action::Properties),
    sep(),
    item("Close", "Alt+F4", Action::Close),
];

const EDIT_MENU: &[MenuItem] = &[
    disabled("Undo", "Ctrl+Z"),
    disabled("Cut", "Ctrl+X"),
    disabled("Copy", "Ctrl+C"),
    disabled("Paste", "Ctrl+V"),
    sep(),
    item("Select All", "Ctrl+A", Action::SelectAll),
    item("Invert Selection", "", Action::InvertSelection),
    sep(),
    item("Copy Path", "", Action::CopyPath),
];

const VIEW_MENU: &[MenuItem] = &[
    item("Standard Buttons", "", Action::ToggleToolbar),
    item("Address Bar", "", Action::ToggleAddress),
    item("Status Bar", "", Action::ToggleStatus),
    sep(),
    item("Folders", "", Action::ToggleFolders),
    item("Search", "", Action::ToggleSearch),
    item("Preview Pane", "", Action::TogglePreview),
    item("Hide Explorer Bar", "", Action::HideExplorerBar),
    sep(),
    disabled("Thumbnails (unavailable)", ""),
    item("Tiles", "", Action::ViewTiles),
    item("Icons", "", Action::ViewIcons),
    item("List", "", Action::ViewList),
    item("Details", "", Action::ViewDetails),
    sep(),
    item("Sort by Name", "", Action::SortName),
    item("Sort by Size", "", Action::SortSize),
    item("Sort by Type", "", Action::SortType),
    item("Ascending", "", Action::SortAscending),
    item("Descending", "", Action::SortDescending),
    sep(),
    item("Refresh", "F5", Action::Refresh),
];

const FAVORITES_MENU: &[MenuItem] = &[
    disabled("Add to Favorites...", ""),
    disabled("Organize Favorites...", ""),
    sep(),
    item("My Computer", "", Action::MyComputer),
    item("My Documents", "", Action::MyDocuments),
    item("Desktop", "", Action::Desktop),
];

const TOOLS_MENU: &[MenuItem] = &[
    disabled("Map Network Drive...", ""),
    disabled("Disconnect Network Drive...", ""),
    sep(),
    item("Folder Options...", "", Action::FolderOptions),
];

const HELP_MENU: &[MenuItem] = &[
    disabled("Help and Support", "F1"),
    sep(),
    item("About File Explorer", "", Action::About),
];

const CONTEXT_ITEM: &[MenuItem] = &[
    item("Open", "Enter", Action::Open),
    item("Explore", "", Action::Explore),
    item("Search...", "F3", Action::Search),
    sep(),
    item("Copy Path", "", Action::CopyPath),
    item("Properties", "Alt+Enter", Action::Properties),
    sep(),
    disabled("Delete", "Del"),
    disabled("Rename", "F2"),
    sep(),
    item("Refresh", "F5", Action::Refresh),
];

const CONTEXT_BACKGROUND: &[MenuItem] = &[
    item("View", "", Action::ViewDetails),
    item("Refresh", "F5", Action::Refresh),
    sep(),
    disabled("Paste", "Ctrl+V"),
    disabled("New Folder", ""),
    sep(),
    item("Properties", "Alt+Enter", Action::Properties),
];

const MENU_LABELS: [&str; 6] = ["File", "Edit", "View", "Favorites", "Tools", "Help"];
const MENU_WIDTHS: [i32; 6] = [52, 52, 52, 84, 52, 52];
const TOOLBAR_WIDTHS: [i32; 6] = [54, 66, 42, 58, 60, 54];

fn menu_items(kind: MenuKind) -> &'static [MenuItem] {
    match kind {
        MenuKind::File => FILE_MENU,
        MenuKind::Edit => EDIT_MENU,
        MenuKind::View => VIEW_MENU,
        MenuKind::Favorites => FAVORITES_MENU,
        MenuKind::Tools => TOOLS_MENU,
        MenuKind::Help => HELP_MENU,
    }
}

/// The edge or corner used to resize the explorer window.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ResizeEdge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ResizeEdge {
    const fn moves_left(self) -> bool {
        matches!(self, Self::Left | Self::TopLeft | Self::BottomLeft)
    }

    const fn moves_right(self) -> bool {
        matches!(self, Self::Right | Self::TopRight | Self::BottomRight)
    }

    const fn moves_top(self) -> bool {
        matches!(self, Self::Top | Self::TopLeft | Self::TopRight)
    }

    const fn moves_bottom(self) -> bool {
        matches!(self, Self::Bottom | Self::BottomLeft | Self::BottomRight)
    }
}

#[derive(Clone, Copy)]
enum DragMode {
    None,
    Move {
        dx: i32,
        dy: i32,
    },
    Resize {
        edge: ResizeEdge,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        pointer_x: i32,
        pointer_y: i32,
    },
    Marquee {
        start: Point,
    },
}

#[derive(Clone, Copy)]
struct ContextMenu {
    target: Option<NodeId>,
    background: bool,
    x: i32,
    y: i32,
    hover: usize,
}

struct ExplorerState {
    screen: Rect,
    current: NodeId,
    entries: [NodeId; MAX_ENTRIES],
    entry_count: usize,
    cursor: usize,
    anchor: usize,
    selected: [u64; SELECTION_WORDS],
    scroll: usize,
    view: ViewMode,
    sort: SortKey,
    sort_desc: bool,
    sort_pending: bool,
    bar: ExplorerBar,
    preview_open: bool,
    status_open: bool,
    toolbar_open: bool,
    address_open: bool,
    focus: Focus,
    address: TextBuffer<PATH_MAX>,
    address_edit: bool,
    address_dropdown: bool,
    search: TextBuffer<64>,
    search_dirty: bool,
    include_subfolders: bool,
    search_results: [NodeId; MAX_SEARCH],
    search_count: usize,
    history: [NodeId; MAX_HISTORY],
    history_len: usize,
    history_pos: usize,
    expanded: [u64; SELECTION_WORDS],
    tree: [NodeId; MAX_TREE],
    tree_depth: [u8; MAX_TREE],
    tree_len: usize,
    tree_cursor: usize,
    tree_scroll: usize,
    preview: [u8; PREVIEW_MAX],
    preview_len: usize,
    preview_node: Option<NodeId>,
    preview_scroll: usize,
    viewer: [u8; VIEWER_MAX],
    viewer_len: usize,
    viewer_scroll: usize,
    viewer_lines: usize,
    dialog: Dialog,
    menu_open: Option<MenuKind>,
    menu_hover: usize,
    context: Option<ContextMenu>,
    notice_title: Text<48>,
    notice_body: Text<192>,
    message: Text<160>,
    message_until: u64,
    pointer: Point,
    has_pointer: bool,
    mouse_down: bool,
    drag: DragMode,
    last_click_ms: u64,
    last_click_node: Option<NodeId>,
    maximized: bool,
    window_x: i32,
    window_y: i32,
    window_w: i32,
    window_h: i32,
    exit: bool,
}

impl ExplorerState {
    fn new(fs: &dyn FileSystem, screen: Rect) -> Self {
        let mut state = Self {
            screen,
            current: fs.root(),
            entries: [NodeId::ROOT; MAX_ENTRIES],
            entry_count: 0,
            cursor: 0,
            anchor: 0,
            selected: [0; SELECTION_WORDS],
            scroll: 0,
            view: ViewMode::Details,
            sort: SortKey::Name,
            sort_desc: false,
            sort_pending: false,
            bar: ExplorerBar::Folders,
            preview_open: true,
            status_open: true,
            toolbar_open: true,
            address_open: true,
            focus: Focus::List,
            address: TextBuffer::new(),
            address_edit: false,
            address_dropdown: false,
            search: TextBuffer::new(),
            search_dirty: false,
            include_subfolders: true,
            search_results: [NodeId::ROOT; MAX_SEARCH],
            search_count: 0,
            history: [NodeId::ROOT; MAX_HISTORY],
            history_len: 0,
            history_pos: 0,
            expanded: [0; SELECTION_WORDS],
            tree: [NodeId::ROOT; MAX_TREE],
            tree_depth: [0; MAX_TREE],
            tree_len: 0,
            tree_cursor: 0,
            tree_scroll: 0,
            preview: [0; PREVIEW_MAX],
            preview_len: 0,
            preview_node: None,
            preview_scroll: 0,
            viewer: [0; VIEWER_MAX],
            viewer_len: 0,
            viewer_scroll: 0,
            viewer_lines: 0,
            dialog: Dialog::None,
            menu_open: None,
            menu_hover: 0,
            context: None,
            notice_title: Text::new(),
            notice_body: Text::new(),
            message: Text::new(),
            message_until: 0,
            pointer: Point::new(-100, -100),
            has_pointer: false,
            mouse_down: false,
            drag: DragMode::None,
            last_click_ms: 0,
            last_click_node: None,
            maximized: true,
            window_x: 38,
            window_y: 30,
            window_w: screen.width as i32 - 76,
            window_h: screen.height as i32 - 60,
            exit: false,
        };
        set_bit(&mut state.expanded, fs.root().0 as usize);
        state.history[0] = fs.root();
        state.history_len = 1;
        state.history_pos = 0;
        state.refresh_address(fs);
        state.rebuild(fs);
        state.rebuild_tree(fs);
        state.sync_preview(fs);
        state
    }

    fn set_screen(&mut self, screen: Rect) {
        self.screen = screen;
        if !self.maximized {
            self.window_w = self.window_w.min(screen.width as i32 - 40).max(720);
            self.window_h = self.window_h.min(screen.height as i32 - 40).max(480);
            self.window_x = self
                .window_x
                .clamp(0, (screen.width as i32 - self.window_w).max(0));
            self.window_y = self
                .window_y
                .clamp(0, (screen.height as i32 - self.window_h).max(0));
        }
    }

    fn refresh_address(&mut self, fs: &dyn FileSystem) {
        let mut buffer = [0u8; PATH_MAX];
        let path = fs.path(self.current, &mut buffer).unwrap_or("/");
        self.address.set(path);
    }

    fn searching(&self) -> bool {
        self.bar == ExplorerBar::Search && !self.search.is_empty()
    }

    fn focused_node(&self) -> Option<NodeId> {
        if self.cursor < self.entry_count {
            self.entries.get(self.cursor).copied()
        } else {
            None
        }
    }

    fn target_node(&self) -> NodeId {
        self.focused_node().unwrap_or(self.current)
    }

    fn is_selected(&self, node: NodeId) -> bool {
        bit(&self.selected, node.0 as usize)
    }

    fn select_node(&mut self, node: NodeId) {
        set_bit(&mut self.selected, node.0 as usize);
    }

    fn deselect_node(&mut self, node: NodeId) {
        clear_bit(&mut self.selected, node.0 as usize);
    }

    fn clear_selection(&mut self) {
        self.selected = [0; SELECTION_WORDS];
    }

    fn select_all(&mut self) {
        for index in 0..self.entry_count {
            self.select_node(self.entries[index]);
        }
    }

    fn invert_selection(&mut self) {
        for index in 0..self.entry_count {
            let node = self.entries[index];
            if self.is_selected(node) {
                self.deselect_node(node);
            } else {
                self.select_node(node);
            }
        }
    }

    fn select_only(&mut self, index: usize) {
        self.clear_selection();
        if let Some(node) = self.entries.get(index).copied() {
            self.cursor = index;
            self.anchor = index;
            self.select_node(node);
        }
    }

    fn toggle_at(&mut self, index: usize) {
        if let Some(node) = self.entries.get(index).copied() {
            self.cursor = index;
            self.anchor = index;
            if self.is_selected(node) {
                self.deselect_node(node);
            } else {
                self.select_node(node);
            }
        }
    }

    fn select_range(&mut self, from: usize, to: usize) {
        self.clear_selection();
        let (start, end) = if from <= to { (from, to) } else { (to, from) };
        for index in start..=end.min(self.entry_count.saturating_sub(1)) {
            self.select_node(self.entries[index]);
        }
    }

    fn move_cursor(&mut self, fs: &dyn FileSystem, index: usize, shift: bool, ctrl: bool) {
        if self.entry_count == 0 {
            return;
        }
        let index = index.min(self.entry_count - 1);
        if shift {
            self.select_range(self.anchor, index);
        } else if ctrl {
            // Ctrl+arrow moves the focus without changing the selection.
        } else {
            self.select_only(index);
            self.anchor = index;
        }
        self.cursor = index;
        self.sync_preview(fs);
    }

    fn selected_count(&self) -> usize {
        (0..self.entry_count)
            .filter(|index| self.is_selected(self.entries[*index]))
            .count()
    }

    fn selected_bytes(&self, fs: &dyn FileSystem) -> usize {
        (0..self.entry_count)
            .filter(|index| self.is_selected(self.entries[*index]))
            .filter(|index| fs.is_file(self.entries[*index]))
            .map(|index| fs.size(self.entries[index]))
            .sum()
    }

    fn navigate(&mut self, fs: &dyn FileSystem, node: NodeId, record: bool) {
        if !fs.is_dir(node) {
            return;
        }
        if record {
            self.record_history(node);
        }
        if node == self.current && record {
            // Visiting the same folder still refreshes it, like Explorer.
            self.refresh(fs);
            return;
        }
        if self.bar == ExplorerBar::Search {
            self.bar = ExplorerBar::Folders;
            self.search.clear();
            self.search_count = 0;
        }
        self.current = node;
        self.cursor = 0;
        self.anchor = 0;
        self.scroll = 0;
        self.clear_selection();
        self.preview_node = None;
        self.preview_scroll = 0;
        self.expand_ancestors(fs, node);
        self.refresh_address(fs);
        self.rebuild(fs);
        self.rebuild_tree(fs);
        self.sync_preview(fs);
    }

    fn record_history(&mut self, node: NodeId) {
        if self.history_pos < self.history_len && self.history.get(self.history_pos) == Some(&node)
        {
            return;
        }
        if self.history_len > 0 && self.history[self.history_pos] == node {
            return;
        }
        if self.history_pos + 1 < self.history_len {
            self.history_len = self.history_pos + 1;
        }
        if self.history_len < MAX_HISTORY {
            self.history_pos = self.history_len;
            self.history[self.history_len] = node;
            self.history_len += 1;
        } else {
            for index in 1..MAX_HISTORY {
                self.history[index - 1] = self.history[index];
            }
            self.history[MAX_HISTORY - 1] = node;
            self.history_pos = MAX_HISTORY - 1;
        }
    }

    fn history_back(&mut self, fs: &dyn FileSystem) {
        if self.history_pos == 0 {
            return;
        }
        self.history_pos -= 1;
        self.jump_history(fs);
    }

    fn history_forward(&mut self, fs: &dyn FileSystem) {
        if self.history_pos + 1 >= self.history_len {
            return;
        }
        self.history_pos += 1;
        self.jump_history(fs);
    }

    fn jump_history(&mut self, fs: &dyn FileSystem) {
        let node = self.history[self.history_pos];
        self.current = node;
        self.cursor = 0;
        self.anchor = 0;
        self.scroll = 0;
        self.clear_selection();
        self.preview_node = None;
        self.expand_ancestors(fs, node);
        self.refresh_address(fs);
        self.rebuild(fs);
        self.rebuild_tree(fs);
        self.sync_preview(fs);
    }

    fn go_up(&mut self, fs: &dyn FileSystem) {
        if let Some(parent) = fs.parent(self.current) {
            let previous = self.current;
            self.navigate(fs, parent, true);
            if let Some(index) = self.entry_position(previous) {
                self.select_only(index);
            }
        }
    }

    fn entry_position(&self, node: NodeId) -> Option<usize> {
        (0..self.entry_count).find(|index| self.entries[*index] == node)
    }

    fn refresh(&mut self, fs: &dyn FileSystem) {
        let old = self.focused_node();
        self.rebuild(fs);
        if let Some(node) = old {
            if let Some(index) = self.entry_position(node) {
                self.cursor = index;
            }
        }
        self.rebuild_tree(fs);
        self.sync_preview(fs);
        self.clamp_scroll();
    }

    fn rebuild(&mut self, fs: &dyn FileSystem) {
        let old = self.focused_node();
        self.entry_count = 0;
        if self.searching() {
            for index in 0..self.search_count {
                if self.entry_count == MAX_ENTRIES {
                    break;
                }
                self.entries[self.entry_count] = self.search_results[index];
                self.entry_count += 1;
            }
        } else {
            let count = fs.child_count(self.current);
            for index in 0..count {
                if self.entry_count == MAX_ENTRIES {
                    break;
                }
                if let Some(child) = fs.child_at(self.current, index) {
                    self.entries[self.entry_count] = child;
                    self.entry_count += 1;
                }
            }
            self.sort_entries(fs);
        }

        self.cursor = old.and_then(|node| self.entry_position(node)).unwrap_or(0);
        if self.entry_count == 0 {
            self.cursor = 0;
            self.anchor = 0;
            self.scroll = 0;
        } else if self.cursor >= self.entry_count {
            self.cursor = self.entry_count - 1;
        }
        self.clamp_scroll();
    }

    fn sort_entries(&mut self, fs: &dyn FileSystem) {
        for index in 1..self.entry_count {
            let value = self.entries[index];
            let mut position = index;
            while position > 0 {
                let previous = self.entries[position - 1];
                if compare_nodes(fs, value, previous, self.sort, self.sort_desc) != Ordering::Less {
                    break;
                }
                self.entries[position] = previous;
                position -= 1;
            }
            self.entries[position] = value;
        }
    }

    fn expand_ancestors(&mut self, fs: &dyn FileSystem, node: NodeId) {
        set_bit(&mut self.expanded, fs.root().0 as usize);
        let mut current = fs.parent(node);
        while let Some(parent) = current {
            set_bit(&mut self.expanded, parent.0 as usize);
            current = fs.parent(parent);
        }
    }

    fn toggle_expanded(&mut self, fs: &dyn FileSystem, node: NodeId) {
        if bit(&self.expanded, node.0 as usize) {
            clear_bit(&mut self.expanded, node.0 as usize);
        } else {
            set_bit(&mut self.expanded, node.0 as usize);
        }
        self.rebuild_tree(fs);
    }

    fn rebuild_tree(&mut self, fs: &dyn FileSystem) {
        self.tree_len = 0;
        set_bit(&mut self.expanded, fs.root().0 as usize);
        self.walk_tree(fs, fs.root(), 0);
        if self.tree_len > 0 {
            if let Some(index) = self
                .tree
                .iter()
                .take(self.tree_len)
                .position(|node| *node == self.current)
            {
                self.tree_cursor = index;
            } else {
                self.tree_cursor = self.tree_cursor.min(self.tree_len - 1);
            }
        } else {
            self.tree_cursor = 0;
        }
        self.clamp_tree_scroll();
    }

    fn walk_tree(&mut self, fs: &dyn FileSystem, node: NodeId, depth: usize) {
        if self.tree_len >= MAX_TREE || depth > 16 {
            return;
        }
        self.tree[self.tree_len] = node;
        self.tree_depth[self.tree_len] = depth.min(255) as u8;
        self.tree_len += 1;
        if !bit(&self.expanded, node.0 as usize) {
            return;
        }
        for index in 0..fs.child_count(node) {
            if self.tree_len >= MAX_TREE {
                return;
            }
            if let Some(child) = fs.child_at(node, index) {
                if fs.is_dir(child) {
                    self.walk_tree(fs, child, depth + 1);
                }
            }
        }
    }

    fn run_search(&mut self, fs: &dyn FileSystem) {
        self.search_count = 0;
        let mut query = Text::<64>::new();
        query.set(self.search.as_str());
        if !query.is_empty() {
            self.search_dir(fs, self.current, query.as_str(), self.include_subfolders, 0);
        }
        self.clear_selection();
        self.cursor = 0;
        self.anchor = 0;
        self.scroll = 0;
        self.rebuild(fs);
        self.preview_node = None;
        self.sync_preview(fs);
    }

    fn search_dir(
        &mut self,
        fs: &dyn FileSystem,
        directory: NodeId,
        query: &str,
        recurse: bool,
        depth: usize,
    ) {
        if depth > 12 || self.search_count >= MAX_SEARCH {
            return;
        }
        for index in 0..fs.child_count(directory) {
            if self.search_count >= MAX_SEARCH {
                return;
            }
            let Some(child) = fs.child_at(directory, index) else {
                continue;
            };
            let name = fs.name(child).unwrap_or("");
            if contains_ignore_ascii_case(name, query) {
                self.search_results[self.search_count] = child;
                self.search_count += 1;
            }
            if recurse && fs.is_dir(child) {
                self.search_dir(fs, child, query, recurse, depth + 1);
            }
        }
    }

    fn sync_preview(&mut self, fs: &dyn FileSystem) {
        let node = self.target_node();
        if self.preview_node == Some(node) {
            return;
        }
        self.preview_node = Some(node);
        self.preview_scroll = 0;
        self.preview_len = 0;
        if fs.is_file(node) {
            self.preview_len = fs.read(node, 0, &mut self.preview);
        }
    }

    fn open_focused(&mut self, fs: &dyn FileSystem) {
        if let Some(node) = self.focused_node() {
            if fs.is_dir(node) {
                self.navigate(fs, node, true);
            } else {
                self.open_viewer(fs, node);
            }
        }
    }

    fn open_viewer(&mut self, fs: &dyn FileSystem, node: NodeId) {
        if !fs.is_file(node) {
            self.navigate(fs, node, true);
            return;
        }
        self.viewer_len = fs.read(node, 0, &mut self.viewer);
        self.viewer_scroll = 0;
        self.viewer_lines = viewer_line_count(&self.viewer[..self.viewer_len]);
        self.dialog = Dialog::Viewer(node);
    }

    fn show_notice(&mut self, title: &str, body: &str, now: u64) {
        self.notice_title.set(title);
        self.notice_body.set(body);
        self.dialog = Dialog::Notice;
        self.menu_open = None;
        self.context = None;
        self.message.clear();
        self.message_until = now;
    }

    fn set_message(&mut self, text: &str, now: u64) {
        if self.message.set(text) {
            self.message_until = now.saturating_add(3500);
        } else {
            self.message_until = now.saturating_add(3500);
        }
    }

    fn read_only(&mut self, now: u64) {
        self.show_notice(
            "Read-only filesystem",
            "This filesystem is compiled into the kernel image. Delete, rename, \
             copy, paste and new-folder operations are not available.",
            now,
        );
    }

    fn commit_address(&mut self, fs: &dyn FileSystem, now: u64) {
        let mut input = Text::<PATH_MAX>::new();
        input.set(self.address.as_str().trim());
        if input.is_empty() {
            self.refresh_address(fs);
            self.address_edit = false;
            return;
        }
        let Some(node) = fs.resolve(input.as_str()) else {
            self.set_message(
                "The path is not valid. Check the address and try again.",
                now,
            );
            return;
        };
        self.address_edit = false;
        if fs.is_dir(node) {
            self.navigate(fs, node, true);
        } else {
            self.open_viewer(fs, node);
            self.address.set(input.as_str());
        }
    }

    fn action_enabled(&self, action: Action, fs: &dyn FileSystem) -> bool {
        match action {
            Action::None => false,
            Action::Open | Action::Properties | Action::CopyPath => self.entry_count > 0,
            Action::Explore => self
                .focused_node()
                .map(|node| fs.is_dir(node))
                .unwrap_or(false),
            Action::Back => self.history_pos > 0,
            Action::Forward => self.history_pos + 1 < self.history_len,
            Action::Up => self.current != fs.root(),
            Action::SelectAll | Action::InvertSelection => self.entry_count > 0,
            _ => true,
        }
    }

    fn action_checked(&self, action: Action) -> bool {
        match action {
            Action::ToggleFolders => self.bar == ExplorerBar::Folders,
            Action::ToggleSearch => self.bar == ExplorerBar::Search,
            Action::TogglePreview => self.preview_open,
            Action::ToggleStatus => self.status_open,
            Action::ToggleToolbar => self.toolbar_open,
            Action::ToggleAddress => self.address_open,
            Action::ViewDetails => self.view == ViewMode::Details,
            Action::ViewIcons => self.view == ViewMode::Icons,
            Action::ViewList => self.view == ViewMode::List,
            Action::ViewTiles => self.view == ViewMode::Tiles,
            Action::SortName => self.sort == SortKey::Name,
            Action::SortSize => self.sort == SortKey::Size,
            Action::SortType => self.sort == SortKey::Type,
            Action::SortAscending => !self.sort_desc,
            Action::SortDescending => self.sort_desc,
            _ => false,
        }
    }

    fn perform(
        &mut self,
        fs: &dyn FileSystem,
        action: Action,
        target: Option<NodeId>,
        now: u64,
    ) -> bool {
        self.menu_open = None;
        self.context = None;
        if let Some(node) = target {
            if let Some(index) = self.entry_position(node) {
                self.select_only(index);
            }
            self.sync_preview(fs);
        }
        let node = target.or_else(|| self.focused_node());
        match action {
            Action::None => {}
            Action::Open => {
                if let Some(node) = node {
                    if fs.is_dir(node) {
                        self.navigate(fs, node, true);
                    } else {
                        self.open_viewer(fs, node);
                    }
                }
            }
            Action::Explore => {
                if let Some(node) = node {
                    if fs.is_dir(node) {
                        self.navigate(fs, node, true);
                    }
                }
            }
            Action::Search => {
                self.bar = ExplorerBar::Search;
                self.focus = Focus::Search;
                self.address_edit = false;
                self.search_dirty = true;
            }
            Action::Properties => {
                if let Some(node) = node {
                    self.dialog = Dialog::Properties(node);
                }
            }
            Action::Close => self.exit = true,
            Action::SelectAll => self.select_all(),
            Action::InvertSelection => self.invert_selection(),
            Action::Back => self.history_back(fs),
            Action::Forward => self.history_forward(fs),
            Action::Up => self.go_up(fs),
            Action::Refresh => {
                self.refresh(fs);
                self.set_message("The folder has been refreshed.", now);
            }
            Action::ViewDetails => self.view = ViewMode::Details,
            Action::ViewIcons => self.view = ViewMode::Icons,
            Action::ViewList => self.view = ViewMode::List,
            Action::ViewTiles => self.view = ViewMode::Tiles,
            Action::ToggleFolders => {
                self.bar = if self.bar == ExplorerBar::Folders {
                    ExplorerBar::None
                } else {
                    ExplorerBar::Folders
                };
                if self.bar == ExplorerBar::None && self.focus == Focus::Tree {
                    self.focus = Focus::List;
                }
            }
            Action::ToggleSearch => {
                self.bar = if self.bar == ExplorerBar::Search {
                    ExplorerBar::Folders
                } else {
                    ExplorerBar::Search
                };
                if self.bar == ExplorerBar::Search {
                    self.focus = Focus::Search;
                    self.search_dirty = true;
                } else if self.focus == Focus::Search {
                    self.focus = Focus::List;
                }
            }
            Action::HideExplorerBar => {
                self.bar = ExplorerBar::None;
                if self.focus == Focus::Tree || self.focus == Focus::Search {
                    self.focus = Focus::List;
                }
            }
            Action::TogglePreview => {
                self.preview_open = !self.preview_open;
                self.sync_preview(fs);
            }
            Action::ToggleStatus => self.status_open = !self.status_open,
            Action::ToggleToolbar => self.toolbar_open = !self.toolbar_open,
            Action::ToggleAddress => self.address_open = !self.address_open,
            Action::SortName => self.set_sort(SortKey::Name),
            Action::SortSize => self.set_sort(SortKey::Size),
            Action::SortType => self.set_sort(SortKey::Type),
            Action::SortAscending => {
                self.sort_desc = false;
                self.rebuild(fs);
            }
            Action::SortDescending => {
                self.sort_desc = true;
                self.rebuild(fs);
            }
            Action::CopyPath => {
                if let Some(node) = node {
                    let mut buffer = [0u8; PATH_MAX];
                    let path = fs.path(node, &mut buffer).unwrap_or("/");
                    self.address.set(path);
                    self.address_edit = false;
                    self.set_message("Path copied to the Address bar.", now);
                }
            }
            Action::FolderOptions => self.dialog = Dialog::Options,
            Action::About => self.dialog = Dialog::About,
            Action::Delete => self.read_only(now),
            Action::NewFolder => self.read_only(now),
            Action::Cut | Action::Copy | Action::Paste => self.read_only(now),
            Action::MapDrive => self.show_notice(
                "Map Network Drive",
                "Network drives are not available on this board.",
                now,
            ),
            Action::Help => self.show_notice(
                "File Explorer Help",
                "Use the mouse or the arrow keys. F3 searches, F5 refreshes, \
                 Ctrl+L focuses the Address bar, Backspace goes up, Enter opens \
                 an item, and Esc closes the explorer.",
                now,
            ),
            Action::Home | Action::MyComputer | Action::Desktop => {
                self.navigate(fs, fs.root(), true)
            }
            Action::MyDocuments => {
                let node = fs.resolve("/docs").unwrap_or_else(|| fs.root());
                self.navigate(fs, node, true);
            }
        }
        self.clamp_scroll();
        self.sync_preview(fs);
        self.exit
    }

    fn set_sort(&mut self, sort: SortKey) {
        if self.sort == sort {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort = sort;
            self.sort_desc = false;
        }
    }

    fn cycle_focus(&mut self, reverse: bool) {
        let mut focus = self.focus;
        for _ in 0..5 {
            focus = match (focus, reverse) {
                (Focus::List, false) => {
                    if self.bar != ExplorerBar::None {
                        Focus::Tree
                    } else if self.address_open {
                        Focus::Address
                    } else {
                        Focus::List
                    }
                }
                (Focus::Tree, false) => {
                    if self.bar == ExplorerBar::Search {
                        Focus::Search
                    } else if self.address_open {
                        Focus::Address
                    } else {
                        Focus::List
                    }
                }
                (Focus::Search, false) => {
                    if self.address_open {
                        Focus::Address
                    } else {
                        Focus::List
                    }
                }
                (Focus::Address, false) => Focus::List,
                (Focus::List, true) => {
                    if self.address_open {
                        Focus::Address
                    } else if self.bar != ExplorerBar::None {
                        Focus::Tree
                    } else {
                        Focus::List
                    }
                }
                (Focus::Address, true) => {
                    if self.bar == ExplorerBar::Search {
                        Focus::Search
                    } else if self.bar != ExplorerBar::None {
                        Focus::Tree
                    } else {
                        Focus::List
                    }
                }
                (Focus::Search, true) => Focus::Tree,
                (Focus::Tree, true) => Focus::List,
            };
            if focus == Focus::Tree && self.bar == ExplorerBar::None {
                continue;
            }
            if focus == Focus::Search && self.bar != ExplorerBar::Search {
                continue;
            }
            if focus == Focus::Address && !self.address_open {
                continue;
            }
            break;
        }
        self.focus = focus;
        self.address_edit = focus == Focus::Address;
        if focus == Focus::Address {
            self.address.set_cursor(self.address.len());
        }
    }

    fn tree_move(&mut self, fs: &dyn FileSystem, delta: isize) {
        if self.tree_len == 0 {
            return;
        }
        if delta < 0 {
            self.tree_cursor = self.tree_cursor.saturating_sub((-delta) as usize);
        } else {
            self.tree_cursor = (self.tree_cursor + delta as usize).min(self.tree_len - 1);
        }
        if let Some(node) = self.tree.get(self.tree_cursor).copied() {
            if fs.is_dir(node) {
                self.sync_preview(fs);
            }
        }
        self.clamp_tree_scroll();
    }

    fn tree_expand(&mut self, fs: &dyn FileSystem) {
        if let Some(node) = self.tree.get(self.tree_cursor).copied() {
            if !has_subdirectories(fs, node) {
                return;
            }
            if !bit(&self.expanded, node.0 as usize) {
                set_bit(&mut self.expanded, node.0 as usize);
                self.rebuild_tree(fs);
            } else if self.tree_cursor + 1 < self.tree_len
                && self.tree_depth[self.tree_cursor + 1] > self.tree_depth[self.tree_cursor]
            {
                self.tree_cursor += 1;
                self.clamp_tree_scroll();
            }
        }
    }

    fn tree_collapse(&mut self, fs: &dyn FileSystem) {
        if let Some(node) = self.tree.get(self.tree_cursor).copied() {
            if bit(&self.expanded, node.0 as usize) && has_subdirectories(fs, node) {
                clear_bit(&mut self.expanded, node.0 as usize);
                self.rebuild_tree(fs);
            } else if let Some(parent) = fs.parent(node) {
                if let Some(index) = self
                    .tree
                    .iter()
                    .take(self.tree_len)
                    .position(|item| *item == parent)
                {
                    self.tree_cursor = index;
                    self.clamp_tree_scroll();
                }
            }
        }
    }

    fn clamp_scroll(&mut self) {
        let max = self.max_scroll();
        if self.scroll > max {
            self.scroll = max;
        }
    }

    fn clamp_tree_scroll(&mut self) {
        let layout = Layout::compute(self.screen, self);
        let visible = (layout.tree.height as i32 / TREE_ROW).max(1) as usize;
        if self.tree_cursor < self.tree_scroll {
            self.tree_scroll = self.tree_cursor;
        } else if self.tree_cursor >= self.tree_scroll + visible {
            self.tree_scroll = self.tree_cursor + 1 - visible;
        }
        let max = self.tree_len.saturating_sub(visible);
        if self.tree_scroll > max {
            self.tree_scroll = max;
        }
    }

    fn max_scroll(&self) -> usize {
        let layout = Layout::compute(self.screen, self);
        let metrics = grid_metrics(self, layout.list, layout.main.width);
        metrics.rows.saturating_sub(metrics.visible_rows)
    }

    fn ensure_cursor_visible(&mut self) {
        let layout = Layout::compute(self.screen, self);
        let metrics = grid_metrics(self, layout.list, layout.main.width);
        if metrics.columns == 0 || metrics.rows == 0 {
            self.scroll = 0;
            return;
        }
        let row = self.cursor / metrics.columns;
        if row < self.scroll {
            self.scroll = row;
        } else if row >= self.scroll + metrics.visible_rows {
            self.scroll = row + 1 - metrics.visible_rows;
        }
        let max = metrics.rows.saturating_sub(metrics.visible_rows);
        if self.scroll > max {
            self.scroll = max;
        }
    }

    fn handle_pointer(&mut self, fs: &dyn FileSystem, report: PointerEvent, now: u64) -> bool {
        self.pointer = Point::new(report.x as i32, report.y as i32);
        self.has_pointer = true;

        if let Some(true) = report.left {
            self.mouse_down = true;
        }
        if self.mouse_down && report.wheel != 0 {
            self.handle_wheel(fs, report.wheel);
        }

        if let Some(true) = report.right {
            self.on_right_click(fs, self.pointer, now);
        }

        if let Some(true) = report.left {
            let exit = self.on_left_down(fs, self.pointer, now);
            if exit {
                self.exit = true;
            }
        }

        if self.mouse_down {
            self.update_drag();
        }

        if let Some(false) = report.left {
            self.finish_drag(fs);
            self.mouse_down = false;
        }
        if let Some(false) = report.right {
            // Nothing to do; the context menu is opened on press.
        }
        if let Some(false) = report.middle {
            // The middle button is intentionally unused.
        }

        if report.wheel != 0 && !self.mouse_down {
            self.handle_wheel(fs, report.wheel);
        }
        self.exit
    }

    fn update_drag(&mut self) {
        match self.drag {
            DragMode::Move { dx, dy } => {
                if !self.maximized {
                    self.window_x = self.pointer.x - dx;
                    self.window_y = self.pointer.y - dy;
                    self.window_x = self
                        .window_x
                        .clamp(0, (self.screen.width as i32 - self.window_w).max(0));
                    self.window_y = self
                        .window_y
                        .clamp(0, (self.screen.height as i32 - self.window_h).max(0));
                }
            }
            DragMode::Resize {
                edge,
                x,
                y,
                width,
                height,
                pointer_x,
                pointer_y,
            } => {
                if !self.maximized {
                    const MIN_WIDTH: i32 = 720;
                    const MIN_HEIGHT: i32 = 480;
                    let dx = self.pointer.x - pointer_x;
                    let dy = self.pointer.y - pointer_y;
                    let max_right = self.screen.width as i32;
                    let max_bottom = self.screen.height as i32;
                    let mut left = x;
                    let mut top = y;
                    let mut right = (x + width).min(max_right);
                    let mut bottom = (y + height).min(max_bottom);

                    if edge.moves_left() {
                        left = (x + dx).clamp(0, right - MIN_WIDTH);
                    } else if edge.moves_right() {
                        right = (x + width + dx).clamp(left + MIN_WIDTH, max_right);
                    }
                    if edge.moves_top() {
                        top = (y + dy).clamp(0, bottom - MIN_HEIGHT);
                    } else if edge.moves_bottom() {
                        bottom = (y + height + dy).clamp(top + MIN_HEIGHT, max_bottom);
                    }

                    self.window_x = left;
                    self.window_y = top;
                    self.window_w = (right - left).max(1);
                    self.window_h = (bottom - top).max(1);
                }
            }
            DragMode::Marquee { .. } => {}
            DragMode::None => {}
        }
    }

    fn finish_drag(&mut self, fs: &dyn FileSystem) {
        if let DragMode::Marquee { start } = self.drag {
            self.select_marquee(fs, start, self.pointer);
        }
        self.drag = DragMode::None;
    }

    fn select_marquee(&mut self, _fs: &dyn FileSystem, a: Point, b: Point) {
        let x0 = a.x.min(b.x);
        let y0 = a.y.min(b.y);
        let x1 = a.x.max(b.x);
        let y1 = a.y.max(b.y);
        let layout = Layout::compute(self.screen, self);
        let metrics = grid_metrics(self, layout.list, layout.main.width);
        if metrics.columns == 0 {
            return;
        }
        self.clear_selection();
        for index in 0..self.entry_count {
            if let Some(rect) = entry_rect(self, layout, metrics, index) {
                if rect.right_i32() >= x0 && rect.x <= x1 && rect.bottom_i32() >= y0 && rect.y <= y1
                {
                    self.select_node(self.entries[index]);
                }
            }
        }
    }

    fn on_left_down(&mut self, fs: &dyn FileSystem, point: Point, now: u64) -> bool {
        let layout = Layout::compute(self.screen, self);

        if self.dialog != Dialog::None {
            return self.dialog_click(fs, point, now);
        }
        if let Some(context) = self.context {
            if context_rect(self, context).contains(point) {
                if let Some(index) = context_item_at(self, context, point) {
                    let entries = if context.background {
                        CONTEXT_BACKGROUND
                    } else {
                        CONTEXT_ITEM
                    };
                    if let Some(entry) = entries.get(index).copied() {
                        if entry.enabled && !entry.separator {
                            let target = if context.background {
                                Some(self.current)
                            } else {
                                context.target
                            };
                            return self.perform(fs, entry.action, target, now);
                        }
                    }
                }
                return false;
            }
            self.context = None;
            return false;
        }
        if let Some(menu) = self.menu_open {
            if let Some(index) = menu_item_at(self, menu, point) {
                let entries = menu_items(menu);
                if let Some(entry) = entries.get(index).copied() {
                    if entry.enabled && !entry.separator && self.action_enabled(entry.action, fs) {
                        return self.perform(fs, entry.action, None, now);
                    }
                }
                return false;
            }
            for kind in MenuKind::ALL {
                if menu_label_rect(layout, kind).contains(point) {
                    if kind == menu {
                        self.menu_open = None;
                    } else {
                        self.menu_open = Some(kind);
                        self.menu_hover = first_enabled(kind, self, fs);
                    }
                    return false;
                }
            }
            self.menu_open = None;
            return false;
        }
        if self.address_dropdown {
            if let Some(index) = address_history_at(self, layout, point) {
                self.address_dropdown = false;
                self.jump_history_index(fs, index);
                return false;
            }
            self.address_dropdown = false;
        }

        for kind in MenuKind::ALL {
            if menu_label_rect(layout, kind).contains(point) {
                self.menu_open = Some(kind);
                self.menu_hover = first_enabled(kind, self, fs);
                self.focus = Focus::List;
                return false;
            }
        }

        if let Some(button) = title_button_at(layout, point) {
            match button {
                0 => self.set_message("The desktop controls minimize from the taskbar.", now),
                1 => {
                    self.maximized = !self.maximized;
                    if !self.maximized {
                        self.window_w = (self.screen.width as i32 - 120)
                            .max(720)
                            .min(self.screen.width as i32);
                        self.window_h = (self.screen.height as i32 - 100)
                            .max(480)
                            .min(self.screen.height as i32);
                        self.window_x = (self.screen.width as i32 - self.window_w) / 2;
                        self.window_y = (self.screen.height as i32 - self.window_h) / 2;
                    }
                }
                2 => {
                    self.exit = true;
                    return true;
                }
                _ => {}
            }
            return false;
        }
        if !self.maximized {
            if let Some(edge) = resize_edge_at(layout.window, point) {
                self.drag = DragMode::Resize {
                    edge,
                    x: self.window_x,
                    y: self.window_y,
                    width: self.window_w,
                    height: self.window_h,
                    pointer_x: point.x,
                    pointer_y: point.y,
                };
                return false;
            }
        }
        if layout.title.contains(point) && !self.maximized {
            self.drag = DragMode::Move {
                dx: point.x - self.window_x,
                dy: point.y - self.window_y,
            };
            return false;
        }

        if self.toolbar_open {
            if let Some(button) = toolbar_button_at(layout, point) {
                self.focus = Focus::List;
                return match button {
                    0 => self.perform(fs, Action::Back, None, now),
                    1 => self.perform(fs, Action::Forward, None, now),
                    2 => self.perform(fs, Action::Up, None, now),
                    3 => self.perform(fs, Action::ToggleSearch, None, now),
                    4 => self.perform(fs, Action::ToggleFolders, None, now),
                    _ => {
                        self.menu_open = Some(MenuKind::View);
                        self.menu_hover = first_enabled(MenuKind::View, self, fs);
                        false
                    }
                };
            }
        }

        if self.address_open {
            if address_arrow_rect(layout).contains(point) {
                self.address_dropdown = !self.address_dropdown;
                return false;
            }
            if address_field_rect(layout).contains(point) || go_button_rect(layout).contains(point)
            {
                self.focus = Focus::Address;
                self.address_edit = true;
                self.address.set_cursor(self.address.len());
                if go_button_rect(layout).contains(point) {
                    self.commit_address(fs, now);
                }
                return false;
            }
        }

        if let Some(sidebar) = layout.sidebar {
            if sidebar.contains(point) {
                return self.sidebar_click(fs, layout, sidebar, point, now);
            }
        }

        if let Some(preview) = layout.preview {
            if preview.contains(point) {
                self.focus = Focus::List;
                if let Some(index) = entry_at(self, layout, point) {
                    self.click_entry(fs, index, point, now, false);
                }
                return false;
            }
        }

        if layout.main.contains(point) {
            self.focus = Focus::List;
            if let Some(header) = layout.header {
                if header.contains(point) {
                    self.click_header(layout, point);
                    return false;
                }
            }
            if layout.list.contains(point) {
                if let Some(index) = entry_at(self, layout, point) {
                    self.click_entry(fs, index, point, now, false);
                } else {
                    self.clear_selection();
                    self.anchor = self.cursor;
                    self.drag = DragMode::Marquee { start: point };
                }
                return false;
            }
        }
        false
    }

    fn click_entry(
        &mut self,
        fs: &dyn FileSystem,
        index: usize,
        point: Point,
        now: u64,
        right: bool,
    ) {
        let same = self.last_click_node == self.entries.get(index).copied()
            && now.saturating_sub(self.last_click_ms) < 480;
        if right {
            if !self.is_selected(self.entries[index]) {
                self.select_only(index);
            } else {
                self.cursor = index;
            }
            self.last_click_node = self.entries.get(index).copied();
            self.last_click_ms = now;
            return;
        }
        self.select_only(index);
        self.last_click_node = self.entries.get(index).copied();
        self.last_click_ms = now;
        self.sync_preview(fs);
        if same {
            self.last_click_node = None;
            self.open_focused(fs);
        }
        let _ = point;
    }

    fn click_header(&mut self, layout: Layout, point: Point) {
        let Some(header) = layout.header else {
            return;
        };
        let width = header.width as i32;
        let name = width * 45 / 100;
        let size = width * 15 / 100;
        let kind = width * 24 / 100;
        let key = if point.x < header.x + name {
            SortKey::Name
        } else if point.x < header.x + name + size {
            SortKey::Size
        } else if point.x < header.x + name + size + kind {
            SortKey::Type
        } else {
            return;
        };
        self.set_sort(key);
        // Header clicks rebuild below; the filesystem is supplied by the caller
        // through refresh in the event loop. This action only re-orders local
        // entries, so mark the sort dirty via a dedicated field-less rebuild is
        // not possible here; the next event refreshes it. See `after_input`.
        self.sort_pending = true;
    }

    fn sidebar_click(
        &mut self,
        fs: &dyn FileSystem,
        layout: Layout,
        sidebar: Rect,
        point: Point,
        now: u64,
    ) -> bool {
        match self.bar {
            ExplorerBar::Folders => {
                if layout.tree.contains(point) {
                    self.focus = Focus::Tree;
                    let visible = (layout.tree.height as i32 / TREE_ROW).max(1) as usize;
                    let row = ((point.y - layout.tree.y) / TREE_ROW) as usize;
                    let index = self.tree_scroll + row;
                    if row < visible && index < self.tree_len {
                        self.tree_cursor = index;
                        let node = self.tree[index];
                        let depth = self.tree_depth[index] as i32;
                        let arrow_x = layout.tree.x + 5 + depth * 12;
                        if point.x >= arrow_x
                            && point.x < arrow_x + 14
                            && has_subdirectories(fs, node)
                        {
                            self.toggle_expanded(fs, node);
                            if let Some(index) = self
                                .tree
                                .iter()
                                .take(self.tree_len)
                                .position(|item| *item == self.current)
                            {
                                self.tree_cursor = index;
                            }
                        } else if fs.is_dir(node) {
                            self.navigate(fs, node, true);
                        }
                    }
                    return false;
                }
                for index in 0..4 {
                    if task_link_rect(layout, index).contains(point) {
                        let action = match index {
                            0 => Action::Open,
                            1 => Action::Search,
                            2 => Action::Properties,
                            _ => Action::Refresh,
                        };
                        return self.perform(fs, action, None, now);
                    }
                }
            }
            ExplorerBar::Search => {
                self.focus = Focus::Search;
                if search_field_rect(sidebar).contains(point) {
                    self.address_edit = false;
                    return false;
                }
                if search_check_rect(sidebar).contains(point) {
                    self.include_subfolders = !self.include_subfolders;
                    self.search_dirty = true;
                    return false;
                }
                if search_button_rect(sidebar).contains(point) {
                    self.run_search(fs);
                    return false;
                }
                if search_clear_rect(sidebar).contains(point) {
                    self.search.clear();
                    self.search_count = 0;
                    self.rebuild(fs);
                    return false;
                }
            }
            ExplorerBar::None => {}
        }
        false
    }

    fn on_right_click(&mut self, fs: &dyn FileSystem, point: Point, now: u64) {
        let layout = Layout::compute(self.screen, self);
        if self.dialog != Dialog::None {
            return;
        }
        if let Some(menu) = self.menu_open {
            if menu_rect(self, menu).contains(point)
                || menu_label_rect(layout, menu).contains(point)
            {
                return;
            }
            self.menu_open = None;
        }
        self.context = None;
        if layout.tree.contains(point) {
            let visible = (layout.tree.height as i32 / TREE_ROW).max(1) as usize;
            let row = ((point.y - layout.tree.y) / TREE_ROW) as usize;
            let index = self.tree_scroll + row;
            if row < visible && index < self.tree_len {
                self.tree_cursor = index;
                let node = self.tree[index];
                self.context = Some(ContextMenu {
                    target: Some(node),
                    background: false,
                    x: point.x,
                    y: point.y,
                    hover: usize::MAX,
                });
                return;
            }
        }
        if layout.list.contains(point) {
            if let Some(index) = entry_at(self, layout, point) {
                self.click_entry(fs, index, point, now, true);
                let node = self.entries[index];
                self.context = Some(ContextMenu {
                    target: Some(node),
                    background: false,
                    x: point.x,
                    y: point.y,
                    hover: usize::MAX,
                });
            } else {
                self.context = Some(ContextMenu {
                    target: Some(self.current),
                    background: true,
                    x: point.x,
                    y: point.y,
                    hover: usize::MAX,
                });
            }
        } else if let Some(preview) = layout.preview {
            if preview.contains(point) {
                if let Some(node) = self.focused_node() {
                    self.context = Some(ContextMenu {
                        target: Some(node),
                        background: false,
                        x: point.x,
                        y: point.y,
                        hover: usize::MAX,
                    });
                }
            }
        } else if layout.main.contains(point) || layout.body.contains(point) {
            self.context = Some(ContextMenu {
                target: Some(self.current),
                background: true,
                x: point.x,
                y: point.y,
                hover: usize::MAX,
            });
        }
    }

    fn handle_wheel(&mut self, fs: &dyn FileSystem, wheel: i32) {
        let point = self.pointer;
        let layout = Layout::compute(self.screen, self);
        if let Dialog::Viewer(node) = self.dialog {
            let dir = if wheel > 0 { -1 } else { 1 };
            self.scroll_viewer(fs, node, dir * 3);
            return;
        }
        if self.dialog != Dialog::None {
            return;
        }
        if let Some(preview) = layout.preview {
            if preview.contains(point) {
                if wheel > 0 {
                    self.preview_scroll = self.preview_scroll.saturating_sub(3);
                } else {
                    self.preview_scroll = self.preview_scroll.saturating_add(3);
                }
                return;
            }
        }
        if let Some(sidebar) = layout.sidebar {
            if sidebar.contains(point)
                && self.bar == ExplorerBar::Folders
                && layout.tree.contains(point)
            {
                let max = self
                    .tree_len
                    .saturating_sub((layout.tree.height as i32 / TREE_ROW).max(1) as usize);
                if wheel > 0 {
                    self.tree_scroll = self.tree_scroll.saturating_sub(3);
                } else {
                    self.tree_scroll = (self.tree_scroll + 3).min(max);
                }
                return;
            }
        }
        if layout.list.contains(point) {
            let max = self.max_scroll();
            if wheel > 0 {
                self.scroll = self.scroll.saturating_sub(3);
            } else {
                self.scroll = (self.scroll + 3).min(max);
            }
        }
    }

    fn scroll_viewer(&mut self, fs: &dyn FileSystem, node: NodeId, delta: isize) {
        if !fs.is_file(node) {
            return;
        }
        let visible = self.viewer_visible_lines();
        let max = self.viewer_lines.saturating_sub(visible);
        if delta < 0 {
            self.viewer_scroll = self.viewer_scroll.saturating_sub((-delta) as usize);
        } else {
            self.viewer_scroll = (self.viewer_scroll + delta as usize).min(max);
        }
    }

    fn viewer_visible_lines(&self) -> usize {
        let layout = DialogLayout::compute(self.screen, Dialog::Viewer(self.target_node()));
        ((layout.body.height.saturating_sub(42)) / 10).max(1) as usize
    }

    fn jump_history_index(&mut self, fs: &dyn FileSystem, index: usize) {
        if index < self.history_len {
            self.history_pos = index;
            self.jump_history(fs);
        }
    }

    fn after_input(&mut self, fs: &dyn FileSystem, now: u64) {
        if self.sort_pending {
            self.sort_pending = false;
            self.refresh(fs);
        }
        if self.search_dirty {
            self.search_dirty = false;
            if self.bar == ExplorerBar::Search {
                self.run_search(fs);
            }
        }
        if self.message_until != 0 && now >= self.message_until {
            self.message.clear();
            self.message_until = 0;
        }
        self.ensure_cursor_visible();
        self.clamp_tree_scroll();
        self.sync_preview(fs);
    }

    fn handle_key(
        &mut self,
        fs: &dyn FileSystem,
        key: Key,
        modifiers: Modifiers,
        now: u64,
    ) -> bool {
        if self.dialog != Dialog::None {
            self.dialog_key(fs, key, now);
            return false;
        }
        if self.menu_open.is_some() {
            self.menu_key(fs, key, now);
            return false;
        }
        if let Some(context) = self.context {
            match key {
                Key::Escape => self.context = None,
                Key::Up => self.context = Some(context_move(self, context, -1)),
                Key::Down => self.context = Some(context_move(self, context, 1)),
                Key::Enter => {
                    let entries = if context.background {
                        CONTEXT_BACKGROUND
                    } else {
                        CONTEXT_ITEM
                    };
                    if let Some(entry) = entries.get(context.hover).copied() {
                        if entry.enabled && !entry.separator {
                            let target = if context.background {
                                Some(self.current)
                            } else {
                                context.target
                            };
                            return self.perform(fs, entry.action, target, now);
                        }
                    }
                }
                _ => {}
            }
            return false;
        }
        if self.address_dropdown {
            if key == Key::Escape {
                self.address_dropdown = false;
                return false;
            }
        }

        if key == Key::F1 {
            self.dialog = Dialog::About;
            return false;
        }
        if key == Key::F3 {
            return self.perform(fs, Action::Search, None, now);
        }
        if key == Key::F5 {
            return self.perform(fs, Action::Refresh, None, now);
        }
        if key == Key::F10 {
            self.menu_open = Some(MenuKind::File);
            self.menu_hover = first_enabled(MenuKind::File, self, fs);
            return false;
        }
        if key == Key::F6 {
            self.cycle_focus(modifiers.contains(Modifiers::SHIFT));
            return false;
        }

        if modifiers.contains(Modifiers::CTRL) {
            match key {
                Key::Character('a') => {
                    if self.focus == Focus::Address {
                        self.address.select_all();
                    } else if self.focus == Focus::Search {
                        self.search.select_all();
                    } else {
                        self.select_all();
                    }
                    return false;
                }
                Key::Character('l') => {
                    self.focus = Focus::Address;
                    self.address_edit = true;
                    self.address.set_cursor(self.address.len());
                    return false;
                }
                Key::Character('f') => {
                    return self.perform(fs, Action::Search, None, now);
                }
                Key::Character('r') => {
                    return self.perform(fs, Action::Refresh, None, now);
                }
                Key::Character('o') => {
                    self.open_focused(fs);
                    return false;
                }
                Key::Character('p') => {
                    return self.perform(fs, Action::Properties, None, now);
                }
                Key::Character('c') => {
                    return self.perform(fs, Action::CopyPath, None, now);
                }
                Key::Character('i') => {
                    self.invert_selection();
                    return false;
                }
                Key::Left => {
                    self.history_back(fs);
                    return false;
                }
                Key::Right => {
                    self.history_forward(fs);
                    return false;
                }
                Key::Up => {
                    self.go_up(fs);
                    return false;
                }
                _ => {}
            }
        }

        if modifiers.contains(Modifiers::ALT) {
            match key {
                Key::Left => {
                    self.history_back(fs);
                    return false;
                }
                Key::Right => {
                    self.history_forward(fs);
                    return false;
                }
                Key::Up => {
                    self.go_up(fs);
                    return false;
                }
                Key::Enter => {
                    return self.perform(fs, Action::Properties, None, now);
                }
                _ => {}
            }
        }

        if key == Key::Tab {
            self.cycle_focus(modifiers.contains(Modifiers::SHIFT));
            return false;
        }

        match self.focus {
            Focus::Address => {
                self.address_key(fs, key, modifiers, now);
            }
            Focus::Search => {
                self.search_key(fs, key, modifiers, now);
            }
            Focus::Tree => {
                self.tree_key(fs, key, now);
            }
            Focus::List => {
                self.list_key(fs, key, modifiers, now);
            }
        }
        false
    }

    fn address_key(&mut self, fs: &dyn FileSystem, key: Key, modifiers: Modifiers, now: u64) {
        match key {
            Key::Enter => self.commit_address(fs, now),
            Key::Escape => {
                self.address_edit = false;
                self.focus = Focus::List;
                self.refresh_address(fs);
            }
            Key::Backspace => {
                self.address.backspace();
            }
            Key::Delete => {
                self.address.delete();
            }
            Key::Left => self.address.move_left(modifiers.contains(Modifiers::SHIFT)),
            Key::Right => self
                .address
                .move_right(modifiers.contains(Modifiers::SHIFT)),
            Key::Home => self.address.move_home(modifiers.contains(Modifiers::SHIFT)),
            Key::End => self.address.move_end(modifiers.contains(Modifiers::SHIFT)),
            _ => {}
        }
    }

    fn search_key(&mut self, fs: &dyn FileSystem, key: Key, modifiers: Modifiers, now: u64) {
        match key {
            Key::Enter => self.run_search(fs),
            Key::Escape => {
                self.focus = Focus::List;
            }
            Key::Backspace => {
                if self.search.backspace() {
                    self.search_dirty = true;
                }
            }
            Key::Delete => {
                if self.search.delete() {
                    self.search_dirty = true;
                }
            }
            Key::Left => self.search.move_left(modifiers.contains(Modifiers::SHIFT)),
            Key::Right => self.search.move_right(modifiers.contains(Modifiers::SHIFT)),
            Key::Home => self.search.move_home(modifiers.contains(Modifiers::SHIFT)),
            Key::End => self.search.move_end(modifiers.contains(Modifiers::SHIFT)),
            _ => {}
        }
        let _ = now;
    }

    fn tree_key(&mut self, fs: &dyn FileSystem, key: Key, _now: u64) {
        match key {
            Key::Up => self.tree_move(fs, -1),
            Key::Down => self.tree_move(fs, 1),
            Key::Home => {
                self.tree_cursor = 0;
                self.clamp_tree_scroll();
            }
            Key::End => {
                self.tree_cursor = self.tree_len.saturating_sub(1);
                self.clamp_tree_scroll();
            }
            Key::PageUp => self.tree_move(fs, -8),
            Key::PageDown => self.tree_move(fs, 8),
            Key::Right => self.tree_expand(fs),
            Key::Enter => {
                if let Some(node) = self.tree.get(self.tree_cursor).copied() {
                    if fs.is_dir(node) {
                        self.navigate(fs, node, true);
                    }
                }
            }
            Key::Left => self.tree_collapse(fs),
            Key::Space => {
                if let Some(node) = self.tree.get(self.tree_cursor).copied() {
                    self.toggle_expanded(fs, node);
                }
            }
            Key::Backspace => self.go_up(fs),
            Key::Escape => self.exit = true,
            Key::Character('q') => self.exit = true,
            _ => {}
        }
    }

    fn list_key(&mut self, fs: &dyn FileSystem, key: Key, modifiers: Modifiers, now: u64) {
        let shift = modifiers.contains(Modifiers::SHIFT);
        let ctrl = modifiers.contains(Modifiers::CTRL);
        let page = self.page_step();
        match key {
            Key::Up => self.move_cursor(fs, self.cursor.saturating_sub(1), shift, ctrl),
            Key::Down => {
                if self.entry_count > 0 {
                    self.move_cursor(fs, (self.cursor + 1).min(self.entry_count - 1), shift, ctrl);
                }
            }
            Key::Left => self.go_up(fs),
            Key::Right => self.open_focused(fs),
            Key::Home => self.move_cursor(fs, 0, shift, ctrl),
            Key::End => {
                if self.entry_count > 0 {
                    self.move_cursor(fs, self.entry_count - 1, shift, ctrl);
                }
            }
            Key::PageUp => self.move_cursor(fs, self.cursor.saturating_sub(page), shift, ctrl),
            Key::PageDown => {
                if self.entry_count > 0 {
                    self.move_cursor(
                        fs,
                        (self.cursor + page).min(self.entry_count - 1),
                        shift,
                        ctrl,
                    );
                }
            }
            Key::Enter => self.open_focused(fs),
            Key::Space => {
                if self.entry_count > 0 {
                    self.toggle_at(self.cursor);
                    self.sync_preview(fs);
                }
            }
            Key::Insert => {
                if self.entry_count > 0 {
                    self.toggle_at(self.cursor);
                    if self.cursor + 1 < self.entry_count {
                        self.cursor += 1;
                    }
                    self.sync_preview(fs);
                }
            }
            Key::Delete => self.read_only(now),
            Key::Backspace => self.go_up(fs),
            Key::Escape => self.exit = true,
            Key::Character('q') if !ctrl => self.exit = true,
            _ => {}
        }
    }

    fn page_step(&self) -> usize {
        let layout = Layout::compute(self.screen, self);
        let metrics = grid_metrics(self, layout.list, layout.main.width);
        metrics.visible_rows.saturating_mul(metrics.columns).max(1)
    }

    fn handle_text(&mut self, ch: char) {
        if self.dialog != Dialog::None || self.menu_open.is_some() || self.context.is_some() {
            return;
        }
        if self.focus == Focus::Address {
            self.address.insert(ch);
        } else if self.focus == Focus::Search {
            if self.search.insert(ch) {
                self.search_dirty = true;
            }
        }
    }

    fn menu_key(&mut self, fs: &dyn FileSystem, key: Key, now: u64) {
        let Some(mut kind) = self.menu_open else {
            return;
        };
        let entries = menu_items(kind);
        match key {
            Key::Escape => self.menu_open = None,
            Key::Up => self.menu_hover = next_enabled(entries, self.menu_hover, -1, self, fs),
            Key::Down => self.menu_hover = next_enabled(entries, self.menu_hover, 1, self, fs),
            Key::Left => {
                let index = if kind.index() == 0 {
                    MenuKind::ALL.len() - 1
                } else {
                    kind.index() - 1
                };
                kind = MenuKind::ALL[index];
                self.menu_open = Some(kind);
                self.menu_hover = first_enabled(kind, self, fs);
            }
            Key::Right => {
                kind = MenuKind::ALL[(kind.index() + 1) % MenuKind::ALL.len()];
                self.menu_open = Some(kind);
                self.menu_hover = first_enabled(kind, self, fs);
            }
            Key::Enter => {
                if let Some(entry) = entries.get(self.menu_hover).copied() {
                    if entry.enabled && !entry.separator {
                        self.perform(fs, entry.action, None, now);
                    }
                }
            }
            _ => {}
        }
    }

    fn dialog_key(&mut self, fs: &dyn FileSystem, key: Key, now: u64) {
        match self.dialog {
            Dialog::Viewer(node) => match key {
                Key::Escape => self.dialog = Dialog::None,
                Key::Up => self.scroll_viewer(fs, node, -1),
                Key::Down => self.scroll_viewer(fs, node, 1),
                Key::PageUp => self.scroll_viewer(fs, node, -12),
                Key::PageDown => self.scroll_viewer(fs, node, 12),
                Key::Home => self.viewer_scroll = 0,
                Key::End => {
                    let visible = self.viewer_visible_lines();
                    self.viewer_scroll = self.viewer_lines.saturating_sub(visible);
                }
                _ => {}
            },
            Dialog::None => {}
            _ => match key {
                Key::Escape | Key::Enter => self.dialog = Dialog::None,
                _ => {}
            },
        }
        let _ = now;
    }

    fn dialog_click(&mut self, fs: &dyn FileSystem, point: Point, now: u64) -> bool {
        let dialog = self.dialog;
        let layout = DialogLayout::compute(self.screen, dialog);
        if dialog == Dialog::None {
            return false;
        }
        if layout.close.contains(point) || layout.ok.contains(point) {
            self.dialog = Dialog::None;
            return false;
        }
        if dialog == Dialog::Options {
            for index in 0..5 {
                if option_check_rect(layout, index).contains(point) {
                    match index {
                        0 => self.toolbar_open = !self.toolbar_open,
                        1 => self.address_open = !self.address_open,
                        2 => self.status_open = !self.status_open,
                        3 => {
                            self.bar = if self.bar == ExplorerBar::Folders {
                                ExplorerBar::None
                            } else {
                                ExplorerBar::Folders
                            }
                        }
                        4 => self.preview_open = !self.preview_open,
                        _ => {}
                    }
                    return false;
                }
            }
        }
        if !layout.window.contains(point) {
            // Modal dialogs ignore clicks outside; only the close/OK buttons
            // dismiss them, matching XP's property sheets.
        }
        let _ = (fs, now);
        false
    }
}

fn bit(words: &[u64; SELECTION_WORDS], index: usize) -> bool {
    let word = index / 64;
    let mask = 1u64 << (index % 64);
    word < SELECTION_WORDS && words[word] & mask != 0
}

fn set_bit(words: &mut [u64; SELECTION_WORDS], index: usize) {
    let word = index / 64;
    if word < SELECTION_WORDS {
        words[word] |= 1u64 << (index % 64);
    }
}

fn clear_bit(words: &mut [u64; SELECTION_WORDS], index: usize) {
    let word = index / 64;
    if word < SELECTION_WORDS {
        words[word] &= !(1u64 << (index % 64));
    }
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.len() > haystack.len() {
        return false;
    }
    for start in 0..=haystack.len() - needle.len() {
        if haystack[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return true;
        }
    }
    false
}

fn compare_names(a: &str, b: &str) -> Ordering {
    let a = a.as_bytes();
    let b = b.as_bytes();
    for (left, right) in a.iter().zip(b) {
        let left = left.to_ascii_lowercase();
        let right = right.to_ascii_lowercase();
        if left != right {
            return left.cmp(&right);
        }
    }
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn file_extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension,
        _ => "",
    }
}

fn type_name(node: NodeId, fs: &dyn FileSystem) -> &'static str {
    if fs.is_dir(node) {
        return "File Folder";
    }
    let extension = file_extension(fs.name(node).unwrap_or(""));
    if extension.eq_ignore_ascii_case("txt") {
        "Text Document"
    } else if extension.eq_ignore_ascii_case("md") {
        "Markdown File"
    } else if extension.eq_ignore_ascii_case("rs") {
        "Rust Source"
    } else if extension.eq_ignore_ascii_case("toml") {
        "TOML File"
    } else if extension.eq_ignore_ascii_case("yaml") || extension.eq_ignore_ascii_case("yml") {
        "YAML Document"
    } else if extension.eq_ignore_ascii_case("json") {
        "JSON File"
    } else if extension.eq_ignore_ascii_case("bmp")
        || extension.eq_ignore_ascii_case("png")
        || extension.eq_ignore_ascii_case("qoi")
        || extension.eq_ignore_ascii_case("ppm")
        || extension.eq_ignore_ascii_case("jpg")
    {
        "Image"
    } else if extension.eq_ignore_ascii_case("fapp") {
        "App Bundle"
    } else if extension.eq_ignore_ascii_case("zip")
        || extension.eq_ignore_ascii_case("tar")
        || extension.eq_ignore_ascii_case("gz")
    {
        "Archive"
    } else {
        "File"
    }
}

fn file_accent(name: &str) -> Color {
    let extension = file_extension(name);
    if extension.eq_ignore_ascii_case("txt") || extension.eq_ignore_ascii_case("md") {
        Color::rgb(60, 130, 220)
    } else if extension.eq_ignore_ascii_case("rs") {
        Color::rgb(222, 112, 52)
    } else if extension.eq_ignore_ascii_case("json")
        || extension.eq_ignore_ascii_case("yaml")
        || extension.eq_ignore_ascii_case("yml")
        || extension.eq_ignore_ascii_case("toml")
    {
        Color::rgb(90, 160, 80)
    } else if extension.eq_ignore_ascii_case("bmp")
        || extension.eq_ignore_ascii_case("png")
        || extension.eq_ignore_ascii_case("qoi")
        || extension.eq_ignore_ascii_case("ppm")
        || extension.eq_ignore_ascii_case("jpg")
    {
        Color::rgb(176, 82, 196)
    } else {
        Color::rgb(120, 120, 120)
    }
}

fn compare_nodes(
    fs: &dyn FileSystem,
    a: NodeId,
    b: NodeId,
    sort: SortKey,
    descending: bool,
) -> Ordering {
    let a_dir = fs.is_dir(a);
    let b_dir = fs.is_dir(b);
    if a_dir != b_dir {
        return if a_dir {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    let order = match sort {
        SortKey::Name => compare_names(fs.name(a).unwrap_or(""), fs.name(b).unwrap_or("")),
        SortKey::Size => fs.size(a).cmp(&fs.size(b)),
        SortKey::Type => {
            let a = type_name(a, fs);
            let b = type_name(b, fs);
            compare_names(a, b)
        }
    };
    if descending {
        order.reverse()
    } else {
        order
    }
}

fn has_subdirectories(fs: &dyn FileSystem, node: NodeId) -> bool {
    for index in 0..fs.child_count(node) {
        if let Some(child) = fs.child_at(node, index) {
            if fs.is_dir(child) {
                return true;
            }
        }
    }
    false
}

fn viewer_line_count(bytes: &[u8]) -> usize {
    if core::str::from_utf8(bytes).is_ok() && !bytes.contains(&0) {
        bytes.iter().filter(|byte| **byte == b'\n').count() + 1
    } else {
        (bytes.len() + 15) / 16
    }
}

fn is_text(bytes: &[u8]) -> bool {
    core::str::from_utf8(bytes).is_ok() && !bytes.contains(&0)
}

fn first_enabled(kind: MenuKind, state: &ExplorerState, fs: &dyn FileSystem) -> usize {
    menu_items(kind)
        .iter()
        .position(|entry| {
            !entry.separator && entry.enabled && state.action_enabled(entry.action, fs)
        })
        .unwrap_or(0)
}

fn next_enabled(
    entries: &[MenuItem],
    current: usize,
    delta: isize,
    state: &ExplorerState,
    fs: &dyn FileSystem,
) -> usize {
    if entries.is_empty() {
        return 0;
    }
    let mut index = current.min(entries.len() - 1) as isize;
    for _ in 0..entries.len() {
        index = if delta < 0 {
            if index <= 0 {
                entries.len() as isize - 1
            } else {
                index - 1
            }
        } else {
            (index + 1) % entries.len() as isize
        };
        let entry = entries[index as usize];
        if !entry.separator && entry.enabled && state.action_enabled(entry.action, fs) {
            return index as usize;
        }
    }
    current
}

fn context_move(state: &ExplorerState, mut context: ContextMenu, delta: isize) -> ContextMenu {
    let entries = if context.background {
        CONTEXT_BACKGROUND
    } else {
        CONTEXT_ITEM
    };
    let mut index = if context.hover >= entries.len() {
        if delta < 0 {
            entries.len() as isize - 1
        } else {
            0
        }
    } else {
        context.hover as isize
    };
    for _ in 0..entries.len() {
        index = if delta < 0 {
            if index <= 0 {
                entries.len() as isize - 1
            } else {
                index - 1
            }
        } else {
            (index + 1) % entries.len() as isize
        };
        let entry = entries[index as usize];
        if !entry.separator && entry.enabled {
            context.hover = index as usize;
            break;
        }
    }
    let _ = state;
    context
}

#[derive(Clone, Copy)]
struct GridMetrics {
    columns: usize,
    visible_rows: usize,
    rows: usize,
    cell_width: i32,
    cell_height: i32,
}

#[derive(Clone, Copy)]
struct Layout {
    window: Rect,
    inner: Rect,
    title: Rect,
    menu_bar: Rect,
    toolbar: Rect,
    address: Rect,
    body: Rect,
    sidebar: Option<Rect>,
    tree: Rect,
    main: Rect,
    preview: Option<Rect>,
    header: Option<Rect>,
    list: Rect,
    status: Rect,
}

impl Layout {
    fn compute(screen: Rect, state: &ExplorerState) -> Self {
        let window = if state.maximized {
            screen
        } else {
            Rect::new(
                state.window_x,
                state.window_y,
                state.window_w.max(720) as u32,
                state.window_h.max(480) as u32,
            )
        };
        let inner = if state.maximized {
            window
        } else {
            window.inset(3)
        };
        let (title, rest) = inner.split_top(TITLE_HEIGHT);
        let (menu_bar, rest) = rest.split_top(MENU_BAR_HEIGHT);
        let (toolbar, rest) = if state.toolbar_open {
            rest.split_top(TOOLBAR_HEIGHT)
        } else {
            (Rect::new(rest.x, rest.y, rest.width, 0), rest)
        };
        let (address, rest) = if state.address_open {
            rest.split_top(ADDRESS_HEIGHT)
        } else {
            (Rect::new(rest.x, rest.y, rest.width, 0), rest)
        };
        let (status, rest) = if state.status_open {
            rest.split_bottom(STATUS_HEIGHT)
        } else {
            (Rect::new(rest.x, rest.bottom_i32(), rest.width, 0), rest)
        };
        let body = rest.inset_each(2, 0, 2, 2);
        let (sidebar, main) = if state.bar != ExplorerBar::None && body.width as i32 > 560 {
            let (side, rest) = body.split_left(SIDEBAR_WIDTH);
            (Some(side), rest)
        } else {
            (None, body)
        };
        let (main, preview) = if state.preview_open && main.width as i32 > 520 {
            let (preview, rest) = main.split_right(PREVIEW_WIDTH);
            (rest.inset_each(0, 0, 3, 0), Some(preview))
        } else {
            (main, None)
        };
        let (header, list) = if state.view == ViewMode::Details {
            let (header, list) = main.split_top(HEADER_HEIGHT);
            (Some(header), list)
        } else {
            (None, main)
        };
        let tree = if let Some(sidebar) = sidebar {
            if state.bar == ExplorerBar::Folders {
                Rect::new(
                    sidebar.x + 5,
                    sidebar.y + 25,
                    sidebar.width.saturating_sub(10),
                    ((sidebar.height as i32 - 25 - 116).max(40)) as u32,
                )
            } else {
                Rect::new(0, 0, 0, 0)
            }
        } else {
            Rect::new(0, 0, 0, 0)
        };
        Self {
            window,
            inner,
            title,
            menu_bar,
            toolbar,
            address,
            body,
            sidebar,
            tree,
            main,
            preview,
            header,
            list,
            status,
        }
    }
}

#[derive(Clone, Copy)]
struct DialogLayout {
    window: Rect,
    inner: Rect,
    title: Rect,
    close: Rect,
    body: Rect,
    ok: Rect,
}

impl DialogLayout {
    fn compute(screen: Rect, dialog: Dialog) -> Self {
        let (width, height) = match dialog {
            Dialog::None => (1, 1),
            Dialog::About => (520, 286),
            Dialog::Options => (520, 330),
            Dialog::Properties(_) => (570, 380),
            Dialog::Viewer(_) => (760, 570),
            Dialog::Notice => (480, 232),
        };
        let width = width.min(screen.width as i32 - 30).max(320);
        let height = height.min(screen.height as i32 - 30).max(200);
        let window = Rect::new(
            screen.x + (screen.width as i32 - width) / 2,
            screen.y + (screen.height as i32 - height) / 2,
            width as u32,
            height as u32,
        );
        let inner = window.inset(3);
        let (title, rest) = inner.split_top(TITLE_HEIGHT);
        let body = rest.inset_each(10, 8, 10, 40);
        let close = Rect::new(title.right_i32() - 24, title.y + 4, 20, 20);
        let ok = Rect::new(window.right_i32() - 94, window.bottom_i32() - 32, 80, 24);
        Self {
            window,
            inner,
            title,
            close,
            body,
            ok,
        }
    }
}

fn empty_rect() -> Rect {
    Rect::new(0, 0, 0, 0)
}

fn menu_label_rect(layout: Layout, kind: MenuKind) -> Rect {
    let mut x = layout.menu_bar.x + 4;
    for index in 0..kind.index() {
        x += MENU_WIDTHS[index];
    }
    Rect::new(
        x,
        layout.menu_bar.y,
        MENU_WIDTHS[kind.index()] as u32,
        layout.menu_bar.height,
    )
}

fn menu_height(kind: MenuKind) -> i32 {
    let mut height = 6;
    for entry in menu_items(kind) {
        height += if entry.separator { 7 } else { 21 };
    }
    height
}

fn menu_rect(state: &ExplorerState, kind: MenuKind) -> Rect {
    let layout = Layout::compute(state.screen, state);
    let label = menu_label_rect(layout, kind);
    let width = 260i32.min(state.screen.width as i32 - 8);
    let x = label.x.min(state.screen.right_i32() - width - 3);
    Rect::new(
        x.max(2),
        label.bottom_i32(),
        width as u32,
        menu_height(kind) as u32,
    )
}

fn menu_item_at(state: &ExplorerState, kind: MenuKind, point: Point) -> Option<usize> {
    let rect = menu_rect(state, kind);
    if !rect.contains(point) {
        return None;
    }
    let mut y = rect.y + 3;
    for (index, entry) in menu_items(kind).iter().enumerate() {
        let height = if entry.separator { 7 } else { 21 };
        if point.y >= y && point.y < y + height {
            return Some(index);
        }
        y += height;
    }
    None
}

fn context_entries(context: ContextMenu) -> &'static [MenuItem] {
    if context.background {
        CONTEXT_BACKGROUND
    } else {
        CONTEXT_ITEM
    }
}

fn context_height(context: ContextMenu) -> i32 {
    let mut height = 6;
    for entry in context_entries(context) {
        height += if entry.separator { 7 } else { 21 };
    }
    height
}

fn context_rect(state: &ExplorerState, context: ContextMenu) -> Rect {
    let width = 238i32.min(state.screen.width as i32 - 8);
    let height = context_height(context);
    let x = context.x.min(state.screen.right_i32() - width - 2).max(1);
    let y = context.y.min(state.screen.bottom_i32() - height - 2).max(1);
    Rect::new(x, y, width as u32, height as u32)
}

fn context_item_at(state: &ExplorerState, context: ContextMenu, point: Point) -> Option<usize> {
    let rect = context_rect(state, context);
    if !rect.contains(point) {
        return None;
    }
    let mut y = rect.y + 3;
    for (index, entry) in context_entries(context).iter().enumerate() {
        let height = if entry.separator { 7 } else { 21 };
        if point.y >= y && point.y < y + height {
            return Some(index);
        }
        y += height;
    }
    None
}

fn title_button_rect(layout: Layout, index: usize) -> Rect {
    let offset = match index {
        0 => 46,
        1 => 25,
        _ => 4,
    };
    Rect::new(
        layout.title.right_i32() - offset - 18,
        layout.title.y + 4,
        18,
        20,
    )
}

fn title_button_at(layout: Layout, point: Point) -> Option<usize> {
    for index in 0..3 {
        if title_button_rect(layout, index).contains(point) {
            return Some(index);
        }
    }
    None
}

/// Thickness of the invisible resize border around the explorer window.
const RESIZE_BORDER: i32 = 5;
/// Size of the invisible resize corners around the explorer window.
const RESIZE_CORNER: i32 = 14;

fn resize_edge_at(window: Rect, point: Point) -> Option<ResizeEdge> {
    if !window.contains(point) {
        return None;
    }
    if window.width <= (RESIZE_CORNER * 2) as u32 || window.height <= (RESIZE_CORNER * 2) as u32 {
        return None;
    }

    let left = point.x < window.x + RESIZE_CORNER;
    let right = point.x >= window.right_i32() - RESIZE_CORNER;
    let top = point.y < window.y + RESIZE_CORNER;
    let bottom = point.y >= window.bottom_i32() - RESIZE_CORNER;

    if left && top {
        Some(ResizeEdge::TopLeft)
    } else if right && top {
        Some(ResizeEdge::TopRight)
    } else if left && bottom {
        Some(ResizeEdge::BottomLeft)
    } else if right && bottom {
        Some(ResizeEdge::BottomRight)
    } else if point.x < window.x + RESIZE_BORDER {
        Some(ResizeEdge::Left)
    } else if point.x >= window.right_i32() - RESIZE_BORDER {
        Some(ResizeEdge::Right)
    } else if point.y < window.y + RESIZE_BORDER {
        Some(ResizeEdge::Top)
    } else if point.y >= window.bottom_i32() - RESIZE_BORDER {
        Some(ResizeEdge::Bottom)
    } else {
        None
    }
}

fn resize_cursor(edge: ResizeEdge) -> feos_gui::CursorShape {
    match edge {
        ResizeEdge::Left | ResizeEdge::Right => feos_gui::CursorShape::SizeHorizontal,
        ResizeEdge::Top | ResizeEdge::Bottom => feos_gui::CursorShape::SizeVertical,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => feos_gui::CursorShape::SizeNwse,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => feos_gui::CursorShape::SizeNesw,
    }
}

fn toolbar_button_rect(layout: Layout, index: usize) -> Rect {
    let mut x = layout.toolbar.x + 4;
    for current in 0..index {
        x += TOOLBAR_WIDTHS[current];
        if current == 2 {
            x += 8;
        }
    }
    Rect::new(
        x,
        layout.toolbar.y + 3,
        TOOLBAR_WIDTHS[index] as u32,
        (layout.toolbar.height as i32 - 6).max(1) as u32,
    )
}

fn toolbar_button_at(layout: Layout, point: Point) -> Option<usize> {
    if !layout.toolbar.contains(point) {
        return None;
    }
    for index in 0..6 {
        if toolbar_button_rect(layout, index).contains(point) {
            return Some(index);
        }
    }
    None
}

fn address_field_rect(layout: Layout) -> Rect {
    Rect::new(
        layout.address.x + 56,
        layout.address.y + 4,
        (layout.address.width as i32 - 56 - 42 - 19 - 5).max(40) as u32,
        22,
    )
}

fn address_arrow_rect(layout: Layout) -> Rect {
    let field = address_field_rect(layout);
    Rect::new(field.right_i32(), field.y, 18, field.height)
}

fn go_button_rect(layout: Layout) -> Rect {
    Rect::new(
        layout.address.right_i32() - 43,
        layout.address.y + 4,
        37,
        22,
    )
}

fn address_drop_rect(state: &ExplorerState, layout: Layout) -> Rect {
    let field = address_field_rect(layout);
    let count = state.history_len.min(8) as i32;
    Rect::new(
        field.x,
        field.bottom_i32() + 2,
        field.width,
        (count * 18 + 4).max(22) as u32,
    )
}

fn address_history_at(state: &ExplorerState, layout: Layout, point: Point) -> Option<usize> {
    let rect = address_drop_rect(state, layout);
    if !rect.contains(point) {
        return None;
    }
    let index = ((point.y - rect.y - 2) / 18).max(0) as usize;
    if index < state.history_len.min(8) {
        Some(index)
    } else {
        None
    }
}

fn task_link_rect(layout: Layout, index: usize) -> Rect {
    let sidebar = layout.sidebar.unwrap_or(empty_rect());
    let panel_y = sidebar.bottom_i32() - 112;
    Rect::new(
        sidebar.x + 11,
        panel_y + 25 + index as i32 * 20,
        sidebar.width.saturating_sub(22),
        18,
    )
}

fn search_field_rect(sidebar: Rect) -> Rect {
    Rect::new(
        sidebar.x + 8,
        sidebar.y + 54,
        sidebar.width.saturating_sub(16),
        22,
    )
}

fn search_check_rect(sidebar: Rect) -> Rect {
    Rect::new(sidebar.x + 9, sidebar.y + 84, 13, 13)
}

fn search_button_rect(sidebar: Rect) -> Rect {
    Rect::new(sidebar.x + 8, sidebar.y + 106, 82, 23)
}

fn search_clear_rect(sidebar: Rect) -> Rect {
    Rect::new(sidebar.x + 96, sidebar.y + 106, 66, 23)
}

fn option_check_rect(layout: DialogLayout, index: usize) -> Rect {
    Rect::new(
        layout.body.x + 8,
        layout.body.y + 14 + index as i32 * 31,
        14,
        14,
    )
}

fn grid_metrics(state: &ExplorerState, list: Rect, main_width: u32) -> GridMetrics {
    let width = main_width.max(1) as i32;
    let height = list.height as i32;
    let (cell_width, cell_height) = match state.view {
        ViewMode::Details => (width, ROW_HEIGHT),
        ViewMode::Icons => (112, 82),
        ViewMode::Tiles => (218, 52),
        ViewMode::List => (190, ROW_HEIGHT),
    };
    let columns = match state.view {
        ViewMode::Details => 1,
        _ => (width / cell_width).max(1) as usize,
    };
    let visible_rows = (height / cell_height).max(1) as usize;
    let rows = if state.entry_count == 0 {
        0
    } else {
        (state.entry_count + columns - 1) / columns
    };
    GridMetrics {
        columns,
        visible_rows,
        rows,
        cell_width,
        cell_height,
    }
}

fn entry_rect(
    state: &ExplorerState,
    layout: Layout,
    metrics: GridMetrics,
    index: usize,
) -> Option<Rect> {
    if metrics.columns == 0 || index >= state.entry_count {
        return None;
    }
    let row = index / metrics.columns;
    if row < state.scroll || row >= state.scroll + metrics.visible_rows {
        return None;
    }
    let column = index % metrics.columns;
    let x = layout.list.x + column as i32 * metrics.cell_width;
    let y = layout.list.y + (row - state.scroll) as i32 * metrics.cell_height;
    Some(Rect::new(
        x,
        y,
        metrics.cell_width as u32,
        metrics.cell_height as u32,
    ))
}

fn entry_at(state: &ExplorerState, layout: Layout, point: Point) -> Option<usize> {
    if !layout.list.contains(point) {
        return None;
    }
    let metrics = grid_metrics(state, layout.list, layout.main.width);
    if metrics.columns == 0 {
        return None;
    }
    let row = ((point.y - layout.list.y) / metrics.cell_height).max(0) as usize;
    let column = ((point.x - layout.list.x) / metrics.cell_width).max(0) as usize;
    if column >= metrics.columns || row >= metrics.visible_rows {
        return None;
    }
    let index = (state.scroll + row) * metrics.columns + column;
    if index < state.entry_count {
        Some(index)
    } else {
        None
    }
}

/// Runs the interactive XP-style explorer.
pub fn run(fs: &dyn FileSystem, context: &mut Context<'_>) {
    let platform = context.platform;
    let suspend = context.suspend;
    let suspend_area = context.suspend_area;
    let Some(display) = context.display.as_mut() else {
        feos_console::println!("files: no framebuffer; try 'files ls /' instead");
        return;
    };
    let full = Rect::new(0, 0, display.width() as u32, display.height() as u32);
    let view = context.viewport.unwrap_or(full).clipped_to(full);
    if view.is_empty() {
        return;
    }
    let screen = Rect::new(0, 0, view.width, view.height);
    let mut state = ExplorerState::new(fs, screen);
    state.has_pointer = platform.pointer.is_some();
    if let Some(pointer) = platform.pointer {
        let (x, y) = pointer.position();
        state.pointer = Point::new(x as i32 - view.x, y as i32 - view.y);
    }
    let width = view.width as usize;
    let height = view.height as usize;
    let mut back_pixels = vec![0u32; width * height];
    let mut back = Canvas::new(&mut back_pixels, width, height, width);

    let mut input = Input::new();
    let mut keys = SerialKeys::new();
    let mut dirty = true;
    let mut last_notice = false;

    draw_frame(
        display,
        back.as_mut(),
        fs,
        &mut state,
        platform.clock.uptime_ms(),
        view,
    );

    loop {
        let now = platform.clock.uptime_ms();
        if keys.poll(now, &mut input) {
            break;
        }

        let mut pointer_changed = false;
        let mut suspend_requested = false;
        if let Some(pointer) = platform.pointer {
            let mut budget = 32;
            while budget > 0 {
                let Some(mut report) = pointer.poll() else {
                    break;
                };
                if let (Some(cell), Some(area), Some(true)) = (suspend, suspend_area, report.left) {
                    if area.contains(Point::new(report.x as i32, report.y as i32)) {
                        cell.set(true);
                        suspend_requested = true;
                        break;
                    }
                }
                report.x = report.x.saturating_sub(view.x as u16);
                report.y = report.y.saturating_sub(view.y as u16);
                pointer_changed = true;
                state.handle_pointer(fs, report, now);
                budget -= 1;
            }
        }
        if suspend_requested {
            break;
        }

        let input_changed = !input.is_empty();
        if input_changed {
            for event in input.events() {
                match event {
                    Event::KeyDown(key, modifiers) => {
                        if state.handle_key(fs, key, modifiers, now) {
                            state.exit = true;
                        }
                    }
                    Event::Text(ch) => state.handle_text(ch),
                    Event::Resize(size) => {
                        state.set_screen(Rect::new(0, 0, size.width, size.height));
                    }
                    _ => {}
                }
            }
            state.after_input(fs, now);
        } else if pointer_changed {
            state.after_input(fs, now);
        }

        if state.message_until != 0 && now >= state.message_until {
            state.message.clear();
            state.message_until = 0;
            dirty = true;
        }
        let notice_active = state.message_until > now;
        if state.exit {
            break;
        }
        if dirty || input_changed || pointer_changed || notice_active != last_notice {
            draw_frame(display, back.as_mut(), fs, &mut state, now, view);
            dirty = false;
            last_notice = notice_active;
        }

        input.clear_events();
        platform.clock.idle_ms(12);
    }
    feos_console::println!("[files] explorer closed");
}

fn draw_frame(
    front: &mut Canvas<'static>,
    back: Option<&mut Canvas<'_>>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    now: u64,
    view: Rect,
) {
    if let Some(back) = back {
        render(back, fs, state, now);
        front.blit(
            view.x,
            view.y,
            back.width() as u32,
            back.height() as u32,
            back.pixels(),
        );
    } else {
        render(front, fs, state, now);
    }
}

fn render(canvas: &mut Canvas<'_>, fs: &dyn FileSystem, state: &mut ExplorerState, now: u64) {
    state.set_screen(Rect::new(
        0,
        0,
        canvas.width() as u32,
        canvas.height() as u32,
    ));
    let mut painter = Painter::new(canvas);
    draw_backdrop(&mut painter, state);
    let layout = Layout::compute(state.screen, state);
    draw_window(&mut painter, fs, state, layout, now);
    if state.address_dropdown {
        draw_address_dropdown(&mut painter, fs, state, layout);
    }
    if let Some(menu) = state.menu_open {
        draw_menu(&mut painter, fs, state, menu);
    }
    if let Some(context) = state.context {
        draw_context(&mut painter, fs, state, context);
    }
    if state.dialog != Dialog::None {
        draw_dialog(&mut painter, fs, state, now);
    }
    if state.has_pointer {
        let shape = if state.menu_open.is_none()
            && state.context.is_none()
            && state.dialog == Dialog::None
            && !state.maximized
        {
            resize_edge_at(layout.window, state.pointer)
                .map(resize_cursor)
                .unwrap_or(feos_gui::CursorShape::Arrow)
        } else {
            feos_gui::CursorShape::Arrow
        };
        feos_gui::draw_cursor_shape(&mut painter, state.pointer.x, state.pointer.y, shape);
    }
}

fn draw_backdrop(painter: &mut Painter<'_, '_>, state: &ExplorerState) {
    let full = painter.canvas_rect();
    if state.maximized {
        painter.fill_rect(full, FACE);
        return;
    }
    painter.canvas_mut().fill_vertical_gradient(
        full,
        Color::rgb(63, 140, 243),
        Color::rgb(36, 94, 219),
    );
    let mut x = full.x;
    while x < full.right_i32() {
        painter.fill_rect_alpha(
            Rect::new(x, full.y, 1, full.height),
            Color::rgb(84, 156, 246),
            90,
        );
        x += 64;
    }
    let mut y = full.y;
    while y < full.bottom_i32() {
        painter.fill_rect_alpha(
            Rect::new(full.x, y, full.width, 1),
            Color::rgb(84, 156, 246),
            90,
        );
        y += 64;
    }
}

fn draw_window(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
    now: u64,
) {
    if !state.maximized {
        painter.shadow(layout.window, 6, 10, 72);
    }
    painter.fill_rect(layout.window, TITLE_BOTTOM);
    painter.fill_rect(layout.inner, FACE);
    painter
        .canvas_mut()
        .fill_vertical_gradient(layout.title, TITLE_TOP, TITLE_BOTTOM);

    draw_folder_icon(
        painter,
        Rect::new(layout.title.x + 7, layout.title.y + 6, 16, 16),
    );
    let mut title = Text::<96>::new();
    if state.current == fs.root() {
        let _ = write!(title, "File Explorer - My Computer");
    } else {
        let _ = write!(
            title,
            "{} - File Explorer",
            fs.name(state.current).unwrap_or("/")
        );
    }
    let buttons_left = title_button_rect(layout, 0).x - 6;
    painter.draw_text_ellipsis(
        Rect::new(
            layout.title.x + 30,
            layout.title.y,
            (buttons_left - layout.title.x - 34).max(10) as u32,
            layout.title.height,
        ),
        title.as_str(),
        Color::WHITE,
        2,
        Align::Start,
        VerticalAlign::Middle,
    );
    for index in 0..3 {
        draw_title_button(
            painter,
            title_button_rect(layout, index),
            index,
            state.pointer,
        );
    }

    draw_menu_bar(painter, fs, state, layout);
    if state.toolbar_open {
        draw_toolbar(painter, fs, state, layout);
    }
    if state.address_open {
        draw_address_bar(painter, state, layout);
    }
    draw_body(painter, fs, state, layout, now);
    if state.status_open {
        draw_status_bar(painter, fs, state, layout, now);
    }
    if !state.maximized {
        draw_resize_grip(painter, layout.window);
    }
}

fn draw_title_button(painter: &mut Painter<'_, '_>, rect: Rect, index: usize, pointer: Point) {
    let hovered = rect.contains(pointer);
    let color = if index == 2 {
        if hovered {
            Color::rgb(232, 78, 58)
        } else {
            Color::rgb(214, 58, 40)
        }
    } else if hovered {
        Color::rgb(96, 170, 255)
    } else {
        Color::rgb(62, 122, 220)
    };
    painter.fill_rounded_rect(rect, 3, color);
    painter.stroke_rounded_rect(rect, 3, 1, Color::rgb(255, 255, 255).lerp(color, 90));
    let center_y = rect.y + rect.height as i32 / 2;
    match index {
        0 => {
            painter.fill_rect(Rect::new(rect.x + 5, center_y + 2, 8, 2), Color::WHITE);
        }
        1 => {
            painter.stroke_rect(Rect::new(rect.x + 5, center_y - 4, 8, 6), 1, Color::WHITE);
            painter.fill_rect(Rect::new(rect.x + 5, center_y - 4, 8, 2), Color::WHITE);
        }
        _ => {
            painter.line(
                rect.x + 5,
                center_y - 4,
                rect.x + 12,
                center_y + 3,
                Color::WHITE,
            );
            painter.line(
                rect.x + 12,
                center_y - 4,
                rect.x + 5,
                center_y + 3,
                Color::WHITE,
            );
        }
    }
}

fn draw_resize_grip(painter: &mut Painter<'_, '_>, window: Rect) {
    let x = window.right_i32() - 13;
    let y = window.bottom_i32() - 13;
    for index in 0..3 {
        let offset = index * 4;
        painter.line(
            x + offset,
            y + 9,
            x + 9,
            y + offset,
            Color::rgb(255, 255, 255),
        );
        painter.line(
            x + offset + 1,
            y + 10,
            x + 10,
            y + offset + 1,
            Color::rgb(80, 100, 150),
        );
    }
}

fn draw_menu_bar(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
) {
    painter.fill_rect(layout.menu_bar, FACE);
    painter.fill_rect(
        Rect::new(
            layout.menu_bar.x,
            layout.menu_bar.bottom_i32() - 1,
            layout.menu_bar.width,
            1,
        ),
        BORDER,
    );
    for kind in MenuKind::ALL {
        let rect = menu_label_rect(layout, kind);
        let active = state.menu_open == Some(kind);
        let hovered = rect.contains(state.pointer) && state.has_pointer;
        if active {
            painter.fill_rect(rect.inset_xy(2, 2), SELECT);
        } else if hovered {
            painter.fill_rect(rect.inset_xy(2, 2), SELECT_SOFT);
        }
        painter.draw_text_centered(
            rect,
            MENU_LABELS[kind.index()],
            if active { Color::WHITE } else { TEXT },
            1,
        );
    }
    let _ = fs;
}

fn draw_toolbar(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
) {
    painter
        .canvas_mut()
        .fill_vertical_gradient(layout.toolbar, TOOLBAR_TOP, TOOLBAR_BOTTOM);
    painter.fill_rect(
        Rect::new(
            layout.toolbar.x,
            layout.toolbar.bottom_i32() - 1,
            layout.toolbar.width,
            1,
        ),
        BORDER,
    );
    let labels = ["Back", "Forward", "Up", "Search", "Folders", "Views"];
    let icons = [
        Icon::ArrowLeft,
        Icon::ArrowRight,
        Icon::ArrowUp,
        Icon::Search,
        Icon::Folder,
        Icon::Grid,
    ];
    let enabled = [
        state.action_enabled(Action::Back, fs),
        state.action_enabled(Action::Forward, fs),
        state.action_enabled(Action::Up, fs),
        true,
        true,
        true,
    ];
    for index in 0..6 {
        let rect = toolbar_button_rect(layout, index);
        let hovered = rect.contains(state.pointer) && state.has_pointer && enabled[index];
        let pressed = (index == 3 && state.bar == ExplorerBar::Search)
            || (index == 4 && state.bar == ExplorerBar::Folders);
        if pressed {
            painter.fill_rounded_rect(rect, 3, FACE_DARK);
            painter.stroke_rounded_rect(rect, 3, 1, BORDER);
        } else if hovered {
            painter.fill_rounded_rect(rect, 3, HOVER_LIGHT);
            painter.stroke_rounded_rect(rect, 3, 1, Color::rgb(140, 170, 220));
        }
        let color = if enabled[index] { TEXT } else { DISABLED };
        paint_icon(
            painter,
            Rect::new(rect.x + (rect.width as i32 - 18) / 2, rect.y + 3, 18, 18),
            icons[index],
            color,
        );
        painter.draw_text_ellipsis(
            Rect::new(rect.x + 1, rect.y + 23, rect.width.saturating_sub(2), 14),
            labels[index],
            color,
            1,
            Align::Center,
            VerticalAlign::Middle,
        );
    }
    let separator = toolbar_button_rect(layout, 2).right_i32() + 4;
    painter.fill_rect(
        Rect::new(
            separator,
            layout.toolbar.y + 6,
            1,
            (layout.toolbar.height as i32 - 12).max(0) as u32,
        ),
        BORDER,
    );
}

fn draw_address_bar(painter: &mut Painter<'_, '_>, state: &mut ExplorerState, layout: Layout) {
    painter.fill_rect(layout.address, FACE);
    painter.fill_rect(
        Rect::new(
            layout.address.x,
            layout.address.bottom_i32() - 1,
            layout.address.width,
            1,
        ),
        BORDER,
    );
    painter.draw_text_ellipsis(
        Rect::new(
            layout.address.x + 7,
            layout.address.y,
            58,
            layout.address.height,
        ),
        "Address",
        TEXT,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let field = address_field_rect(layout);
    painter.fill_rect(field, FIELD);
    painter.stroke_rect(
        field,
        1,
        if state.focus == Focus::Address {
            SELECT
        } else {
            FIELD_BORDER
        },
    );
    draw_folder_icon(painter, Rect::new(field.x + 3, field.y + 4, 14, 14));
    let text_rect = Rect::new(
        field.x + 21,
        field.y,
        field.width.saturating_sub(24),
        field.height,
    );
    painter.draw_text_ellipsis(
        text_rect,
        state.address.as_str(),
        TEXT,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    if state.focus == Focus::Address {
        let prefix = &state.address.as_str()[..state.address.cursor().min(state.address.len())];
        let x = text_rect.x + painter.text_width(prefix, 1);
        if x < text_rect.right_i32() - 1 {
            painter.fill_rect(
                Rect::new(
                    x,
                    text_rect.y + 4,
                    1,
                    (text_rect.height as i32 - 8).max(0) as u32,
                ),
                TEXT,
            );
        }
    }
    let arrow = address_arrow_rect(layout);
    painter.fill_rect(arrow, FACE_LIGHT);
    painter.stroke_rect(arrow, 1, FIELD_BORDER);
    let cx = arrow.x + arrow.width as i32 / 2;
    let cy = arrow.y + arrow.height as i32 / 2;
    let points = [
        Point::new(cx - 4, cy - 2),
        Point::new(cx + 4, cy - 2),
        Point::new(cx, cy + 3),
    ];
    painter.fill_polygon(&points, TEXT);

    let go = go_button_rect(layout);
    let hovered = go.contains(state.pointer) && state.has_pointer;
    draw_push_button(painter, go, "Go", hovered, false);
    paint_icon(
        painter,
        Rect::new(go.x + 4, go.y + 5, 12, 12),
        Icon::ArrowRight,
        Color::rgb(22, 120, 42),
    );
}

fn draw_body(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
    now: u64,
) {
    if let Some(sidebar) = layout.sidebar {
        draw_sidebar(painter, fs, state, layout, sidebar, now);
    }
    draw_main(painter, fs, state, layout);
    if let Some(preview) = layout.preview {
        draw_preview(painter, fs, state, preview);
    }
}

fn draw_sidebar(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
    sidebar: Rect,
    now: u64,
) {
    painter.fill_rect(sidebar, PANEL_BLUE);
    painter.stroke_rect(sidebar, 1, Color::rgb(140, 158, 200));
    let header = Rect::new(
        sidebar.x + 1,
        sidebar.y + 1,
        sidebar.width.saturating_sub(2),
        23,
    );
    painter
        .canvas_mut()
        .fill_vertical_gradient(header, HEADER_TOP, HEADER_BOTTOM);
    painter.draw_text_ellipsis(
        Rect::new(
            header.x + 8,
            header.y,
            header.width.saturating_sub(12),
            header.height,
        ),
        match state.bar {
            ExplorerBar::Folders => "Folders",
            ExplorerBar::Search => "Search",
            ExplorerBar::None => "",
        },
        Color::WHITE,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    match state.bar {
        ExplorerBar::Folders => {
            draw_tree(painter, fs, state, layout);
            draw_folder_tasks(painter, fs, state, layout, now);
        }
        ExplorerBar::Search => draw_search_pane(painter, fs, state, sidebar),
        ExplorerBar::None => {}
    }
}

fn draw_tree(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
) {
    if layout.tree.is_empty() {
        return;
    }
    painter.fill_rect(layout.tree, FIELD);
    painter.stroke_rect(layout.tree, 1, Color::rgb(150, 166, 200));
    let visible = (layout.tree.height as i32 / TREE_ROW).max(1) as usize;
    let max = state.tree_len.saturating_sub(visible);
    if state.tree_scroll > max {
        state.tree_scroll = max;
    }
    let clip = painter.push_clip(layout.tree.inset(1));
    for row in 0..visible {
        let index = state.tree_scroll + row;
        if index >= state.tree_len {
            break;
        }
        let node = state.tree[index];
        let depth = state.tree_depth[index] as i32;
        let y = layout.tree.y + 1 + row as i32 * TREE_ROW;
        let row_rect = Rect::new(
            layout.tree.x + 1,
            y,
            layout.tree.width.saturating_sub(2),
            TREE_ROW as u32,
        );
        let current = node == state.current;
        let selected = index == state.tree_cursor && state.focus == Focus::Tree;
        let hovered = row_rect.contains(state.pointer) && state.has_pointer;
        if selected {
            painter.fill_rect(row_rect, SELECT);
        } else if hovered {
            painter.fill_rect(row_rect, HOVER_LIGHT);
        } else if current {
            painter.fill_rect(row_rect, SELECT_SOFT);
        }
        let x = layout.tree.x + 5 + depth * 12;
        if has_subdirectories(fs, node) {
            let cy = y + TREE_ROW / 2;
            let points = if bit(&state.expanded, node.0 as usize) {
                [
                    Point::new(x, cy - 2),
                    Point::new(x + 8, cy - 2),
                    Point::new(x + 4, cy + 3),
                ]
            } else {
                [
                    Point::new(x + 1, cy - 4),
                    Point::new(x + 1, cy + 4),
                    Point::new(x + 6, cy),
                ]
            };
            painter.fill_polygon(&points, if selected { Color::WHITE } else { TEXT });
        }
        let icon = if node == fs.root() {
            Icon::Home
        } else {
            Icon::Folder
        };
        paint_icon(
            painter,
            Rect::new(x + 11, y + 2, 14, 14),
            icon,
            if selected {
                Color::WHITE
            } else {
                Color::rgb(230, 170, 35)
            },
        );
        let label = if node == fs.root() {
            "My Computer"
        } else {
            fs.name(node).unwrap_or("?")
        };
        painter.draw_text_ellipsis(
            Rect::new(
                x + 28,
                y,
                (layout.tree.right_i32() - x - 31).max(0) as u32,
                TREE_ROW as u32,
            ),
            label,
            if selected { Color::WHITE } else { TEXT },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
    painter.restore_clip(clip);
    draw_scrollbar(
        painter,
        layout.tree,
        state.tree_scroll,
        visible,
        state.tree_len,
    );
}

fn draw_folder_tasks(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
    now: u64,
) {
    let Some(sidebar) = layout.sidebar else {
        return;
    };
    let panel_y = sidebar.bottom_i32() - 112;
    painter.fill_rect(
        Rect::new(sidebar.x + 1, panel_y, sidebar.width.saturating_sub(2), 111),
        FACE_LIGHT,
    );
    painter.fill_rect(
        Rect::new(sidebar.x + 1, panel_y, sidebar.width.saturating_sub(2), 19),
        Color::rgb(236, 240, 250),
    );
    painter.draw_text_ellipsis(
        Rect::new(sidebar.x + 9, panel_y, sidebar.width.saturating_sub(14), 19),
        "File and Folder Tasks",
        Color::rgb(20, 60, 160),
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let links = ["Open", "Search", "Properties", "Refresh"];
    for (index, label) in links.iter().enumerate() {
        let rect = task_link_rect(layout, index);
        let hovered = rect.contains(state.pointer) && state.has_pointer;
        painter.draw_text_ellipsis(
            rect,
            label,
            if hovered { Color::rgb(0, 0, 255) } else { LINK },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        if hovered {
            painter.fill_rect(
                Rect::new(
                    rect.x,
                    rect.bottom_i32() - 1,
                    painter.text_width(label, 1) as u32,
                    1,
                ),
                Color::rgb(0, 0, 255),
            );
        }
    }
    let _ = (fs, now);
}

fn draw_search_pane(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    sidebar: Rect,
) {
    painter.draw_text_ellipsis(
        Rect::new(
            sidebar.x + 8,
            sidebar.y + 30,
            sidebar.width.saturating_sub(16),
            20,
        ),
        "Find files or folders:",
        TEXT,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    draw_text_field(
        painter,
        search_field_rect(sidebar),
        state.search.as_str(),
        state.search.cursor(),
        state.focus == Focus::Search,
    );
    let check = search_check_rect(sidebar);
    draw_checkbox(painter, check, state.include_subfolders, false);
    painter.draw_text_ellipsis(
        Rect::new(
            check.right_i32() + 5,
            check.y - 2,
            sidebar.width.saturating_sub(50),
            18,
        ),
        "Include subfolders",
        TEXT,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let search = search_button_rect(sidebar);
    let clear = search_clear_rect(sidebar);
    draw_push_button(
        painter,
        search,
        "Search",
        search.contains(state.pointer) && state.has_pointer,
        true,
    );
    draw_push_button(
        painter,
        clear,
        "Clear",
        clear.contains(state.pointer) && state.has_pointer,
        false,
    );
    painter.fill_rect(
        Rect::new(
            sidebar.x + 8,
            sidebar.y + 138,
            sidebar.width.saturating_sub(16),
            1,
        ),
        BORDER,
    );
    let mut summary = Text::<96>::new();
    match state.search_count {
        0 if state.search.is_empty() => {
            let _ = write!(summary, "Type a name to search this folder.");
        }
        0 => {
            let _ = write!(summary, "No items matched.");
        }
        1 => {
            let _ = write!(summary, "1 item found.");
        }
        count => {
            let _ = write!(summary, "{} items found.", count);
        }
    }
    painter.draw_text_ellipsis(
        Rect::new(
            sidebar.x + 8,
            sidebar.y + 146,
            sidebar.width.saturating_sub(16),
            20,
        ),
        summary.as_str(),
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Top,
    );
    painter.draw_text_ellipsis(
        Rect::new(
            sidebar.x + 8,
            sidebar.y + 168,
            sidebar.width.saturating_sub(16),
            20,
        ),
        "Scope:",
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Top,
    );
    let mut path = Text::<PATH_MAX>::new();
    let mut buffer = [0u8; PATH_MAX];
    path.set(fs.path(state.current, &mut buffer).unwrap_or("/"));
    painter.draw_text_ellipsis(
        Rect::new(
            sidebar.x + 8,
            sidebar.y + 184,
            sidebar.width.saturating_sub(16),
            20,
        ),
        path.as_str(),
        TEXT,
        1,
        Align::Start,
        VerticalAlign::Top,
    );

    // The search pane keeps a short command reference below the results.
    if sidebar.height > 280 {
        let y = sidebar.bottom_i32() - 76;
        painter.fill_rect(
            Rect::new(sidebar.x + 8, y, sidebar.width.saturating_sub(16), 1),
            BORDER,
        );
        painter.draw_text_ellipsis(
            Rect::new(sidebar.x + 8, y + 6, sidebar.width.saturating_sub(16), 18),
            "F3 search  F5 refresh",
            MUTED,
            1,
            Align::Start,
            VerticalAlign::Top,
        );
        painter.draw_text_ellipsis(
            Rect::new(sidebar.x + 8, y + 24, sidebar.width.saturating_sub(16), 18),
            "Enter opens  Esc returns",
            MUTED,
            1,
            Align::Start,
            VerticalAlign::Top,
        );
    }
}

fn draw_main(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
) {
    if layout.main.is_empty() {
        return;
    }
    painter.fill_rect(layout.main, FIELD);
    painter.stroke_rect(layout.main, 1, Color::rgb(150, 166, 200));
    if state.entry_count == 0 {
        let message = if state.searching() {
            "No files or folders match your search."
        } else {
            "This folder is empty."
        };
        painter.draw_text_centered(
            Rect::new(
                layout.list.x,
                layout.list.y + layout.list.height as i32 / 3,
                layout.list.width,
                30,
            ),
            message,
            MUTED,
            1,
        );
        return;
    }
    match state.view {
        ViewMode::Details => draw_details(painter, fs, state, layout),
        ViewMode::Icons => draw_icons(painter, fs, state, layout),
        ViewMode::List => draw_list(painter, fs, state, layout),
        ViewMode::Tiles => draw_tiles(painter, fs, state, layout),
    }
}

fn draw_details(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
) {
    let Some(header) = layout.header else {
        return;
    };
    painter.fill_rect(header, FACE);
    painter.fill_rect(
        Rect::new(header.x, header.bottom_i32() - 1, header.width, 1),
        BORDER,
    );
    let width = header.width as i32;
    let name_w = width * 45 / 100;
    let size_w = width * 15 / 100;
    let type_w = width * 24 / 100;
    let columns = [
        (header.x, name_w, "Name", Some(SortKey::Name)),
        (header.x + name_w, size_w, "Size", Some(SortKey::Size)),
        (
            header.x + name_w + size_w,
            type_w,
            "Type",
            Some(SortKey::Type),
        ),
        (
            header.x + name_w + size_w + type_w,
            width - name_w - size_w - type_w,
            "Modified",
            None,
        ),
    ];
    for (x, w, label, key) in columns {
        let rect = Rect::new(x, header.y, w.max(1) as u32, header.height);
        let active = key.map(|key| state.sort == key).unwrap_or(false);
        if active {
            painter.fill_rect(rect, Color::rgb(226, 231, 242));
        }
        painter.draw_text_ellipsis(
            Rect::new(
                rect.x + 4,
                rect.y,
                rect.width.saturating_sub(12),
                rect.height,
            ),
            label,
            TEXT,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        if active {
            let cx = rect.right_i32() - 8;
            let cy = rect.y + rect.height as i32 / 2;
            let points = if state.sort_desc {
                [
                    Point::new(cx - 3, cy - 2),
                    Point::new(cx + 3, cy - 2),
                    Point::new(cx, cy + 2),
                ]
            } else {
                [
                    Point::new(cx - 3, cy + 2),
                    Point::new(cx + 3, cy + 2),
                    Point::new(cx, cy - 2),
                ]
            };
            painter.fill_polygon(&points, TEXT);
        }
        painter.fill_rect(
            Rect::new(
                rect.right_i32() - 1,
                rect.y + 2,
                1,
                (rect.height as i32 - 4).max(0) as u32,
            ),
            BORDER,
        );
    }

    let metrics = grid_metrics(state, layout.list, layout.main.width);
    let clip = painter.push_clip(layout.list);
    for index in state.scroll..state.entry_count.min(state.scroll + metrics.visible_rows) {
        draw_detail_row(
            painter, fs, state, layout, metrics, index, name_w, size_w, type_w,
        );
    }
    painter.restore_clip(clip);
    draw_scrollbar(
        painter,
        layout.list,
        state.scroll,
        metrics.visible_rows,
        metrics.rows,
    );
}

fn draw_detail_row(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &ExplorerState,
    layout: Layout,
    metrics: GridMetrics,
    index: usize,
    name_w: i32,
    size_w: i32,
    type_w: i32,
) {
    let Some(row) = entry_rect(state, layout, metrics, index) else {
        return;
    };
    let node = state.entries[index];
    let selected = state.is_selected(node);
    let hovered = row.contains(state.pointer) && state.has_pointer;
    if selected {
        painter.fill_rect(row, SELECT);
    } else if hovered {
        painter.fill_rect(row, HOVER_LIGHT);
    }
    let fg = if selected { Color::WHITE } else { TEXT };
    draw_node_icon(painter, node, fs, Rect::new(row.x + 4, row.y + 2, 16, 16));
    painter.draw_text_ellipsis(
        Rect::new(row.x + 25, row.y, (name_w - 28).max(10) as u32, row.height),
        fs.name(node).unwrap_or("?"),
        fg,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let size = if fs.is_dir(node) {
        Text::<16>::from_str("")
    } else {
        format_size(fs.size(node))
    };
    painter.draw_text_ellipsis(
        Rect::new(
            row.x + name_w,
            row.y,
            (size_w - 8).max(10) as u32,
            row.height,
        ),
        size.as_str(),
        if selected { Color::WHITE } else { MUTED },
        1,
        Align::End,
        VerticalAlign::Middle,
    );
    painter.draw_text_ellipsis(
        Rect::new(
            row.x + name_w + size_w,
            row.y,
            (type_w - 8).max(10) as u32,
            row.height,
        ),
        type_name(node, fs),
        if selected { Color::WHITE } else { TEXT },
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    painter.draw_text_ellipsis(
        Rect::new(
            row.x + name_w + size_w + type_w + 4,
            row.y,
            row.width
                .saturating_sub((name_w + size_w + type_w + 8) as u32),
            row.height,
        ),
        "-",
        if selected { Color::WHITE } else { MUTED },
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
}

fn draw_icons(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
) {
    let metrics = grid_metrics(state, layout.list, layout.main.width);
    let clip = painter.push_clip(layout.list);
    let first = state.scroll * metrics.columns;
    let last = ((state.scroll + metrics.visible_rows) * metrics.columns).min(state.entry_count);
    for index in first..last {
        let Some(cell) = entry_rect(state, layout, metrics, index) else {
            continue;
        };
        let node = state.entries[index];
        let selected = state.is_selected(node);
        let hovered = cell.contains(state.pointer) && state.has_pointer;
        if hovered && !selected {
            painter.fill_rect(cell.inset(2), HOVER_LIGHT);
        }
        draw_node_icon(
            painter,
            node,
            fs,
            Rect::new(cell.x + (cell.width as i32 - 40) / 2, cell.y + 5, 40, 36),
        );
        let label = Rect::new(cell.x + 3, cell.y + 46, cell.width.saturating_sub(6), 17);
        if selected {
            painter.fill_rect(
                Rect::new(
                    label.x + 4,
                    label.y,
                    label.width.saturating_sub(8),
                    label.height,
                ),
                SELECT,
            );
        }
        painter.draw_text_ellipsis(
            label,
            fs.name(node).unwrap_or("?"),
            if selected { Color::WHITE } else { TEXT },
            1,
            Align::Center,
            VerticalAlign::Middle,
        );
        let detail = if fs.is_dir(node) {
            Text::<16>::from_str("File Folder")
        } else {
            format_size(fs.size(node))
        };
        painter.draw_text_ellipsis(
            Rect::new(cell.x + 3, cell.y + 64, cell.width.saturating_sub(6), 14),
            detail.as_str(),
            if selected { Color::WHITE } else { MUTED },
            1,
            Align::Center,
            VerticalAlign::Middle,
        );
    }
    painter.restore_clip(clip);
    draw_scrollbar(
        painter,
        layout.list,
        state.scroll,
        metrics.visible_rows,
        metrics.rows,
    );
}

fn draw_list(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
) {
    let metrics = grid_metrics(state, layout.list, layout.main.width);
    let clip = painter.push_clip(layout.list);
    let first = state.scroll * metrics.columns;
    let last = ((state.scroll + metrics.visible_rows) * metrics.columns).min(state.entry_count);
    for index in first..last {
        let Some(cell) = entry_rect(state, layout, metrics, index) else {
            continue;
        };
        let node = state.entries[index];
        let selected = state.is_selected(node);
        let hovered = cell.contains(state.pointer) && state.has_pointer;
        if selected {
            painter.fill_rect(cell, SELECT);
        } else if hovered {
            painter.fill_rect(cell, HOVER_LIGHT);
        }
        draw_node_icon(painter, node, fs, Rect::new(cell.x + 3, cell.y + 2, 16, 16));
        painter.draw_text_ellipsis(
            Rect::new(
                cell.x + 24,
                cell.y,
                cell.width.saturating_sub(27),
                cell.height,
            ),
            fs.name(node).unwrap_or("?"),
            if selected { Color::WHITE } else { TEXT },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
    painter.restore_clip(clip);
    draw_scrollbar(
        painter,
        layout.list,
        state.scroll,
        metrics.visible_rows,
        metrics.rows,
    );
}

fn draw_tiles(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    layout: Layout,
) {
    let metrics = grid_metrics(state, layout.list, layout.main.width);
    let clip = painter.push_clip(layout.list);
    let first = state.scroll * metrics.columns;
    let last = ((state.scroll + metrics.visible_rows) * metrics.columns).min(state.entry_count);
    for index in first..last {
        let Some(cell) = entry_rect(state, layout, metrics, index) else {
            continue;
        };
        let node = state.entries[index];
        let selected = state.is_selected(node);
        let hovered = cell.contains(state.pointer) && state.has_pointer;
        if selected {
            painter.fill_rect(cell.inset(1), SELECT);
        } else if hovered {
            painter.fill_rect(cell.inset(1), HOVER_LIGHT);
        }
        draw_node_icon(painter, node, fs, Rect::new(cell.x + 5, cell.y + 8, 30, 30));
        painter.draw_text_ellipsis(
            Rect::new(cell.x + 42, cell.y + 6, cell.width.saturating_sub(46), 18),
            fs.name(node).unwrap_or("?"),
            if selected { Color::WHITE } else { TEXT },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        painter.draw_text_ellipsis(
            Rect::new(cell.x + 42, cell.y + 26, cell.width.saturating_sub(46), 16),
            type_name(node, fs),
            if selected { Color::WHITE } else { MUTED },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        let detail = if fs.is_dir(node) {
            Text::<16>::from_str("Folder")
        } else {
            format_size(fs.size(node))
        };
        painter.draw_text_ellipsis(
            Rect::new(cell.x + 42, cell.y + 44, cell.width.saturating_sub(46), 14),
            detail.as_str(),
            if selected { Color::WHITE } else { MUTED },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
    painter.restore_clip(clip);
    draw_scrollbar(
        painter,
        layout.list,
        state.scroll,
        metrics.visible_rows,
        metrics.rows,
    );
}

fn draw_preview(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    rect: Rect,
) {
    painter.fill_rect(rect, FACE_LIGHT);
    painter.stroke_rect(rect, 1, Color::rgb(150, 166, 200));
    let header = Rect::new(rect.x + 1, rect.y + 1, rect.width.saturating_sub(2), 23);
    painter
        .canvas_mut()
        .fill_vertical_gradient(header, HEADER_TOP, HEADER_BOTTOM);
    painter.draw_text_ellipsis(
        Rect::new(
            header.x + 8,
            header.y,
            header.width.saturating_sub(12),
            header.height,
        ),
        "Details",
        Color::WHITE,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let body = Rect::new(
        rect.x + 1,
        header.bottom_i32(),
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(24),
    );
    painter.fill_rect(body, FIELD);
    let node = state.preview_node.unwrap_or(state.current);
    draw_node_icon(painter, node, fs, Rect::new(body.x + 8, body.y + 8, 32, 30));
    painter.draw_text_ellipsis(
        Rect::new(body.x + 48, body.y + 7, body.width.saturating_sub(54), 18),
        fs.name(node).unwrap_or("My Computer"),
        TEXT,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    painter.draw_text_ellipsis(
        Rect::new(body.x + 48, body.y + 24, body.width.saturating_sub(54), 16),
        type_name(node, fs),
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let mut meta = Text::<48>::new();
    if fs.is_dir(node) {
        let _ = write!(meta, "{} item(s)", fs.child_count(node));
    } else {
        let _ = write!(meta, "{}", format_size(fs.size(node)));
    }
    painter.draw_text_ellipsis(
        Rect::new(body.x + 48, body.y + 39, body.width.saturating_sub(54), 15),
        meta.as_str(),
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let mut path = Text::<PATH_MAX>::new();
    let mut buffer = [0u8; PATH_MAX];
    path.set(fs.path(node, &mut buffer).unwrap_or("/"));
    painter.draw_text_ellipsis(
        Rect::new(body.x + 8, body.y + 68, body.width.saturating_sub(16), 16),
        path.as_str(),
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    painter.fill_rect(
        Rect::new(body.x + 7, body.y + 86, body.width.saturating_sub(14), 1),
        BORDER,
    );
    let content = Rect::new(
        body.x + 8,
        body.y + 92,
        body.width.saturating_sub(16),
        body.height.saturating_sub(94),
    );
    if content.height > 8 {
        draw_preview_content(painter, fs, state, node, content);
    }
}

fn draw_preview_content(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    node: NodeId,
    rect: Rect,
) {
    let clip = painter.push_clip(rect);
    if fs.is_dir(node) {
        let count = fs.child_count(node).min(24);
        let mut y = rect.y;
        for index in 0..count {
            let Some(child) = fs.child_at(node, index) else {
                continue;
            };
            if y + 16 > rect.bottom_i32() {
                break;
            }
            draw_node_icon(painter, child, fs, Rect::new(rect.x, y + 1, 12, 12));
            painter.draw_text_ellipsis(
                Rect::new(rect.x + 17, y, rect.width.saturating_sub(17), 15),
                fs.name(child).unwrap_or("?"),
                TEXT,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
            y += 16;
        }
    } else {
        let bytes = &state.preview[..state.preview_len];
        if is_text(bytes) {
            let text = core::str::from_utf8(bytes).unwrap_or("");
            draw_scrolled_text(painter, rect, text, state.preview_scroll, TEXT);
        } else {
            draw_hex_dump(painter, rect, bytes, state.preview_scroll);
        }
        if state.preview_len < fs.size(node) {
            painter.draw_text_ellipsis(
                Rect::new(rect.x, rect.bottom_i32() - 12, rect.width, 12),
                "[preview truncated]",
                Color::rgb(170, 80, 30),
                1,
                Align::Start,
                VerticalAlign::Top,
            );
        }
    }
    painter.restore_clip(clip);
}

fn draw_status_bar(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &ExplorerState,
    layout: Layout,
    now: u64,
) {
    let rect = layout.status;
    painter.fill_rect(rect, FACE);
    painter.fill_rect(Rect::new(rect.x, rect.y, rect.width, 1), BORDER);
    let first_width = rect.width as i32 / 2;
    let second_width = rect.width as i32 * 3 / 4;
    painter.stroke_rect(
        Rect::new(
            rect.x + 3,
            rect.y + 3,
            (first_width - 6).max(1) as u32,
            rect.height.saturating_sub(6),
        ),
        1,
        Color::rgb(210, 205, 190),
    );
    painter.stroke_rect(
        Rect::new(
            rect.x + first_width + 1,
            rect.y + 3,
            (second_width - first_width - 4).max(1) as u32,
            rect.height.saturating_sub(6),
        ),
        1,
        Color::rgb(210, 205, 190),
    );
    let mut left = Text::<160>::new();
    if state.message_until > now && !state.message.is_empty() {
        left.set(state.message.as_str());
    } else if state.searching() {
        let _ = write!(left, "{} item(s) found", state.entry_count);
    } else {
        let _ = write!(left, "{} object(s)", state.entry_count);
    }
    if state.selected_count() > 0 && !(state.message_until > now) {
        let _ = write!(left, "    {} selected", state.selected_count());
    }
    painter.draw_text_ellipsis(
        Rect::new(
            rect.x + 8,
            rect.y,
            (first_width - 12).max(10) as u32,
            rect.height,
        ),
        left.as_str(),
        TEXT,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let mut middle = Text::<96>::new();
    if let Some(node) = state.focused_node() {
        if fs.is_file(node) {
            let _ = write!(middle, "{}", format_size(fs.size(node)));
        } else {
            let _ = write!(middle, "{}", type_name(node, fs));
        }
    }
    if state.selected_count() > 1 {
        let _ = write!(middle, "   {}", format_size(state.selected_bytes(fs)));
    }
    painter.draw_text_ellipsis(
        Rect::new(
            rect.x + first_width + 6,
            rect.y,
            (second_width - first_width - 10).max(10) as u32,
            rect.height,
        ),
        middle.as_str(),
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let right = Rect::new(
        rect.x + second_width + 4,
        rect.y,
        (rect.width as i32 - second_width - 8).max(10) as u32,
        rect.height,
    );
    paint_icon(
        painter,
        Rect::new(right.x + 2, right.y + 4, 14, 14),
        Icon::Home,
        Color::rgb(80, 110, 170),
    );
    painter.draw_text_ellipsis(
        Rect::new(
            right.x + 19,
            right.y,
            right.width.saturating_sub(21),
            right.height,
        ),
        "My Computer",
        TEXT,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
}

fn draw_scrollbar(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    offset: usize,
    visible: usize,
    total: usize,
) {
    if total <= visible || rect.width < 10 {
        return;
    }
    let track = Rect::new(
        rect.right_i32() - 8,
        rect.y + 2,
        7,
        rect.height.saturating_sub(4),
    );
    painter.fill_rect(track, Color::rgb(241, 239, 230));
    painter.stroke_rect(track, 1, Color::rgb(200, 196, 182));
    let height = (track.height as f32 * visible as f32 / total as f32).max(18.0) as u32;
    let max_offset = track.height.saturating_sub(height) as f32;
    let max_scroll = total.saturating_sub(visible).max(1) as f32;
    let progress = (offset as f32 / max_scroll).clamp(0.0, 1.0);
    painter.fill_rounded_rect(
        Rect::new(
            track.x + 1,
            track.y + (progress * max_offset) as i32,
            track.width.saturating_sub(2),
            height,
        ),
        2,
        Color::rgb(178, 190, 214),
    );
}

fn draw_menu(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    kind: MenuKind,
) {
    if state.has_pointer {
        if let Some(index) = menu_item_at(state, kind, state.pointer) {
            let entry = menu_items(kind)[index];
            if !entry.separator && entry.enabled && state.action_enabled(entry.action, fs) {
                state.menu_hover = index;
            }
        }
    }
    let rect = menu_rect(state, kind);
    painter.shadow(rect, 3, 7, 80);
    painter.fill_rect(rect, FACE_LIGHT);
    painter.stroke_rect(rect, 1, BORDER_DARK);
    let mut y = rect.y + 3;
    for (index, entry) in menu_items(kind).iter().enumerate() {
        if entry.separator {
            painter.fill_rect(
                Rect::new(rect.x + 26, y + 3, rect.width.saturating_sub(32), 1),
                BORDER,
            );
            y += 7;
            continue;
        }
        let enabled = entry.enabled && state.action_enabled(entry.action, fs);
        let hovered = index == state.menu_hover;
        let item_rect = Rect::new(rect.x + 2, y, rect.width.saturating_sub(4), 21);
        if hovered && enabled {
            painter.fill_rect(item_rect, SELECT);
        }
        if state.action_checked(entry.action) {
            paint_icon(
                painter,
                Rect::new(item_rect.x + 5, item_rect.y + 6, 10, 10),
                Icon::Check,
                if hovered { Color::WHITE } else { TEXT },
            );
        }
        painter.draw_text_ellipsis(
            Rect::new(
                item_rect.x + 22,
                item_rect.y,
                item_rect.width.saturating_sub(86),
                item_rect.height,
            ),
            entry.label,
            if !enabled {
                DISABLED
            } else if hovered {
                Color::WHITE
            } else {
                TEXT
            },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        if !entry.shortcut.is_empty() {
            painter.draw_text_ellipsis(
                Rect::new(
                    item_rect.right_i32() - 78,
                    item_rect.y,
                    72,
                    item_rect.height,
                ),
                entry.shortcut,
                if !enabled {
                    DISABLED
                } else if hovered {
                    Color::WHITE
                } else {
                    MUTED
                },
                1,
                Align::End,
                VerticalAlign::Middle,
            );
        }
        y += 21;
    }
}

fn draw_context(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    context: ContextMenu,
) {
    if state.has_pointer {
        if let Some(index) = context_item_at(state, context, state.pointer) {
            let entries = context_entries(context);
            let entry = entries[index];
            if !entry.separator && entry.enabled {
                state.context = Some(ContextMenu {
                    hover: index,
                    ..context
                });
            }
        }
    }
    let context = state.context.unwrap_or(context);
    let rect = context_rect(state, context);
    painter.shadow(rect, 3, 7, 80);
    painter.fill_rect(rect, FACE_LIGHT);
    painter.stroke_rect(rect, 1, BORDER_DARK);
    let mut y = rect.y + 3;
    for (index, entry) in context_entries(context).iter().enumerate() {
        if entry.separator {
            painter.fill_rect(
                Rect::new(rect.x + 24, y + 3, rect.width.saturating_sub(28), 1),
                BORDER,
            );
            y += 7;
            continue;
        }
        let hovered = index == context.hover;
        let item_rect = Rect::new(rect.x + 2, y, rect.width.saturating_sub(4), 21);
        if hovered {
            painter.fill_rect(item_rect, SELECT);
        }
        painter.draw_text_ellipsis(
            Rect::new(
                item_rect.x + 22,
                item_rect.y,
                item_rect.width.saturating_sub(88),
                item_rect.height,
            ),
            entry.label,
            if !entry.enabled {
                DISABLED
            } else if hovered {
                Color::WHITE
            } else {
                TEXT
            },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        if !entry.shortcut.is_empty() {
            painter.draw_text_ellipsis(
                Rect::new(
                    item_rect.right_i32() - 78,
                    item_rect.y,
                    72,
                    item_rect.height,
                ),
                entry.shortcut,
                if !entry.enabled {
                    DISABLED
                } else if hovered {
                    Color::WHITE
                } else {
                    MUTED
                },
                1,
                Align::End,
                VerticalAlign::Middle,
            );
        }
        y += 21;
    }
    let _ = fs;
}

fn draw_address_dropdown(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &ExplorerState,
    layout: Layout,
) {
    let rect = address_drop_rect(state, layout);
    painter.shadow(rect, 2, 5, 70);
    painter.fill_rect(rect, FIELD);
    painter.stroke_rect(rect, 1, FIELD_BORDER);
    let count = state.history_len.min(8);
    for index in 0..count {
        let row = Rect::new(
            rect.x + 1,
            rect.y + 2 + index as i32 * 18,
            rect.width.saturating_sub(2),
            18,
        );
        if index == state.history_pos {
            painter.fill_rect(row, SELECT_SOFT);
        }
        let node = state.history[index];
        let mut buffer = [0u8; PATH_MAX];
        let path = fs.path(node, &mut buffer).unwrap_or("/");
        painter.draw_text_ellipsis(
            Rect::new(row.x + 20, row.y, row.width.saturating_sub(24), row.height),
            path,
            TEXT,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        draw_folder_icon(painter, Rect::new(row.x + 3, row.y + 3, 12, 12));
    }
}

fn draw_dialog(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    now: u64,
) {
    let dialog = state.dialog;
    if dialog == Dialog::None {
        return;
    }
    painter.fill_rect_alpha(state.screen, Color::rgb(10, 20, 40), 86);
    let layout = DialogLayout::compute(state.screen, dialog);
    painter.shadow(layout.window, 7, 12, 110);
    painter.fill_rect(layout.window, TITLE_BOTTOM);
    painter.fill_rect(layout.inner, FACE);
    painter
        .canvas_mut()
        .fill_vertical_gradient(layout.title, TITLE_TOP, TITLE_BOTTOM);
    let title = dialog_title(dialog, state, fs);
    painter.draw_text_ellipsis(
        Rect::new(
            layout.title.x + 10,
            layout.title.y,
            layout.title.width.saturating_sub(40),
            layout.title.height,
        ),
        title.as_str(),
        Color::WHITE,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    draw_close_x(painter, layout.close, layout.close.contains(state.pointer));
    match dialog {
        Dialog::About => draw_about(painter, layout),
        Dialog::Options => draw_options(painter, state, layout),
        Dialog::Properties(node) => draw_properties(painter, fs, node, layout),
        Dialog::Viewer(node) => draw_viewer(painter, fs, state, node, layout),
        Dialog::Notice => draw_notice(painter, state, layout),
        Dialog::None => {}
    }
    let label = if matches!(dialog, Dialog::Viewer(_)) {
        "Close"
    } else {
        "OK"
    };
    draw_push_button(
        painter,
        layout.ok,
        label,
        layout.ok.contains(state.pointer),
        true,
    );
    let _ = now;
}

fn dialog_title(dialog: Dialog, state: &ExplorerState, fs: &dyn FileSystem) -> Text<80> {
    let mut title = Text::<80>::new();
    match dialog {
        Dialog::About => {
            title.set("About File Explorer");
        }
        Dialog::Options => {
            title.set("Folder Options");
        }
        Dialog::Properties(node) => {
            let _ = write!(title, "{} Properties", fs.name(node).unwrap_or("Item"));
        }
        Dialog::Viewer(node) => {
            let _ = write!(title, "{} - Viewer", fs.name(node).unwrap_or("File"));
        }
        Dialog::Notice => {
            title.set(state.notice_title.as_str());
        }
        Dialog::None => {}
    }
    title
}

fn draw_about(painter: &mut Painter<'_, '_>, layout: DialogLayout) {
    let body = layout.body;
    draw_folder_icon(painter, Rect::new(body.x + 8, body.y + 6, 48, 44));
    painter.draw_text(
        body.x + 70,
        body.y + 7,
        "File Explorer for feos",
        Color::rgb(0, 60, 170),
        2,
    );
    painter.draw_text(body.x + 70, body.y + 28, "Windows XP-style shell", MUTED, 1);
    let lines = [
        "A complete, allocation-free file explorer:",
        "menu bar, standard buttons, address bar, folder tree,",
        "search pane, four view modes, preview pane, dialogs,",
        "context menus, mouse input, wheel scrolling and",
        "keyboard navigation.",
        "",
        "The filesystem is read-only and embedded in the kernel.",
        "feos 0.1.0",
    ];
    let mut y = body.y + 64;
    for line in lines {
        painter.draw_text(body.x + 8, y, line, TEXT, 1);
        y += 14;
    }
}

fn draw_options(painter: &mut Painter<'_, '_>, state: &ExplorerState, layout: DialogLayout) {
    let body = layout.body;
    painter.draw_text(
        body.x + 8,
        body.y + 2,
        "Use these settings for the current session:",
        TEXT,
        1,
    );
    let checks = [
        state.toolbar_open,
        state.address_open,
        state.status_open,
        state.bar == ExplorerBar::Folders,
        state.preview_open,
    ];
    let labels = [
        "Show Standard Buttons",
        "Show Address Bar",
        "Show Status Bar",
        "Show Folders Pane",
        "Show Preview Pane",
    ];
    for index in 0..5 {
        let rect = option_check_rect(layout, index);
        draw_checkbox(painter, rect, checks[index], false);
        painter.draw_text(rect.right_i32() + 8, rect.y + 1, labels[index], TEXT, 1);
    }
    painter.draw_text(
        body.x + 8,
        body.bottom_i32() - 24,
        "Tip: every option is also available from the View menu.",
        MUTED,
        1,
    );
}

fn draw_properties(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    node: NodeId,
    layout: DialogLayout,
) {
    let body = layout.body;
    draw_node_icon(
        painter,
        node,
        fs,
        Rect::new(body.x + 10, body.y + 8, 48, 44),
    );
    painter.draw_text(
        body.x + 70,
        body.y + 10,
        fs.name(node).unwrap_or("/"),
        TEXT,
        2,
    );
    painter.draw_text(body.x + 70, body.y + 31, type_name(node, fs), MUTED, 1);
    let mut y = body.y + 66;
    let mut path = Text::<PATH_MAX>::new();
    let mut buffer = [0u8; PATH_MAX];
    path.set(fs.path(node, &mut buffer).unwrap_or("/"));
    paint_property_line(painter, body.x + 8, y, "Location", path.as_str());
    y += 20;
    let size = if fs.is_dir(node) {
        Text::<16>::from_str("")
    } else {
        format_size(fs.size(node))
    };
    paint_property_line(painter, body.x + 8, y, "Size", size.as_str());
    y += 20;
    if fs.is_dir(node) {
        let mut count = Text::<32>::new();
        let _ = write!(count, "{} items", fs.child_count(node));
        paint_property_line(painter, body.x + 8, y, "Contains", count.as_str());
        y += 20;
    }
    paint_property_line(painter, body.x + 8, y, "Modified", "Not available");
    y += 20;
    paint_property_line(painter, body.x + 8, y, "Attributes", "Read-only");
    y += 26;
    painter.fill_rect(
        Rect::new(body.x + 8, y, body.width.saturating_sub(16), 1),
        BORDER,
    );
    y += 8;
    painter.draw_text(
        body.x + 8,
        y,
        "This item lives in the filesystem image embedded in the kernel.",
        MUTED,
        1,
    );
    painter.draw_text(
        body.x + 8,
        y + 16,
        "File and folder changes require rebuilding the image.",
        MUTED,
        1,
    );
}

fn paint_property_line(painter: &mut Painter<'_, '_>, x: i32, y: i32, label: &str, value: &str) {
    painter.draw_text(x, y, label, MUTED, 1);
    painter.draw_text(x + 92, y, value, TEXT, 1);
}

fn draw_viewer(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut ExplorerState,
    node: NodeId,
    layout: DialogLayout,
) {
    let body = layout.body;
    painter.fill_rect(body, FIELD);
    painter.stroke_rect(body, 1, FIELD_BORDER);
    let text_rect = Rect::new(
        body.x + 6,
        body.y + 5,
        body.width.saturating_sub(20),
        body.height.saturating_sub(10),
    );
    let clip = painter.push_clip(text_rect);
    let bytes = &state.viewer[..state.viewer_len];
    if is_text(bytes) {
        let text = core::str::from_utf8(bytes).unwrap_or("");
        draw_scrolled_text(painter, text_rect, text, state.viewer_scroll, TEXT);
    } else {
        draw_hex_dump(painter, text_rect, bytes, state.viewer_scroll);
    }
    painter.restore_clip(clip);
    let visible = (text_rect.height / 10).max(1) as usize;
    draw_scrollbar(
        painter,
        text_rect,
        state.viewer_scroll,
        visible,
        state.viewer_lines,
    );
    let mut footer = Text::<128>::new();
    let _ = write!(
        footer,
        "{} bytes   [{}]   {}read-only",
        fs.size(node),
        if is_text(bytes) { "text" } else { "binary" },
        if state.viewer_len < fs.size(node) {
            "truncated   "
        } else {
            ""
        }
    );
    painter.draw_text_ellipsis(
        Rect::new(
            body.x + 7,
            body.bottom_i32() - 15,
            body.width.saturating_sub(20),
            13,
        ),
        footer.as_str(),
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Top,
    );
}

fn draw_notice(painter: &mut Painter<'_, '_>, state: &ExplorerState, layout: DialogLayout) {
    let body = layout.body;
    paint_icon(
        painter,
        Rect::new(body.x + 8, body.y + 6, 30, 30),
        Icon::Info,
        Color::rgb(0, 84, 227),
    );
    painter.draw_text_ellipsis(
        Rect::new(body.x + 50, body.y + 4, body.width.saturating_sub(56), 22),
        state.notice_title.as_str(),
        TEXT,
        2,
        Align::Start,
        VerticalAlign::Middle,
    );
    painter.draw_text_wrapped(
        Rect::new(
            body.x + 50,
            body.y + 32,
            body.width.saturating_sub(56),
            body.height.saturating_sub(34),
        ),
        state.notice_body.as_str(),
        TEXT,
        1,
    );
}

fn draw_scrolled_text(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    text: &str,
    offset: usize,
    color: Color,
) {
    let mut y = rect.y;
    for (index, line) in text.split('\n').enumerate() {
        if index < offset {
            continue;
        }
        if y + 10 > rect.bottom_i32() {
            break;
        }
        painter.draw_text_ellipsis(
            Rect::new(rect.x, y, rect.width, 10),
            line.trim_end_matches('\r'),
            color,
            1,
            Align::Start,
            VerticalAlign::Top,
        );
        y += 10;
    }
}

fn draw_hex_dump(painter: &mut Painter<'_, '_>, rect: Rect, bytes: &[u8], offset: usize) {
    let rows = (bytes.len() + 15) / 16;
    let mut y = rect.y;
    for row in offset..rows {
        if y + 10 > rect.bottom_i32() {
            break;
        }
        let start = row * 16;
        let end = (start + 16).min(bytes.len());
        let mut line = Text::<96>::new();
        let _ = write!(line, "{:04X}  ", start);
        for index in 0..16 {
            if start + index < end {
                let _ = write!(line, "{:02X} ", bytes[start + index]);
            } else {
                let _ = write!(line, "   ");
            }
        }
        let _ = write!(line, " ");
        for index in start..end {
            let byte = bytes[index];
            let ch = if (0x20..=0x7e).contains(&byte) {
                byte as char
            } else {
                '.'
            };
            let _ = write!(line, "{ch}");
        }
        painter.draw_text(rect.x, y, line.as_str(), TEXT, 1);
        y += 10;
    }
}

fn draw_text_field(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    text: &str,
    cursor: usize,
    focused: bool,
) {
    painter.fill_rect(rect, FIELD);
    painter.stroke_rect(rect, 1, if focused { SELECT } else { FIELD_BORDER });
    let inner = rect.inset_xy(4, 1);
    painter.draw_text_ellipsis(inner, text, TEXT, 1, Align::Start, VerticalAlign::Middle);
    if focused {
        let prefix = &text[..cursor.min(text.len())];
        let x = inner.x + painter.text_width(prefix, 1);
        if x < inner.right_i32() {
            painter.fill_rect(
                Rect::new(x, inner.y + 3, 1, inner.height.saturating_sub(6)),
                TEXT,
            );
        }
    }
}

fn draw_checkbox(painter: &mut Painter<'_, '_>, rect: Rect, checked: bool, disabled: bool) {
    painter.fill_rect(rect, FIELD);
    painter.stroke_rect(rect, 1, if disabled { DISABLED } else { FIELD_BORDER });
    if checked {
        paint_icon(
            painter,
            Rect::new(
                rect.x + 2,
                rect.y + 2,
                rect.width.saturating_sub(4),
                rect.height.saturating_sub(4),
            ),
            Icon::Check,
            if disabled {
                DISABLED
            } else {
                Color::rgb(20, 90, 200)
            },
        );
    }
}

fn draw_push_button(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    label: &str,
    hovered: bool,
    default: bool,
) {
    painter.fill_rounded_rect(rect, 3, if hovered { HOVER_LIGHT } else { FACE_LIGHT });
    painter.stroke_rounded_rect(
        rect,
        3,
        1,
        if default && !hovered {
            Color::rgb(70, 110, 180)
        } else {
            BORDER_DARK
        },
    );
    painter.draw_text_centered(rect, label, TEXT, 1);
}

fn draw_close_x(painter: &mut Painter<'_, '_>, rect: Rect, hovered: bool) {
    painter.fill_rounded_rect(
        rect,
        3,
        if hovered {
            Color::rgb(232, 78, 58)
        } else {
            Color::rgb(214, 58, 40)
        },
    );
    let cx = rect.x + rect.width as i32 / 2;
    let cy = rect.y + rect.height as i32 / 2;
    painter.line(cx - 3, cy - 3, cx + 4, cy + 4, Color::WHITE);
    painter.line(cx + 4, cy - 3, cx - 3, cy + 4, Color::WHITE);
}

fn draw_node_icon(painter: &mut Painter<'_, '_>, node: NodeId, fs: &dyn FileSystem, rect: Rect) {
    if fs.is_dir(node) {
        draw_folder_icon(painter, rect);
    } else {
        draw_file_icon(painter, rect, file_accent(fs.name(node).unwrap_or("")));
    }
}

fn draw_folder_icon(painter: &mut Painter<'_, '_>, rect: Rect) {
    if rect.width < 4 || rect.height < 4 {
        return;
    }
    let w = rect.width as i32;
    let h = rect.height as i32;
    let tab = Rect::new(
        rect.x + 1,
        rect.y + 1,
        (w * 55 / 100).max(4) as u32,
        (h / 4).max(3) as u32,
    );
    painter.fill_rounded_rect(tab, 2, FOLDER_BACK);
    let body = Rect::new(rect.x, rect.y + h / 4, rect.width, (h - h / 4) as u32);
    painter.fill_rounded_rect(body, 2, FOLDER_FRONT);
    painter.stroke_rounded_rect(body, 2, 1, FOLDER_EDGE);
    painter.fill_rect(
        Rect::new(
            body.x + 1,
            body.y + 1,
            body.width.saturating_sub(2),
            (body.height / 6).max(1) as u32,
        ),
        Color::rgb(255, 234, 160),
    );
    if w >= 12 {
        painter.fill_rect(
            Rect::new(
                body.x + 2,
                body.bottom_i32() - 3,
                body.width.saturating_sub(4),
                1,
            ),
            Color::rgb(222, 169, 45),
        );
    }
}

fn draw_file_icon(painter: &mut Painter<'_, '_>, rect: Rect, accent: Color) {
    if rect.width < 5 || rect.height < 5 {
        return;
    }
    let w = rect.width as i32;
    let h = rect.height as i32;
    let page = Rect::new(
        rect.x + w / 6,
        rect.y + 1,
        (w * 2 / 3).max(5) as u32,
        (h - 2).max(5) as u32,
    );
    painter.fill_rect(page, Color::WHITE);
    painter.stroke_rect(page, 1, Color::rgb(110, 110, 110));
    let fold = ((page.width.min(page.height as u32) / 4).max(3)) as i32;
    let points = [
        Point::new(page.right_i32() - fold, page.y + 1),
        Point::new(page.right_i32() - 1, page.y + 1 + fold),
        Point::new(page.right_i32() - 1, page.y + 1),
    ];
    painter.fill_polygon(&points, accent);
    painter.line(
        page.right_i32() - fold - 1,
        page.y + 1,
        page.right_i32() - fold - 1,
        page.y + fold,
        Color::rgb(110, 110, 110),
    );
    painter.line(
        page.right_i32() - fold,
        page.y + fold,
        page.right_i32() - 1,
        page.y + fold,
        Color::rgb(110, 110, 110),
    );
    let line_y = page.bottom_i32() - (page.height as i32 / 3).max(4);
    painter.fill_rect(
        Rect::new(page.x + 3, line_y, page.width.saturating_sub(6), 1),
        accent,
    );
    painter.fill_rect(
        Rect::new(page.x + 3, line_y + 3, page.width.saturating_sub(8), 1),
        Color::rgb(170, 170, 170),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matching_is_case_insensitive_and_bounded() {
        assert!(contains_ignore_ascii_case("README.txt", "read"));
        assert!(contains_ignore_ascii_case("README.txt", "TXT"));
        assert!(!contains_ignore_ascii_case("README.txt", "docs"));
        assert!(contains_ignore_ascii_case("anything", ""));
    }

    #[test]
    fn extensions_and_types_are_recognised() {
        assert_eq!(file_extension("archive.tar.gz"), "gz");
        assert_eq!(file_extension("README"), "");
        assert_eq!(file_extension(".hidden"), "");
    }

    #[test]
    fn viewer_line_count_handles_text_and_binary() {
        assert_eq!(viewer_line_count(b"one\ntwo\n"), 3);
        assert_eq!(viewer_line_count(&[0u8; 32]), 2);
    }

    #[test]
    fn bit_sets_are_bounded() {
        let mut words = [0u64; SELECTION_WORDS];
        set_bit(&mut words, 0);
        set_bit(&mut words, 511);
        set_bit(&mut words, 4096);
        assert!(bit(&words, 0));
        assert!(bit(&words, 511));
        assert!(!bit(&words, 4096));
        clear_bit(&mut words, 0);
        assert!(!bit(&words, 0));
    }
}
