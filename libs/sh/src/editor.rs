//! The serial console's front end to the [`LineEditor`].

use alloc::string::String;

use feos_keys::{ByteDecoder, KeyPress};

use crate::line::{render_line, rows_below, EditEvent, LineEditor};
use crate::{Host, Io, Shell};

/// Result of one `read_line` call.
pub enum ReadResult {
    /// A complete line (possibly containing embedded newlines from a
    /// continuation).
    Line(String),
    /// The input reached end of file (Ctrl-D on an empty line).
    Eof,
    /// The user pressed Ctrl-C.
    Interrupted,
}

/// Width assumed when the terminal will not say.
const DEFAULT_COLUMNS: usize = 80;

/// Bytes read while asking the terminal its width that turned out to be typing.
#[derive(Default)]
struct Replay {
    bytes: [u8; 16],
    length: usize,
    position: usize,
}

impl Replay {
    fn next(&mut self) -> Option<u8> {
        let byte = *self.bytes[..self.length].get(self.position)?;
        self.position += 1;
        Some(byte)
    }
}

/// Asks a real terminal how wide it is, once per shell: move far right, ask
/// where the cursor went, and go back. A terminal that does not answer gets the
/// default after a short wait, a late answer decodes to nothing, and anything
/// typed meanwhile is kept.
fn terminal_columns(shell: &mut Shell<'_>, io: &mut Io<'_>, replay: &mut Replay) -> usize {
    if let Some(columns) = shell
        .env
        .get("COLUMNS")
        .and_then(|text| text.parse::<usize>().ok())
        .filter(|columns| *columns > 0)
    {
        return columns;
    }
    if let Some(columns) = shell.columns {
        return columns;
    }
    let mut columns = DEFAULT_COLUMNS;
    if io.stdin.is_terminal() && io.stdout.is_terminal() {
        io.stdout.write(b"\x1b[999C\x1b[6n\r");
        let mut reply = [0u8; 16];
        let mut length = 0usize;
        while length < reply.len() {
            let mut byte = [0u8; 1];
            if io.stdin.read_pending(&mut byte) == 0 {
                break;
            }
            reply[length] = byte[0];
            length += 1;
            if !reply_so_far_is_valid(&reply[..length]) || byte[0] == b'R' {
                break;
            }
        }
        match parse_cursor_reply(&reply[..length]) {
            Some(found) => columns = found,
            None => {
                replay.bytes = reply;
                replay.length = length;
            }
        }
    }
    shell.columns = Some(columns);
    columns
}

/// True when `bytes` could still become `ESC [ row ; column R`.
fn reply_so_far_is_valid(bytes: &[u8]) -> bool {
    bytes.iter().enumerate().all(|(index, byte)| match index {
        0 => *byte == 0x1b,
        1 => *byte == b'[',
        _ => byte.is_ascii_digit() || matches!(byte, b';' | b'R'),
    })
}

/// Reads the column out of `ESC [ row ; column R`.
fn parse_cursor_reply(reply: &[u8]) -> Option<usize> {
    let text = core::str::from_utf8(reply).ok()?;
    let body = text.strip_prefix("\x1b[")?.strip_suffix('R')?;
    let (_, column) = body.split_once(';')?;
    column.parse().ok().filter(|column| *column > 0)
}

/// Reads and edits one command line, printing the prompt.
pub fn read_line(shell: &mut Shell<'_>, host: &mut dyn Host, io: &mut Io<'_>) -> ReadResult {
    let mut replay = Replay::default();
    let cols = terminal_columns(shell, io, &mut replay);
    let mut editor = LineEditor::new(shell.history.len());
    let mut decoder = ByteDecoder::new();
    let mut row = 0usize;

    io.stdout.write(shell.prompt().as_bytes());

    loop {
        let mut byte = [0u8; 1];
        // An escape already in progress must not wait forever for a next byte:
        // a lone Escape key is followed by nothing.
        let read = if let Some(saved) = replay.next() {
            byte[0] = saved;
            1
        } else if decoder.is_pending() {
            io.stdin.read_pending(&mut byte)
        } else {
            io.stdin.read_raw(&mut byte)
        };

        let mut keys: [Option<KeyPress>; 2] = [None, None];
        let mut count = 0usize;
        let mut collect = |key: KeyPress| {
            if let Some(slot) = keys.get_mut(count) {
                *slot = Some(key);
                count += 1;
            }
        };
        if read == 0 {
            if !decoder.is_pending() {
                return ReadResult::Eof;
            }
            decoder.flush(&mut collect);
        } else {
            decoder.feed(byte[0], &mut collect);
        }

        for key in keys.into_iter().flatten() {
            match editor.handle(key, shell, &*host) {
                EditEvent::None => {}
                EditEvent::Changed => {
                    row = redraw(io, &editor, shell, cols, row);
                }
                EditEvent::Submit(line) => {
                    finish_line(io, &editor, shell, cols, row);
                    return ReadResult::Line(line);
                }
                EditEvent::Continuation => {
                    io.stdout.write(b"\n");
                    io.stdout.write(shell.continuation_prompt().as_bytes());
                    row = 0;
                }
                EditEvent::Interrupted => {
                    io.stdout.write(b"^C\n");
                    return ReadResult::Interrupted;
                }
                EditEvent::Eof => return ReadResult::Eof,
                EditEvent::ClearScreen => {
                    io.stdout.write(b"\x1b[2J\x1b[H");
                    io.stdout.write(editor.prompt(shell).as_bytes());
                    row = redraw(io, &editor, shell, cols, 0);
                }
                EditEvent::Candidates(candidates) => {
                    finish_line(io, &editor, shell, cols, row);
                    for candidate in &candidates {
                        io.stdout.write(candidate.as_bytes());
                        io.stdout.write(b"  ");
                    }
                    io.stdout.write(b"\n");
                    io.stdout.write(editor.prompt(shell).as_bytes());
                    row = redraw(io, &editor, shell, cols, 0);
                }
            }
        }
    }
}

