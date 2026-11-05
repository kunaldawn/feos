//! `coreutils`: list or describe the POSIX utilities built into the shell.

use feos_app::{App, Context};
use feos_console::{eprintln, print, println};

/// The `coreutils` application.
pub struct CoreUtils;

impl App for CoreUtils {
    fn name(&self) -> &'static str {
        "coreutils"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["utilities", "utils"]
    }

    fn usage(&self) -> &'static str {
        "coreutils [command]"
    }

    fn description(&self) -> &'static str {
        "list POSIX utilities available in the terminal"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(name) = context.argv.first().copied() else {
            println!("coreutils ({} commands):", feos_coreutils::COMMANDS.len());
            for spec in feos_coreutils::COMMANDS {
                println!("  {:<18} {}", spec.usage, spec.description);
            }
            return 0;
        };

        match feos_coreutils::command(name) {
            Some(spec) => {
                print!(
                    "{}: {}\nusage: {}\n",
                    spec.name, spec.description, spec.usage
                );
                if !spec.aliases.is_empty() {
                    print!("aliases:");
                    for alias in spec.aliases {
                        print!(" {alias}");
                    }
                    println!();
                }
                0
            }
            None => {
                eprintln!("coreutils: unknown command '{name}'");
                1
            }
        }
    }
}
