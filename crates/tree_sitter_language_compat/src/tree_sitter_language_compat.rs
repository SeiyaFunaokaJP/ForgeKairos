// The workspace's `[patch.crates-io]` points `tree-sitter-language` at this
// package instead of at the git revision that `tree-sitter` itself uses.
// `tree-sitter-mermaid` pins `tree-sitter-language = "=0.1.7"` while that git
// crate is 0.1.8, and cargo activates only one semver-compatible version per
// source, so patching straight to 0.1.8 leaves that pin unsatisfiable. Being
// version 0.1.7 and re-exporting the git crate, this package answers both
// `^0.1` and `=0.1.7` with one activation and keeps `LanguageFn` a single type
// across every grammar. It also deliberately omits the `links` key so that the
// git crate remains the only package claiming that native library.
//
// Delete this crate and repoint the patch at the git revision once the
// upstream grammar relaxes its pin to `^0.1`.

pub use tree_sitter_language_upstream::*;
