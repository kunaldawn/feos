//! The stock icons the desktop and its apps draw.
//!
//! Only these are embedded, not the whole of `assets/icons`: 18 pictures, each at
//! 16, 32 and 48 pixels, about 250 KiB in all. They're decoded when drawn, into a
//! buffer on the stack, so drawing allocates nothing and can't fail for lack of
//! memory. Pixels are blended with straight alpha, which is what these files hold
//! (see `feos_image::ico`).

#![cfg_attr(not(test), no_std)]

use feos_gfx::{Color, Rect};
use feos_gui::Painter;

/// Bytes for the largest size, in RGBA.
const LARGEST: usize = 48 * 48 * 4;

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
}

impl Stock {
    /// Every picture, for tests.
    pub const ALL: [Stock; 18] = [
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
    ];

    /// The encoded files, smallest first.
    fn files(self) -> [&'static [u8]; 3] {
        macro_rules! sizes {
            ($base:literal) => {
                [
                    include_bytes!(concat!("../../../assets/icons/", $base, "-16.ico")),
                    include_bytes!(concat!("../../../assets/icons/", $base, "-32.ico")),
                    include_bytes!(concat!("../../../assets/icons/", $base, "-48.ico")),
                ]
            };
        }
        match self {
            Stock::Folder => sizes!("folders/folder-closed"),
            Stock::FolderOpen => sizes!("folders/folder-open"),
            Stock::Documents => sizes!("folders/folder-open-documents"),
            Stock::Computer => sizes!("computers/computer"),
            Stock::Drive => sizes!("drives/hard-disk"),
            Stock::TextFile => sizes!("files/text-file"),
            Stock::SystemFile => sizes!("files/file-system"),
            Stock::GenericFile => sizes!("files/file-unassociated"),
            Stock::ImageFile => sizes!("media/photos"),
            Stock::AudioFile => sizes!("files/file-audio"),
            Stock::VideoFile => sizes!("files/file-video"),
            Stock::Archive => sizes!("apps/backup"),
            Stock::AppBundle => sizes!("apps/program-group"),
            Stock::Help => sizes!("status/help"),
            Stock::Search => sizes!("navigation/search"),
            Stock::Run => sizes!("apps/run"),
            Stock::ControlPanel => sizes!("system/settings"),
            Stock::Display => sizes!("system/display-properties"),
        }
    }
}

/// The file to use for a size: the smallest not smaller than it, so that what's
/// drawn is the artist's own version at or just below the size it was made for.
fn file_for(icon: Stock, size: u32) -> &'static [u8] {
    let files = icon.files();
    match size {
        0..=16 => files[0],
        17..=32 => files[1],
        _ => files[2],
    }
}

/// Draws an icon to fill `rect`, scaled if it must be. Returns `false` if the
/// file wouldn't decode, which only a test can arrange.
pub fn draw(painter: &mut Painter<'_, '_>, icon: Stock, rect: Rect) -> bool {
    if rect.is_empty() || !painter.visible(rect) {
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
        let source_y = y * source_height / height;
        for x in 0..width {
            let source_x = x * source_width / width;
            let at = ((source_y * source_width + source_x) * 4) as usize;
            let alpha = rgba[at + 3];
            if alpha != 0 {
                painter.blend_pixel(
                    rect.x + x,
                    rect.y + y,
                    Color::rgb(rgba[at], rgba[at + 1], rgba[at + 2]),
                    alpha,
                );
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_gfx::Canvas;

    #[test]
    fn every_file_decodes_at_its_stated_size() {
        for icon in Stock::ALL {
            for (file, side) in icon.files().into_iter().zip([16, 32, 48]) {
                let mut rgba = [0u8; LARGEST];
                let info = feos_image::decode(file, &mut rgba, &mut []).unwrap();
                assert_eq!((info.width, info.height), (side, side), "{icon:?}");
                assert!(rgba.chunks_exact(4).any(|pixel| pixel[3] == 255), "{icon:?}");
            }
        }
    }

    #[test]
    fn the_size_picks_the_file() {
        let files = Stock::Folder.files();
        assert_eq!(file_for(Stock::Folder, 16).as_ptr(), files[0].as_ptr());
        assert_eq!(file_for(Stock::Folder, 24).as_ptr(), files[1].as_ptr());
        assert_eq!(file_for(Stock::Folder, 48).as_ptr(), files[2].as_ptr());
    }

    #[test]
    fn drawing_blends_over_the_background_and_leaves_the_corners_alone() {
        let mut pixels = [0x0012_3456u32; 32 * 32];
        let mut canvas = Canvas::new(&mut pixels, 32, 32, 32).unwrap();
        let mut painter = Painter::new(&mut canvas);
        assert!(draw(&mut painter, Stock::Folder, Rect::new(0, 0, 32, 32)));
        drop(painter);
        assert_eq!(pixels[0] & 0xff_ffff, 0x12_3456, "transparent stays");
        assert!(pixels.iter().any(|&pixel| pixel & 0xff_ffff != 0x12_3456));
    }

    #[test]
    fn a_rect_off_the_canvas_is_harmless() {
        let mut pixels = [0u32; 16];
        let mut canvas = Canvas::new(&mut pixels, 4, 4, 4).unwrap();
        let mut painter = Painter::new(&mut canvas);
        assert!(draw(&mut painter, Stock::Help, Rect::new(-40, -40, 16, 16)));
        assert!(draw(&mut painter, Stock::Help, Rect::new(2, 2, 16, 16)));
    }
}
