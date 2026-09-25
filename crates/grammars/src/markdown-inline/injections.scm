((html_tag) @injection.content
  (#set! injection.language "html")
  (#set! injection.combined))

; Only what the highlights query paints as math: display math, and inline math
; with no space just inside either `$` and no digit right after the closing
; one, so `$5 and $10` and `$5/$10` stay prose.
((latex_block) @injection.content
  (#match? @injection.content "^\\$\\$")
  (#set! injection.language "latex"))

((latex_block) @injection.content
  (#match? @injection.content "^\\$[^$\\s]([^$]*[^$\\s])?\\$$")
  (#not-followed-by? @injection.content "0123456789")
  (#set! injection.language "latex"))
