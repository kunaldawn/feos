//! The stock icons the desktop and its apps draw.
//!
//! Only these are embedded, not the whole of `assets/icons`: the pictures named
//! in [`Stock::files`], each at the sizes something draws it at. They're decoded
//! when drawn, into a buffer on the stack, so drawing allocates nothing and can't
//! fail for lack of memory. Pixels are blended with straight alpha, which is what
//! these files hold (see `feos_image::ico`).
//!
//! This crate knows nothing of the toolkit: it draws on anything that implements
//! [`Target`], which `feos-gui` does for its `Painter`.

#![cfg_attr(not(test), no_std)]

use feos_gfx::{Color, Rect};

/// Bytes for the largest size, in RGBA.
const LARGEST: usize = 48 * 48 * 4;

/// Something an icon can be drawn on.
pub trait Target {
    /// Whether any of `rect` would be drawn.
    fn visible(&self, rect: Rect) -> bool;
    /// Blends one pixel over what's there, with straight alpha.
    fn blend_pixel(&mut self, x: i32, y: i32, color: Color, alpha: u8);
}

/// One picture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stock {
    /// A closed folder.
    Folder,
    /// An open folder.
    FolderOpen,
    /// An open folder holding a page.
    Documents,
    /// The computer.
    Computer,
    /// A hard disk.
    Drive,
    /// A text document.
    TextFile,
    /// Source code, configuration and other system files.
    SystemFile,
    /// A file with no type of its own.
    GenericFile,
    /// A picture.
    ImageFile,
    /// A sound file.
    AudioFile,
    /// A video file.
    VideoFile,
    /// An archive of other files.
    Archive,
    /// An application bundle.
    AppBundle,
    /// Help.
    Help,
    /// Search.
    Search,
    /// Run a command.
    Run,
    /// Settings.
    ControlPanel,
    /// Display settings.
    Display,
    /// Information: a speech bubble with an i.
    Info,
    /// An error or a stop: a red no-entry circle.
    Error,
    /// A gear, for options.
    Gear,
    /// A house.
    Home,
    /// People: the user.
    Users,
    /// A calendar, for dates and times.
    Calendar,
    /// The recycle bin, empty.
    RecycleBin,
    /// A sheet and a pen, for editing.
    Edit,
    /// Two pages, for copying.
    Copy,
    /// An envelope, for mail and messages.
    Mail,
    /// A globe, for the network and the Internet.
    Globe,
    /// A memory chip, for processors and memory.
    Chip,
    /// A software box, for packages and installing.
    Software,
    /// A key on an orange button, for logging off.
    LogOff,
    /// A red power button, for turning off.
    ShutDown,
    /// A window with a gear, for the terminal and commands.
    Terminal,
    /// A pie chart, for the task manager and performance.
    Performance,
    /// A plain program window, for an application without its own picture.
    Application,
    /// A video camera, for the media player.
    Camcorder,
    /// A music note and a speaker, for sound.
    Sounds,
}

impl Stock {
    /// Every picture, for tests.
    pub const ALL: [Stock; 38] = [
        Stock::Folder,
        Stock::FolderOpen,
        Stock::Documents,
        Stock::Computer,
        Stock::Drive,
        Stock::TextFile,
        Stock::SystemFile,
        Stock::GenericFile,
        Stock::ImageFile,
        Stock::AudioFile,
        Stock::VideoFile,
        Stock::Archive,
        Stock::AppBundle,
        Stock::Help,
        Stock::Search,
        Stock::Run,
        Stock::ControlPanel,
        Stock::Display,
        Stock::Info,
        Stock::Error,
        Stock::Gear,
        Stock::Home,
        Stock::Users,
        Stock::Calendar,
        Stock::RecycleBin,
        Stock::Edit,
        Stock::Copy,
        Stock::Mail,
        Stock::Globe,
        Stock::Chip,
        Stock::Software,
        Stock::LogOff,
        Stock::ShutDown,
        Stock::Terminal,
        Stock::Performance,
        Stock::Application,
        Stock::Camcorder,
        Stock::Sounds,
    ];

