((html_tag) @injection.content
  (#set! injection.language "html")
  (#set! injection.combined))

; Only what the highlights query paints as math: display math, and inline math
; with no space just inside either `$`, so `$5 and $10` stays prose.
((latex_block) @injection.content
  (#match? @injection.content "^\\$\\$")
  (#set! injection.language "latex"))

((latex_block) @injection.content
  (#match? @injection.content "^\\$[^$\\s0-9]([^$]*[^$\\s])?\\$$")
  (#set! injection.language "latex"))
