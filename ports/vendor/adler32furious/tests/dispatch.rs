//! Checks which implementation dispatch picks, for CI runs on known CPUs:
//! emulated ones, and runners whose CPU features are known.

/// Set by CI to the name of the implementation this CPU should get.
const EXPECT: &str = "ADLER32FURIOUS_EXPECT";

#[test]
fn picks_the_expected_implementation() {
    // CI sets it to "" for runners whose CPU it does not pin down.
    let want = std::env::var(EXPECT).unwrap_or_default();
    if want.is_empty() {
        return;
    }
    let names: Vec<_> = adler32furious::implementations()
        .map(|imp| imp.name())
        .collect();
    assert_eq!(names[0], want, "{EXPECT}; available: {names:?}");
}
