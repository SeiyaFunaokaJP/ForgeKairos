// Asks the official Mermaid parser whether each diagram source is valid.
//
// usage: node check.mjs <file.json>
//   input:  [{"name": "...", "source": "..."}, ...]
//   output: one JSON line per diagram: {"name", "valid", "diagramType" | "error"}
//
// Mermaid sanitizes text through DOMPurify while parsing, which needs a DOM.
import { readFileSync } from "node:fs";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<!DOCTYPE html><html><body></body></html>", {
  pretendToBeVisual: true,
});
for (const name of ["window", "document", "Element", "SVGElement", "Node"]) {
  globalThis[name] = dom.window[name] ?? dom.window;
}
globalThis.window = dom.window;
globalThis.document = dom.window.document;

const { default: mermaid } = await import("mermaid");
mermaid.initialize({ startOnLoad: false, securityLevel: "strict" });

const diagrams = JSON.parse(readFileSync(process.argv[2], "utf8"));
for (const { name, source } of diagrams) {
  try {
    const result = await mermaid.parse(source);
    console.log(
      JSON.stringify({ name, valid: true, diagramType: result.diagramType }),
    );
  } catch (error) {
    const message = String(error?.message ?? error)
      .split("\n")
      .slice(0, 4)
      .join(" | ");
    console.log(JSON.stringify({ name, valid: false, error: message }));
  }
}