    /// The encoded files with the side each is drawn at, smallest first. Only
    /// the sizes something draws are here: files and folders at 16, 32 and 48
    /// for the views of File Explorer, most others at 16 and 32.
    pub fn files(self) -> &'static [(u32, &'static [u8])] {
        macro_rules! sizes {
            ($base:literal: $($side:literal),+) => {
                &[$(($side, include_bytes!(concat!("../../../assets/icons/", $base, "-", stringify!($side), ".ico")))),+]
            };
        }
        match self {
            Stock::Folder => sizes!("folders/folder-closed": 16, 32, 48),
            Stock::FolderOpen => sizes!("folders/folder-open": 16, 32, 48),
            Stock::Documents => sizes!("folders/folder-open-documents": 16, 32, 48),
            Stock::Computer => sizes!("computers/computer": 16, 32, 48),
            Stock::Drive => sizes!("drives/hard-disk": 16, 32, 48),
            Stock::TextFile => sizes!("files/text-file": 16, 32, 48),
            Stock::SystemFile => sizes!("files/file-system": 16, 32, 48),
            Stock::GenericFile => sizes!("files/file-unassociated": 16, 32, 48),
            Stock::ImageFile => sizes!("media/photos": 16, 32, 48),
            Stock::AudioFile => sizes!("files/file-audio": 16, 32, 48),
            Stock::VideoFile => sizes!("files/file-video": 16, 32, 48),
            Stock::Archive => sizes!("apps/backup": 16, 32, 48),
            Stock::AppBundle => sizes!("apps/program-group": 16, 32, 48),
            Stock::Help => sizes!("status/help": 16, 32, 48),
            Stock::Search => sizes!("navigation/search": 16, 32, 48),
            Stock::Run => sizes!("apps/run": 16, 32, 48),
            Stock::ControlPanel => sizes!("system/settings": 16, 32, 48),
            Stock::Display => sizes!("system/display-properties": 16, 32, 48),
            Stock::Info => sizes!("status/info": 16, 32),
            Stock::Error => sizes!("status/blocked": 16, 32),
            Stock::Gear => sizes!("system/performance-gear": 16, 32),
            Stock::Home => sizes!("network/home-network": 16, 32),
            Stock::Users => sizes!("users/users": 16, 32),
            Stock::Calendar => sizes!("system/date-time-regional": 16, 32),
            Stock::RecycleBin => sizes!("system/recycle-bin-empty": 16, 32),
            Stock::Edit => sizes!("apps/pen-and-paper": 16, 32),
            Stock::Copy => sizes!("files/file-copy": 16),
            Stock::Mail => sizes!("network/email-open": 16),
            Stock::Globe => sizes!("network/internet-globe": 16, 32),
            Stock::Chip => sizes!("devices/memory-chip": 16, 32),
            Stock::Software => sizes!("system/add-remove-programs": 16, 32),
            Stock::LogOff => sizes!("system/log-off": 16, 24),
            Stock::ShutDown => sizes!("system/shutdown": 16, 24),
            Stock::Terminal => sizes!("apps/window-gear": 16, 32),
            Stock::Performance => sizes!("system/performance-pie-chart": 16, 32),
            Stock::Application => sizes!("apps/application-window": 16, 32),
            Stock::Camcorder => sizes!("devices/camcorder": 16, 32, 48),
            Stock::Sounds => sizes!("media/sounds": 16, 32, 48),
        }
    }
}

/// The file to use for a size: the smallest not smaller than it, so that what's
/// drawn is the artist's own version at or just above the size it was made for,
/// and the largest when none is that big.
fn file_for(icon: Stock, size: u32) -> &'static [u8] {
    let files = icon.files();
    files
        .iter()
        .find(|(side, _)| *side >= size)
        .or(files.last())
        .map_or(&[][..], |(_, file)| *file)
}

/// Draws an icon to fill `rect`, scaled if it must be. Returns `false` if the
/// file wouldn't decode, which only a test can arrange.
///
/// Shrinking averages the source pixels each target pixel covers, weighted by
/// their alpha, so a 32-pixel picture drawn at 24 keeps its lines; growing
/// repeats pixels.
pub fn draw<T: Target + ?Sized>(target: &mut T, icon: Stock, rect: Rect) -> bool {
    if rect.is_empty() || !target.visible(rect) {
        return true;
    }
    let mut rgba = [0u8; LARGEST];
    let file = file_for(icon, rect.width.max(rect.height));
    let Ok(info) = feos_image::decode(file, &mut rgba, &mut []) else {
        return false;
    };
    let (source_width, source_height) = (info.width as i32, info.height as i32);
    let (width, height) = (rect.width as i32, rect.height as i32);
    for y in 0..height {
        let (top, bottom) = span(y, height, source_height);
        for x in 0..width {
            let (left, right) = span(x, width, source_width);
            let (mut red, mut green, mut blue, mut alpha, mut count) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for source_y in top..bottom {
                for source_x in left..right {
                    let at = ((source_y * source_width + source_x) * 4) as usize;
                    let weight = u32::from(rgba[at + 3]);
                    red += u32::from(rgba[at]) * weight;
                    green += u32::from(rgba[at + 1]) * weight;
                    blue += u32::from(rgba[at + 2]) * weight;
                    alpha += weight;
                    count += 1;
                }
            }
            if alpha == 0 {
                continue;
            }
            let color = Color::rgb((red / alpha) as u8, (green / alpha) as u8, (blue / alpha) as u8);
            target.blend_pixel(rect.x + x, rect.y + y, color, (alpha / count.max(1)) as u8);
        }
    }
    true
}

