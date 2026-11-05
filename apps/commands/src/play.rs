//! `play`: plays a sound file from the filesystem and waits for it to end.

use feos_app::{App, Context, EXIT_KILLED};
use feos_console::{eprintln, println};

use crate::audio_worker::{play_media, AudioFailure, AudioPipe, Finished, Source};

/// The `play` command.
pub struct Play;

impl App for Play {
    fn name(&self) -> &'static str {
        "play"
    }

    fn usage(&self) -> &'static str {
        "play <file>"
    }

    fn description(&self) -> &'static str {
        "play a WAV, AIFF, AU, MP3, FLAC, Vorbis or AAC file"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let path = context.args.trim();
        if path.is_empty() {
            eprintln!("usage: {}", self.usage());
            return 2;
        }
        let Some(fs) = context.platform.filesystem else {
            eprintln!("play: no filesystem is available on this board");
            return 1;
        };
        let Some(node) = fs.resolve(path).filter(|&node| fs.is_file(node)) else {
            eprintln!("play: {path}: no such file");
            return 1;
        };
        // This process is the task, so the pipe is only here to hold reports.
        let pipe = AudioPipe::new();
        let identity = pipe.claim();
        pipe.open(node, Source::File, false);
        let media = pipe.media();
        let _ = pipe.take_open();
        println!("playing {path}");
        let finished = play_media(
            &pipe,
            context.platform,
            fs,
            node,
            Source::File,
            identity,
            media,
            false,
        );
        match finished {
            Finished::Ended => {
                println!("finished {path}");
                0
            }
            Finished::Interrupted => EXIT_KILLED,
            Finished::Failed(failure) => {
                match failure {
                    AudioFailure::Mixer(error) => eprintln!("play: no sound output: {error:?}"),
                    AudioFailure::Codec(error) => eprintln!("play: {path}: {error}"),
                    AudioFailure::Unreadable => eprintln!("play: {path}: cannot read it"),
                    AudioFailure::NoTrack => eprintln!("play: {path}: no sound in it"),
                }
                1
            }
        }
    }
}
