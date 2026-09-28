# merman-render (Zed fork)

This directory is `merman-render` **0.8.0-alpha.5** from crates.io
(<https://crates.io/crates/merman-render/0.8.0-alpha.5>, package checksum
`cd17bfdd6866df20f34a8697eba00e0b86efa734de0b6a629b07a755d748fd68`),
copied as published — `LICENSE-MIT`, `LICENSE-APACHE`, `THIRD_PARTY_LICENSES`
and `THIRD_PARTY_NOTICES.md` included — with the change below. The workspace
uses it through `[patch.crates-io]` in the root `Cargo.toml`; it is listed in
the workspace's `exclude`, so it is neither linted nor formatted with the rest
of the tree.

## Change

`src/svg/fallback/html.rs`, `wrap_html_line_to_width`: the foreignObject
fallback text (the native SVG text that replaces an HTML label for resvg) is
wrapped at the break opportunities of `text::html_break_spaces_segments`
(UAX #14, with Chromium's rule for `/` and `break-spaces` spaces) instead of
`DeterministicTextMeasurer::split_line_to_words`, which splits at spaces only.
The layout already measures HTML labels with those opportunities, so a long
label without spaces — Japanese, Chinese — got a box sized for several lines
but was drawn on one line running out of it. `src/text.rs` re-exports
`html_break_spaces_segments` to the crate for this.

The published crate's own tests (`--lib`, 1124 tests) pass with the change,
as they do without it.

## Updating

Replace this directory with the new published version, reapply the change
above, and keep this file. Drop the patch once merman wraps the fallback text
the same way.
