[
  (paragraph)
  (indented_code_block)
  (pipe_table)
] @text

[
  (atx_heading)
  (setext_heading)
  (thematic_break)
] @title.markup

(setext_heading
  (paragraph) @title.markup)

[
  (list_marker_plus)
  (list_marker_minus)
  (list_marker_star)
  (list_marker_dot)
  (list_marker_parenthesis)
  (task_list_marker_unchecked)
  (task_list_marker_checked)
] @punctuation.list_marker.markup

[
  (block_quote_marker)
  (block_continuation)
] @punctuation.markup

(pipe_table_header
  "|" @punctuation.markup)

(pipe_table_row
  "|" @punctuation.markup)

(pipe_table_delimiter_row
  "|" @punctuation.markup)

(pipe_table_delimiter_cell
  "-" @punctuation.markup)

[
  (fenced_code_block_delimiter)
  (info_string)
] @punctuation.embedded.markup

; Footnotes are not in the grammar: a footnote definition whose text is one
; word reads as a link definition with that word for its destination.
((link_reference_definition
  (link_label) @_label) @link_text.markup
  (#not-match? @_label "^\\[\\^"))

((link_label) @label.markup
  (#match? @label.markup "^\\[\\^"))

(link_reference_definition
  (link_label) @_label
  (link_destination) @link_uri.markup
  (#not-match? @_label "^\\[\\^"))

(link_title) @string.markup

; A closing tag whose opening tag is in no HTML block (it was written inside a
; paragraph, or in a list item the Markdown grammar fails to parse) is an error
; to the HTML grammar, which leaves the tag name uncolored. The HTML layer
; paints the `</` and `>` over this, and the tags that follow in the block, so
; the block is painted only when no text sits between its tags.
((html_block) @tag
  (#match? @tag "^([\\s>]*</[A-Za-z][A-Za-z0-9-]*\\s*>)+([\\s>]*<[A-Za-z!/][^<>]*>)*[\\s>]*$"))
