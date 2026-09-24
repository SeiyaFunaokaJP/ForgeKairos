(emphasis) @emphasis.markup

(strong_emphasis) @emphasis.strong.markup

(code_span) @text.literal.markup

(strikethrough) @strikethrough.markup

[
  (inline_link)
  (shortcut_link)
  (collapsed_reference_link)
  (full_reference_link)
  (image)
  (link_text)
  (link_label)
] @link_text.markup

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

; Display math only: the grammar also reads `$5 and $10` as inline math, which
; would paint prices as formulas.
((latex_block) @text.literal.markup
  (#match? @text.literal.markup "^\\$\\$"))
