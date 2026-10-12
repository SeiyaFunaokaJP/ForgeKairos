#!/usr/bin/env python3
"""
Turn script/terms/notice.<lang>.txt into the RTF the installers show
(Inno Setup's information page on Windows, dmg-license on macOS).

The sources are plain text: "# " starts the title, "## " a heading, and
blank lines separate paragraphs. RTF is written with \\uN? escapes so the
files stay ASCII and render the same in both installers.
"""

from pathlib import Path

TERMS_DIR = Path(__file__).resolve().parent / "terms"

# (font, charset) per language, so RichEdit picks a font with the right glyphs.
FONTS = {
    "en": ("Segoe UI", 0),
    "ja": ("Yu Gothic UI", 128),
    "zh-cn": ("Microsoft YaHei UI", 134),
}


def escape(text: str) -> str:
    out = []
    for character in text:
        code = ord(character)
        if character in "\\{}":
            out.append("\\" + character)
        elif 0x20 <= code < 0x80:
            out.append(character)
        else:
            # RTF takes signed 16-bit UTF-16 code units.
            encoded = character.encode("utf-16-le")
            for index in range(0, len(encoded), 2):
                unit = int.from_bytes(encoded[index : index + 2], "little", signed=True)
                out.append(f"\\u{unit}?")
    return "".join(out)


def to_rtf(source: str, font: str, charset: int) -> str:
    lines = [
        "{\\rtf1\\ansi\\deff0",
        f"{{\\fonttbl{{\\f0\\fnil\\fcharset{charset} {font};}}}}",
        "\\viewkind4\\uc1",
    ]
    for block in source.strip().split("\n\n"):
        block = " ".join(line.strip() for line in block.splitlines())
        if block.startswith("## "):
            lines.append(f"\\pard\\sb120\\sa80\\b\\fs22 {escape(block[3:])}\\b0\\par")
        elif block.startswith("# "):
            lines.append(f"\\pard\\sa120\\b\\fs26 {escape(block[2:])}\\b0\\par")
        else:
            lines.append(f"\\pard\\sa120\\fs18 {escape(block)}\\par")
    lines.append("}")
    return "\n".join(lines) + "\n"


def main() -> None:
    for language, (font, charset) in FONTS.items():
        source = (TERMS_DIR / f"notice.{language}.txt").read_text(encoding="utf-8")
        target = TERMS_DIR / f"notice.{language}.rtf"
        target.write_text(to_rtf(source, font, charset), encoding="ascii", newline="\n")
        print(f"wrote {target}")


if __name__ == "__main__":
    main()