fn redraw(
    io: &mut Io<'_>,
    editor: &LineEditor,
    shell: &Shell<'_>,
    cols: usize,
    row: usize,
) -> usize {
    let prompt = editor.prompt(shell);
    let (bytes, new_row) = render_line(&prompt, editor.text(), editor.cursor(), cols, row);
    io.stdout.write(&bytes);
    new_row
}

/// Moves below the whole line, wherever the cursor is in it.
fn finish_line(io: &mut Io<'_>, editor: &LineEditor, shell: &Shell<'_>, cols: usize, row: usize) {
    let prompt = editor.prompt(shell);
    let down = rows_below(&prompt, editor.text(), cols).saturating_sub(row);
    for _ in 0..down {
        io.stdout.write(b"\n");
    }
    io.stdout.write(b"\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::NoHost;
    use feos_coreutils::{BytesInput, BytesOutput, NoSystem};

    fn read(input: &[u8]) -> (ReadResult, String) {
        let mut shell = Shell::new(None, &NoSystem);
        shell.history = alloc::vec![String::from("older")];
        let mut stdin = BytesInput::new(input.to_vec());
        let mut stdout = BytesOutput::new();
        let mut stderr = BytesOutput::new();
        let mut io = Io {
            stdin: &mut stdin,
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let result = read_line(&mut shell, &mut NoHost, &mut io);
        (
            result,
            String::from_utf8_lossy(stdout.bytes()).into_owned(),
        )
    }

    fn line(result: ReadResult) -> String {
        match result {
            ReadResult::Line(line) => line,
            _ => panic!("expected a line"),
        }
    }

    #[test]
    fn edits_with_escape_sequences() {
        assert_eq!(line(read(b"abc\x1b[D\x1b[Dx\r").0), "axbc");
        assert_eq!(line(read(b"abc\x1b[H\x1b[3~\r").0), "bc");
        assert_eq!(line(read(b"\x1b[A\r").0), "older");
        assert_eq!(line(read("é\r".as_bytes()).0), "é");
    }

    #[test]
    fn f_keys_are_not_home() {
        assert_eq!(line(read(b"ab\x1b[11~c\r").0), "abc");
    }

    #[test]
    fn a_lone_escape_does_not_eat_the_next_key_when_input_stops() {
        // With nothing after it, Escape is simply ignored.
        assert!(matches!(read(b"\x1b").0, ReadResult::Eof));
    }

    #[test]
    fn continuation_prompts_use_ps2() {
        let (result, shown) = read(b"echo 'a\rb'\r");
        assert_eq!(line(result), "echo 'a\nb'");
        assert!(shown.contains("\n> "), "{shown:?}");
    }

    #[test]
    fn control_keys_end_the_read() {
        assert!(matches!(read(b"ab\x03").0, ReadResult::Interrupted));
        assert!(matches!(read(b"\x04").0, ReadResult::Eof));
    }

    #[test]
    fn typing_during_the_width_query_is_kept() {
        let mut replay = Replay::default();
        replay.bytes[..2].copy_from_slice(b"ab");
        replay.length = 2;
        assert_eq!(replay.next(), Some(b'a'));
        assert_eq!(replay.next(), Some(b'b'));
        assert_eq!(replay.next(), None);
        assert!(reply_so_far_is_valid(b"\x1b[24;8"));
        assert!(!reply_so_far_is_valid(b"e"));
    }

    #[test]
    fn parses_the_cursor_position_reply() {
        assert_eq!(parse_cursor_reply(b"\x1b[24;132R"), Some(132));
        assert_eq!(parse_cursor_reply(b"junk"), None);
        assert_eq!(parse_cursor_reply(b"\x1b[24;0R"), None);
    }
}
