//! Deterministic byte-mutation fuzzing for the `init:` configuration.

#[path = "../../compress/tests/support/fuzz.rs"]
mod fuzz;

use feos_init::{Config, AUTOSTART_MAX};
use fuzz::{fuzz_bytes, run_constrained, LimitAllocator};

#[global_allocator]
static ALLOCATOR: LimitAllocator = LimitAllocator;

const CONFIG: &str = "theme: xp-blue\ninit:\n  restart: \"always\"\n  max_restarts: 5\n  autostart:\n    - terminal\n    - files\n";

#[test]
fn config_parse_survives_mutated_yaml() {
    run_constrained(|| {
        fuzz_bytes(51, &[CONFIG.as_bytes().to_vec()], 4000, 8192, |input| {
            if let Ok(text) = core::str::from_utf8(input) {
                if let Ok(config) = Config::try_parse(text) {
                    assert!(config.autostart().len() <= AUTOSTART_MAX);
                }
            }
        });
    });
}
