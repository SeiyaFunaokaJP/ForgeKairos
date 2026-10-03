# ForgeKairos documentation site

The source of <https://seiyafunaokajp.github.io/ForgeKairos/>: the features ForgeKairos adds
to Zed. Zed's own documentation stays in `docs/`.

- `src/` is the English book (`book.toml`), `src/ja/` the Japanese one (`ja/book.toml`).
  Both have the same chapters under the same file names, and each chapter links to its
  counterpart at the top.
- Build locally with [mdBook](https://github.com/rust-lang/mdBook) 0.4.37, English first
  (it empties `book/`):

  ```sh
  mdbook build pages
  mdbook build pages/ja
  ```

  The site is written to `pages/book/` (Japanese under `pages/book/ja/`).
- `.github/workflows/pages.yml` publishes it when `pages/` changes on `main`, or when run by
  hand.

GitHub Pages is not available while the repository is private; once it is public, enable it
in Settings → Pages → Source = GitHub Actions.
