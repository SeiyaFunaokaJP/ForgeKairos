(fenced_code_block
  (info_string
    (language) @injection.language)
  (code_fence_content) @injection.content)

((inline) @injection.content
  (#set! injection.language "markdown-inline"))

((pipe_table_cell) @injection.content
  (#set! injection.language "markdown-inline"))

; A blank line ends an HTML block, so `<details>`, the Markdown inside it and
; `</details>` are separate blocks. Parsed one at a time, a lone closing tag is
; an error to the HTML grammar and gets no color; parsed as one document, it
; closes the element its opening block started.
((html_block) @injection.content
  (#set! injection.language "html")
  (#set! injection.combined))

((minus_metadata) @injection.content
  (#set! injection.language "yaml"))

((plus_metadata) @injection.content
  (#set! injection.language "toml"))
