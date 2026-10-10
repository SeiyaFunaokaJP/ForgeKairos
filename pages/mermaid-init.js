// mdBook renders ```mermaid fences as code blocks; this turns them into diagrams.
// Shared by the English and Japanese books (see `additional-js` in both book.toml).
(async () => {
    const blocks = document.querySelectorAll("code.language-mermaid");
    if (blocks.length === 0) {
        return;
    }
    const { default: mermaid } = await import(
        "https://cdn.jsdelivr.net/npm/mermaid@11.4.1/dist/mermaid.esm.min.mjs"
    );
    const html = document.documentElement.classList;
    const dark = ["coal", "navy", "ayu"].some((theme) => html.contains(theme));
    mermaid.initialize({ startOnLoad: false, theme: dark ? "dark" : "default" });
    for (const code of blocks) {
        const diagram = document.createElement("pre");
        diagram.className = "mermaid";
        diagram.textContent = code.textContent;
        code.parentElement.replaceWith(diagram);
    }
    await mermaid.run();
})();
