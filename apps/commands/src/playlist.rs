//! Playlists as M3U files: one path a line, and `#` for anything else.

use alloc::string::String;

/// Where a saved playlist goes: the first of these that can be written. A disk is
/// mounted at the root when there is one, and `/tmp` is memory.
pub const PLAYLIST_FILES: [&str; 2] = ["/playlist.m3u", "/tmp/playlist.m3u"];
/// The largest playlist file worth reading.
pub const MAX_BYTES: usize = 256 * 1024;

/// Whether this name is a playlist's.
pub fn is_playlist(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("m3u") || extension.eq_ignore_ascii_case("m3u8"))
}

/// Writes the file for these paths.
pub fn format<'a>(paths: impl Iterator<Item = &'a str>) -> String {
    let mut text = String::from("#EXTM3U\n");
    for path in paths {
        // A path this format can't hold is left out rather than saved as something else.
        if !path.contains(['\n', '\r']) {
            text.push_str(path);
            text.push('\n');
        }
    }
    text
}

/// Calls `f` with each entry, made absolute if it's relative to the file's folder.
/// Entries that are URLs come out as they are, for the caller to fail to find.
pub fn entries(text: &str, directory: &str, mut f: impl FnMut(&str)) {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('/') {
            f(line);
        } else {
            let mut path = String::from(directory.trim_end_matches('/'));
            path.push('/');
            path.push_str(line);
            f(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn what_is_saved_comes_back() {
        let saved = format(["/media/a.mp4", "/big file.mkv"].into_iter());
        assert_eq!(saved, "#EXTM3U\n/media/a.mp4\n/big file.mkv\n");
        let mut read = Vec::new();
        entries(&saved, "/", |path| read.push(String::from(path)));
        assert_eq!(read, ["/media/a.mp4", "/big file.mkv"]);
    }

    #[test]
    fn other_players_files_work() {
        let text = "#EXTM3U\r\n#EXTINF:12,A title\r\nsound/a.mp3\r\n\r\n/b.flac\r\n";
        let mut read = Vec::new();
        entries(text, "/music/", |path| read.push(String::from(path)));
        assert_eq!(read, ["/music/sound/a.mp3", "/b.flac"]);
    }

    #[test]
    fn a_name_with_a_newline_is_left_out() {
        assert_eq!(format(["/ok", "/not\nok"].into_iter()), "#EXTM3U\n/ok\n");
    }

    #[test]
    fn names() {
        assert!(is_playlist("a.M3U"));
        assert!(is_playlist("a.m3u8"));
        assert!(!is_playlist("m3u"));
    }
}