/// The source pixels that target pixel `at` of `target` covers, out of `source`.
/// Always at least one.
fn span(at: i32, target: i32, source: i32) -> (i32, i32) {
    let start = at * source / target;
    let end = ((at + 1) * source / target).max(start + 1).min(source);
    (start, end.max(start + 1))
}

impl Target for feos_gfx::Canvas<'_> {
    fn visible(&self, rect: Rect) -> bool {
        rect.x < self.width() as i32
            && rect.y < self.height() as i32
            && rect.x + rect.width as i32 > 0
            && rect.y + rect.height as i32 > 0
    }

    fn blend_pixel(&mut self, x: i32, y: i32, color: Color, alpha: u8) {
        if let Some(existing) = self.pixel(x, y) {
            self.put_pixel(x, y, existing.lerp(color, alpha));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_gfx::Canvas;

    #[test]
    fn every_file_decodes_at_its_stated_size() {
        for icon in Stock::ALL {
            let files = icon.files();
            assert!(!files.is_empty(), "{icon:?}");
            for (side, file) in files {
                let mut rgba = [0u8; LARGEST];
                let info = feos_image::decode(file, &mut rgba, &mut []).unwrap();
                assert_eq!((info.width, info.height), (*side, *side), "{icon:?}");
                assert!(rgba.chunks_exact(4).any(|pixel| pixel[3] == 255), "{icon:?}");
            }
            assert!(files.windows(2).all(|pair| pair[0].0 < pair[1].0), "{icon:?}");
        }
    }

    #[test]
    fn the_size_picks_the_file() {
        let files = Stock::Folder.files();
        assert_eq!(file_for(Stock::Folder, 16).as_ptr(), files[0].1.as_ptr());
        assert_eq!(file_for(Stock::Folder, 24).as_ptr(), files[1].1.as_ptr());
        assert_eq!(file_for(Stock::Folder, 48).as_ptr(), files[2].1.as_ptr());
        assert_eq!(file_for(Stock::Folder, 96).as_ptr(), files[2].1.as_ptr());
        assert_eq!(file_for(Stock::Copy, 32).as_ptr(), Stock::Copy.files()[0].1.as_ptr());
    }

    #[test]
    fn drawing_blends_over_the_background_and_leaves_the_corners_alone() {
        for side in [12, 16, 24, 32, 40] {
            let mut pixels = [0x0012_3456u32; 40 * 40];
            let mut canvas = Canvas::new(&mut pixels, 40, 40, 40).unwrap();
            assert!(draw(&mut canvas, Stock::Folder, Rect::new(0, 0, side, side)));
            drop(canvas);
            assert_eq!(pixels[0] & 0xff_ffff, 0x12_3456, "transparent stays");
            assert!(pixels.iter().any(|&pixel| pixel & 0xff_ffff != 0x12_3456));
            if side < 40 {
                let past = side as usize * 40 + side as usize;
                assert_eq!(pixels[past] & 0xff_ffff, 0x12_3456, "nothing past the rect");
            }
        }
    }

    #[test]
    fn spans_cover_every_source_pixel_once_when_shrinking() {
        for (target, source) in [(24, 32), (12, 16), (20, 48), (16, 16), (32, 16)] {
            let mut next = 0;
            for at in 0..target {
                let (start, end) = span(at, target, source);
                assert!(start < end && end <= source);
                if target <= source {
                    assert_eq!(start, next);
                    next = end;
                }
            }
            if target <= source {
                assert_eq!(next, source);
            }
        }
    }

    #[test]
    fn a_rect_off_the_canvas_is_harmless() {
        let mut pixels = [0u32; 16];
        let mut canvas = Canvas::new(&mut pixels, 4, 4, 4).unwrap();
        assert!(draw(&mut canvas, Stock::Help, Rect::new(-40, -40, 16, 16)));
        assert!(draw(&mut canvas, Stock::Help, Rect::new(2, 2, 16, 16)));
    }
}
