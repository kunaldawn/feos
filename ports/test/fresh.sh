# Sourced (not run) by every script that runs cargo on the base. Cargo decides
# a path crate (and a build script's rerun-if-changed inputs) is fresh by
# comparing source mtimes with its last build, and `git archive | tar -x` gives
# each file its commit's committer time. So after a build at a newer commit, a
# base made from an older one (an earlier checkpoint, or going back to HEAD from
# a later ref) looks older than everything in the target dir, and cargo would
# reuse the other commit's kernel and user programs.
#
# What was last built in CARGO_TARGET_DIR is kept per piece (arm, x64,
# user-arm, user-x64: the ports' artifacts are mostly separate) in its
# .ports-base-built-PIECE, as a commit. `ports_base_sync PIECE...`, before the
# first cargo command, touches every file of the base when any PIECE's commit
# differs from the base's .ports-base-ref (once per base: the base's
# .ports-base-touched says it's done), so all of it is newer than any earlier
# build. `ports_base_built PIECE`, after that piece built successfully, records
# the commit. A failed build records nothing, so a fresh base of the same ref is
# touched again and nothing half-built at another commit is taken as fresh.
#
# Needs base and CARGO_TARGET_DIR set.

ports_base_sync() {
    _ref=$(cat "$base/.ports-base-ref")
    [ -f "$base/.ports-base-touched" ] && return 0
    for _piece in "$@"; do
        _built=$(cat "$CARGO_TARGET_DIR/.ports-base-built-$_piece" 2>/dev/null || echo none)
        [ "$_built" = "$_ref" ] && continue
        echo "ports-base: $_piece last built at $_built, base is $_ref; touching the base"
        find "$base" -path "$base/kernels" -prune -o -exec touch -c {} +
        : > "$base/.ports-base-touched"
        return 0
    done
}

ports_base_built() {
    mkdir -p "$CARGO_TARGET_DIR"
    cat "$base/.ports-base-ref" > "$CARGO_TARGET_DIR/.ports-base-built-$1"
}
