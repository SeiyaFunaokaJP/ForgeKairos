(emphasis) @emphasis.markup

(strong_emphasis) @emphasis.strong.markup

(code_span) @text.literal.markup

; No bundled theme has a `strikethrough` key, and a theme cannot draw the line
; itself, so struck text falls back to the dimmed comment color.
(strikethrough) @comment.strikethrough.markup @strikethrough.markup

[
  (inline_link)
  (collapsed_reference_link)
  (full_reference_link)
  (image)
  (link_label)
] @link_text.markup

; Footnote references (`[^1]`) are not in the grammar; they read as shortcut
; links.
((shortcut_link) @link_text.markup
  (#not-match? @link_text.markup "^\\[\\^"))

((shortcut_link) @label.markup
  (#match? @label.markup "^\\[\\^"))

((link_text) @link_text.markup
  (#not-match? @link_text.markup "^\\^"))

(inline_link
  [
    "("
    ")"
  ] @link_uri.markup)

(image
  [
    "("
    ")"
  ] @link_uri.markup)

[
  (link_destination)
  (uri_autolink)
  (email_autolink)
] @link_uri.markup

(link_title) @string.markup

[
  (backslash_escape)
  (hard_line_break)
] @string.escape

[
  (entity_reference)
  (numeric_character_reference)
] @string.special

((latex_block) @text.literal.markup
  (#match? @text.literal.markup "^\\$\\$"))

; The grammar also reads `$5 and $10` as inline math. Pandoc's rule keeps
; prices out: no space just inside either `$`. Its other half, no digit right
; after the closing `$`, is outside the node, so math that starts with a digit
; is left as prose instead.
((latex_block) @text.literal.markup
  (#match? @text.literal.markup "^\\$[^$\\s0-9]([^$]*[^$\\s])?\\$$"))
