# Markdown and Mermaid

[日本語](ja/markdown.md)

ForgeKairos adds Markdown input helpers, a wider preview with more HTML support, image and
text color helpers, PDF export, and a structured editor for Mermaid diagrams. For Zed's
Markdown support, see [Zed's Markdown documentation](https://zed.dev/docs/languages/markdown).

## Input helpers

- Enter continues block quotes, bullet and numbered lists, and task lists; Tab indents a
  list item.
- In Markdown, Enter first adds the two trailing spaces of a hard line break
  (`hard_break_on_newline`, on for Markdown). Shift+Enter does the opposite once.
- Formatting actions: block quote, bold, italic, strikethrough, inline code, headings 1 to
  3, bullet list, numbered list, task list, link, horizontal rule and code block. `ctrl-b`,
  `ctrl-i` and `ctrl-shift-x` (`cmd` on macOS) apply bold, italic and strikethrough in
  Markdown files only.
- While a Markdown file is open, a formatting bar appears below the toolbar
  (`toolbar.markdown_format_bar`). It also inserts `<details>`, images and Mermaid
  diagrams, and sets text color, highlight and paragraph text size.
- Headings, fenced code blocks and `<details>` can be folded from the gutter, and all
  headings of a level can be folded at once.

## Preview

- The eye button opens the preview in the same pane; Ctrl+click (Cmd+click on macOS) opens
  it beside the editor.
- The preview uses the full width of the pane (`markdown_preview.limit_content_width` is
  off).
- A single newline is shown as a line break (`markdown_preview.soft_break_as_hard_break`).
- Table columns get a minimum width; a table scrolls sideways only when the columns do not
  fit.
- Supported HTML includes `<details>` and `<summary>` (click to open or close), `<br>`,
  `<div align>`, `<center>`, `<b>`, `<kbd>`, `<u>`, `<s>`, `<hr>`, `<sub>`, `<sup>` and
  `<a href>`. Links with `javascript:`, `data:` and `vbscript:` are refused.

## Images

`markdown: insert image` (also on the formatting bar) opens a dialog for the path, alternative text, alignment and size.
It writes `![alt](path)`, an `<img>` with width and height, or a `<div align>` around the
image, depending on what you set. Paths are relative to the Markdown file; an image from
outside the project is copied into an `images` folder. This also works on SSH and WSL
remotes.

- Pasting an image into a saved Markdown file saves it as
  `images/image-YYYYMMDD-HHMMSS.<ext>` and links it.
- Dropping a file from the operating system copies it; dropping one from the project panel
  links it.
- Paths are completed after `](`, `![](`, `src="` and `href="`.

## Tables

`markdown: edit table` opens the pipe table under the cursor (or a blank table) as a grid of
cells. You can insert, delete and move rows and columns, set each column's alignment, and
paste a range copied from a spreadsheet, which is spread across the table. **Update** writes
the table back as Markdown.

## Text color, highlight and size

- Text color writes `<span style="color: …">` (8 colors).
- Highlight writes `<mark>` (yellow) or `<mark style="background-color: …">`.
- Paragraph text size writes `<span style="font-size: …">` around a paragraph.

These are refused, with a message, where they would break the markup (inside tags, comments
or code, or across links and table cells). GitHub drops `style` attributes, so colors and
sizes do not show there.

## Export to PDF

`markdown: export to pdf` (from the toolbar, the preview or the tab's right-click menu)
writes a PDF with the same name next to the saved Markdown file. It uses Microsoft Edge in
headless mode, or `pandoc` from `PATH` when Edge is not found. The page is A4, Mermaid
diagrams are included, and text colors are kept.

## Mermaid

Mermaid diagrams are drawn by ForgeKairos itself (no JavaScript), in the Markdown preview and
in the agent panel.

- **Insert Mermaid Diagram** (formatting bar, right-click menu, or the `mermaid-*` snippets)
  inserts a diagram of a chosen type.
- **Edit Mermaid Diagram…** (right-click, the **Edit** button on a diagram in the preview, or
  the formatting bar) opens a structured editor for the diagram at or near the cursor, as a
  tab beside your files: the form on the left and a large preview on the right (drag the line
  between them; in a narrow pane they are stacked). The preview enlarges or shrinks the
  diagram to fill the pane, or shows it at the size you pick with the zoom buttons or
  Ctrl+scroll (with scrolling when it is larger than the pane). Opening the same diagram again brings its tab
  forward. **Apply** (Ctrl+Enter, or saving the tab) writes the diagram back as one undo step
  and keeps the tab open; a dot on the tab marks edits not yet applied, and closing the tab
  asks what to do with them. If the diagram is changed in the document while the tab has no
  edits, the tab follows it; with edits, the tab warns and does not write over it unless you
  choose to overwrite. Lines the editor does not understand are kept as they are; syntax it
  cannot reproduce makes it refuse to open, with the reason. These tabs are not restored when
  ForgeKairos restarts.
- In the preview, double-click a label to edit it in flowcharts, ER diagrams, state,
  class, sequence, pie and Gantt diagrams.
- A diagram that makes the renderer fail shows an error in its place instead of stopping
  ForgeKairos.

The structured editor supports 26 diagram types: flowchart, sequence, class, state,
entity relationship, Gantt, pie, GitGraph, mindmap, timeline, user journey, quadrant chart,
XY chart, radar, requirement, C4, block, Sankey, packet, Venn, Kanban, treemap,
architecture, tree view, Ishikawa and event modeling.
