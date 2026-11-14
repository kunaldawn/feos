stringdex
=========

This crate is utterly experimental. It will eat your data. Do not use it yet.
Read [HACKING.md][] for info on how it works.

The demo is hosted at <https://stringdex-d3c2b2.gitlab.io/>.

You ever noticed how, when you click the search box in rustdoc,
it locks up the tab for a few seconds? *This is supposed to solve that.*

[HACKING.md]: ./HACKING.md

Features
--------

When it's done, this will be an easy-to-deploy fuzzy lookup
lookup database, built using search trees. Most of these
features aren't implemented.

Major non-features, not even planned:

- Features that aren't useful for rustdoc aren't planned.
  - You can't store more than 2<sup>32</sup>-1 rows. The rust compiler can't
    handle that many things, since DefIndex is a u32.
- This is a name-searcher. Not a full-text searcher.
- I'm probably leaving a lot of performance on the table, because I'm not
  interested in optimizations that:
  - Are `unsafe`.
  - Require third-party dependencies (first-party deps, like `stacker`, are okay).
  - Prevent `file://` URLs from working.

Features:

- When given a reasonable chance to do so, I try to find ways
  to make the on-disk index smaller. I try to pick compression
  techniques that allow me to keep the data in its compressed form,
  even while searching it, to save memory in the browser (chewing through
  CPU means it takes longer, chewing through RAM means the tab becomes more
  likely to crash).
- You can store multiple indexes in the same database, which
  will be used for rustdoc's two search modes.
- You'll be able to store two kind of data: single-byte columns,
  which are fixed-width, and multi-byte columns, which are length-delimited.
