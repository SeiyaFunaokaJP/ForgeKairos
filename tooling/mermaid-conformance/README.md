# Mermaid conformance check

Asks the official Mermaid parser whether diagram sources are valid.

Zed renders Mermaid with merman, an independent Rust implementation. The text
in a Markdown file is plain Mermaid, read by GitHub, VS Code and others with
the official implementation, so what this fork writes out — the structured
diagram editors, the insert templates, the snippets — has to be valid there
too, not only in merman. This tool is the reference for that.

It is for development only. Nothing here is built into or shipped with the
application, and the application never runs it. See `THIRD_PARTY_NOTICES.md`
at the repository root for versions and licenses.

## Setup

Needs Node.js 22 or later and network access to the npm registry, once:

```sh
cd tooling/mermaid-conformance
npm ci --ignore-scripts
```

The versions are pinned in `package-lock.json`. Mermaid is pinned to the
version merman targets; change both together.

## Use

```sh
node check.mjs diagrams.json
```

`diagrams.json` is a list of `{"name": "...", "source": "..."}`. The output is
one JSON object per line:

```json
{"name":"flowchart","valid":true,"diagramType":"flowchart-v2"}
{"name":"gitGraph LR","valid":false,"error":"Parsing failed: ..."}
```

Only parsing is checked, not how a diagram is drawn.

## What to do with a difference

- Official Mermaid rejects it, merman accepts it: do not write it out, and do
  not add leniency for it. A diagram that only draws in Zed breaks elsewhere.
- Official Mermaid accepts it, merman rejects it or cannot draw it: merman
  falls short. Work around it on the fork's side if that is safe, otherwise
  extend merman under `third_party/` and record the change in its
  `FORK_CHANGES.md`.
