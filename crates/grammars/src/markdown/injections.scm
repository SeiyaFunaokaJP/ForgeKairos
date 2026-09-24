(fenced_code_block
  (info_string
    (language) @injection.language)
  (code_fence_content) @injection.content)

; In a block quote or list, the `>` markers and indentation that start the later
; lines of a paragraph are children of its inline node, not its text.
((inline) @injection.content
  (#set! injection.language "markdown-inline")
  (#set! injection.exclude-children))

((pipe_table_cell) @injection.content
  (#set! injection.language "markdown-inline"))

; A blank line ends an HTML block, so `<details>`, the Markdown inside it and
; `</details>` are separate blocks. Parsed one at a time, a lone closing tag is
; an error to the HTML grammar and gets no color; parsed as one document, it
; closes the element its opening block started.
; In a block quote, the `>` that starts each later line of the block is a child
; of the block; HTML would read it as a stray bracket.
((html_block) @injection.content
  (#set! injection.language "html")
  (#set! injection.combined)
  (#set! injection.exclude-children))

((minus_metadata) @injection.content
  (#set! injection.language "yaml"))

((plus_metadata) @injection.content
  (#set! injection.language "toml"))
