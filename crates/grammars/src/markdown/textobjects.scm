(section
  (atx_heading)
  (_)* @class.inside) @class.around

; `@function.*` is otherwise unused in Markdown. The editor reads this capture to
; offer a gutter fold on a fenced code block's opening fence, which no lexical
; test can do on its own: a delimiter line looks the same whether it opens or
; closes a block.
(fenced_code_block) @function.around
