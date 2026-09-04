---
title: Markdown
description: "Configure Markdown language support in Zed, including language servers, formatting, and debugging."
---

# Markdown

Markdown support is available natively in Zed.

- Tree-sitter: [tree-sitter-markdown](https://github.com/tree-sitter-grammars/tree-sitter-markdown)
- Language Server: N/A

## Syntax Highlighting Code Blocks

Zed supports language-specific syntax highlighting of markdown code blocks by leveraging [tree-sitter language grammars](../extensions/languages.md#grammar). All [Zed supported languages](../languages.md), including those provided by official or community extensions, are available for use in markdown code blocks. All you need to do is provide a language name after the opening <kbd>```</kbd> code fence like so:

````python
```python
import functools as ft

@ft.lru_cache(maxsize=500)
def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)
```
````

## Configuration

### Format

Zed supports using Prettier to automatically re-format Markdown documents. You can trigger this manually via the {#action editor::Format} action or via the {#kb editor::Format} keyboard shortcut. Alternately, you can enable format on save.

Configure formatting in Settings ({#kb zed::OpenSettings}) under Languages > Markdown, or add to your settings file:

```json [settings]
  "languages": {
    "Markdown": {
      "format_on_save": "on"
    }
  },
```

### List Continuation

Zed automatically continues lists when you press Enter at the end of a list item. Supported list types:

- Unordered lists (`-`, `*`, or `+` markers)
- Ordered lists (numbers are auto-incremented)
- Task lists (`- [ ]` and `- [x]`)

Pressing Enter on an empty list item removes the marker and exits the list.

To disable this behavior, configure in Settings ({#kb zed::OpenSettings}) under Languages > Markdown, or add to your settings file:

```json [settings]
  "languages": {
    "Markdown": {
      "extend_list_on_newline": false
    }
  },
```

### List Indentation

Zed indents list items when you press Tab while the cursor is on a line containing only a list marker. This allows you to quickly create nested lists.

To disable this behavior, configure in Settings ({#kb zed::OpenSettings}) under Languages > Markdown, or add to your settings file:

```json [settings]
  "languages": {
    "Markdown": {
      "indent_list_on_tab": false
    }
  },
```

### Enter で行末スペース補完 (このフォーク独自)

このフォークが upstream に追加した設定です。CommonMark の hard break (行末に半角スペースを
2 つ置く書き方) を、手でタイプしなくても済むようにします。

`hard_break_on_newline` が有効な言語では、Enter で改行するときに、直前の行が空行・見出し・
テーブルの行・リスト項目の先頭・すでに行末が半角スペース 2 つまたは `\` である場合を除き、
行末へ半角スペース 2 つを自動的に補います。コードブロックやインラインコードの中では補いません。
Markdown での既定値は `true` です (他の言語は既定 `false`)。

設定 ({#kb zed::OpenSettings}) の Languages > Markdown、または settings ファイルで変更できます:

```json [settings]
  "languages": {
    "Markdown": {
      "hard_break_on_newline": false
    }
  },
```

{#kb markdown::NewlineInvertHardBreak} (既定は `shift-enter`) を押すと、その 1 回だけ
`hard_break_on_newline` と逆の動きで改行します。無効なファイルで一時的に hard break を
入れたいとき、または有効なファイルで hard break を入れずに改行したいときに使います。

Markdown ファイルを編集しているときはタブバーの下に表示される整形バー (Markdown 整形バー)
の右端にも、この設定を切り替えるスイッチ「Enter改行」があります。

### Trailing Whitespace

By default Zed will remove trailing whitespace on save. If you rely on invisible trailing whitespace being converted to `<br />` in Markdown files you can disable this behavior.

Configure in Settings ({#kb zed::OpenSettings}) under Languages > Markdown, or add to your settings file:

```json [settings]
  "languages": {
    "Markdown": {
      "remove_trailing_whitespace_on_save": false
    }
  },
```
