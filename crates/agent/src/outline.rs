use anyhow::Result;
use gpui::{AsyncApp, Entity};
use language::{Buffer, OutlineItem};
use regex::Regex;
use std::fmt::Write;
use text::Point;

/// For files over this size, instead of reading them (or including them in context),
/// we automatically provide the file's symbol outline instead, with line numbers.
pub const AUTO_OUTLINE_SIZE: usize = 16384;

/// Result of getting buffer content, which can be either full content or an outline.
pub struct BufferContent {
    /// The actual content (either full text, a symbol outline, or a
    /// truncated fallback — see `is_synthetic`).
    pub text: String,
    /// `true` when `text` is not the file's full content — either a symbol
    /// outline or the truncated first-1KB fallback used when no outline is
    /// available. Callers that prefix line numbers to file content must
    /// skip prefixing in this case, because line numbers in `text` would
    /// not correspond to the file's real line numbers.
    pub is_synthetic: bool,
}

/// Returns either the full content of a buffer or its outline, depending on size.
/// For files larger than AUTO_OUTLINE_SIZE, returns an outline with a header.
/// For smaller files, returns the full content.
pub async fn get_buffer_content_or_outline(
    buffer: Entity<Buffer>,
    path: Option<&str>,
    cx: &AsyncApp,
) -> Result<BufferContent> {
    let file_size = buffer.read_with(cx, |buffer, _| buffer.text().len());

    if file_size > AUTO_OUTLINE_SIZE {
        // For large files, use outline instead of full content
        // Wait until the buffer has been fully parsed, so we can read its outline
        buffer
            .read_with(cx, |buffer, _| buffer.parsing_idle())
            .await;

        let outline_items = buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            snapshot
                .outline(None)
                .items
                .into_iter()
                .map(|item| item.to_point(&snapshot))
                .collect::<Vec<_>>()
        });

        // If no outline exists, fall back to first 1KB so the agent has some context.
        // This is reported as `is_synthetic: true` because the returned text is not
        // the file's full content — it has a synthetic header and is truncated — so
        // callers must not attach real-file line numbers to it.
        if outline_items.is_empty() {
            let text = buffer.read_with(cx, |buffer, _| {
                let snapshot = buffer.snapshot();
                let len = snapshot.len().min(snapshot.as_rope().floor_char_boundary(1024));
                let content = snapshot.text_for_range(0..len).collect::<String>();
                if let Some(path) = path {
                    format!("# First 1KB of {path} (file too large to show full content, and no outline available)\n\n{content}")
                } else {
                    format!("# First 1KB of file (file too large to show full content, and no outline available)\n\n{content}")
                }
            });

            return Ok(BufferContent {
                text,
                is_synthetic: true,
            });
        }

        let outline_text = render_outline(outline_items, None, 0, usize::MAX).await?;

        let text = if let Some(path) = path {
            format!("# File outline for {path}\n\n{outline_text}",)
        } else {
            format!("# File outline\n\n{outline_text}",)
        };
        Ok(BufferContent {
            text,
            is_synthetic: true,
        })
    } else {
        // File is small enough, return full content
        let text = buffer.read_with(cx, |buffer, _| buffer.text());
        Ok(BufferContent {
            text,
            is_synthetic: false,
        })
    }
}

/// Like [`get_buffer_content_or_outline`], but for a file attached to a chat
/// message rather than read by a tool.
///
/// Files over `inline_max_bytes` (or every file, when it is `None` because the
/// user asked for a summary) are sent as an outline kept within
/// `outline_max_bytes`, preceded by a note that tells the model how large the
/// file is and to read only the ranges it needs.
pub async fn get_attachment_content(
    buffer: Entity<Buffer>,
    path: Option<&str>,
    inline_max_bytes: Option<usize>,
    outline_max_bytes: usize,
    cx: &AsyncApp,
) -> Result<BufferContent> {
    let (file_size, line_count) = buffer.read_with(cx, |buffer, _| {
        (buffer.len(), buffer.max_point().row as usize + 1)
    });
    if inline_max_bytes.is_some_and(|inline_max_bytes| file_size <= inline_max_bytes) {
        let text = buffer.read_with(cx, |buffer, _| buffer.text());
        return Ok(BufferContent {
            text,
            is_synthetic: false,
        });
    }

    buffer
        .read_with(cx, |buffer, _| buffer.parsing_idle())
        .await;
    let outline_items = buffer.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        snapshot
            .outline(None)
            .items
            .into_iter()
            .map(|item| item.to_point(&snapshot))
            .collect::<Vec<_>>()
    });

    let mut text = attachment_note(path, line_count, file_size);
    if outline_items.is_empty() {
        let excerpt = buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let len = snapshot
                .len()
                .min(snapshot.as_rope().floor_char_boundary(1024));
            snapshot.text_for_range(0..len).collect::<String>()
        });
        text.push_str("No outline is available for this file; its first 1KB follows.\n\n");
        text.push_str(&excerpt);
    } else {
        text.push('\n');
        text.push_str(&render_outline_within(&outline_items, outline_max_bytes));
    }
    Ok(BufferContent {
        text,
        is_synthetic: true,
    })
}

fn attachment_note(path: Option<&str>, line_count: usize, file_size: usize) -> String {
    let subject = match path {
        Some(path) => format!("`{path}`"),
        None => "This file".to_string(),
    };
    format!(
        "# {subject} ({line_count} lines, {:.1} KB). Its full content was not attached. \
         Read only the line ranges you need with your file-reading tool \
         (for example, lines 120-180 for a symbol shown as [L120-180]).\n",
        file_size as f64 / 1024.0
    )
}

/// Renders an outline in at most `max_bytes`, dropping the deepest symbols
/// first and, when even the top-level ones do not fit, the last of those, with
/// a line saying how many were left out.
pub fn render_outline_within(items: &[OutlineItem<Point>], max_bytes: usize) -> String {
    let deepest = items.iter().map(|item| item.depth).max().unwrap_or(0);
    for depth_limit in (0..=deepest).rev() {
        let kept = items.iter().filter(|item| item.depth <= depth_limit);
        let mut output = String::new();
        render_entries(&mut output, kept.cloned());
        let omitted = items.iter().filter(|item| item.depth > depth_limit).count();
        let note = omitted_symbols_note(omitted, "deeper ");
        if output.len() + note.len() <= max_bytes {
            output.push_str(&note);
            return output;
        }
    }

    let mut output = String::new();
    let top_level = items
        .iter()
        .filter(|item| item.depth == 0)
        .collect::<Vec<_>>();
    let mut kept = 0;
    for item in &top_level {
        let mut line = String::new();
        render_entries(&mut line, [(*item).clone()]);
        let remaining = items.len() - kept - 1;
        if output.len() + line.len() + omitted_symbols_note(remaining, "").len() > max_bytes {
            break;
        }
        output.push_str(&line);
        kept += 1;
    }
    output.push_str(&omitted_symbols_note(items.len() - kept, ""));
    output
}

fn omitted_symbols_note(omitted: usize, qualifier: &str) -> String {
    if omitted == 0 {
        String::new()
    } else {
        format!("[… {omitted} {qualifier}symbols omitted to fit the outline budget]\n")
    }
}

async fn render_outline(
    items: impl IntoIterator<Item = OutlineItem<Point>>,
    regex: Option<Regex>,
    offset: usize,
    results_per_page: usize,
) -> Result<String> {
    let mut items = items.into_iter().skip(offset);

    let entries = items
        .by_ref()
        .filter(|item| {
            regex
                .as_ref()
                .is_none_or(|regex| regex.is_match(&item.text))
        })
        .take(results_per_page)
        .collect::<Vec<_>>();
    let has_more = items.next().is_some();

    let mut output = String::new();
    let entries_rendered = render_entries(&mut output, entries);

    // Calculate pagination information
    let page_start = offset + 1;
    let page_end = offset + entries_rendered;
    let total_symbols = if has_more {
        format!("more than {}", page_end)
    } else {
        page_end.to_string()
    };

    // Add pagination information
    if has_more {
        writeln!(&mut output, "\nShowing symbols {page_start}-{page_end} (there were more symbols found; use offset: {page_end} to see next page)",
        )
    } else {
        writeln!(
            &mut output,
            "\nShowing symbols {page_start}-{page_end} (total symbols: {total_symbols})",
        )
    }
    .ok();

    Ok(output)
}

fn render_entries(
    output: &mut String,
    items: impl IntoIterator<Item = OutlineItem<Point>>,
) -> usize {
    let mut entries_rendered = 0;

    for item in items {
        // Indent based on depth ("" for level 0, "  " for level 1, etc.)
        for _ in 0..item.depth {
            output.push(' ');
        }
        output.push_str(&item.text);

        // Add position information - convert to 1-based line numbers for display
        let start_line = item.range.start.row + 1;
        let end_line = item.range.end.row + 1;

        if start_line == end_line {
            writeln!(output, " [L{}]", start_line).ok();
        } else {
            writeln!(output, " [L{}-{}]", start_line, end_line).ok();
        }
        entries_rendered += 1;
    }

    entries_rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::Project;
    use settings::SettingsStore;

    #[gpui::test]
    async fn test_large_file_fallback_to_subset(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;

        let content = "⚡".repeat(100 * 1024); // 100KB
        let content_len = content.len();
        let buffer = project
            .update(cx, |project, cx| project.create_buffer(None, true, cx))
            .await
            .expect("failed to create buffer");

        buffer.update(cx, |buffer, cx| buffer.set_text(content, cx));

        let result = cx
            .spawn(|cx| async move { get_buffer_content_or_outline(buffer, None, &cx).await })
            .await
            .unwrap();

        // Should contain some of the actual file content
        assert!(
            result.text.contains("⚡⚡⚡⚡⚡⚡⚡"),
            "Result did not contain content subset"
        );

        // Should be marked synthetic: the returned text is not the file's full
        // content (it's a truncated first-1KB fallback with a synthetic header), so
        // callers must treat it the same as the symbol-outline case and not attach
        // real-file line numbers to it.
        assert!(
            result.is_synthetic,
            "Truncated fallback should be reported as synthetic so callers skip line numbering"
        );

        // Should be reasonably sized (much smaller than original)
        assert!(
            result.text.len() < 50 * 1024,
            "Result size {} should be smaller than 50KB",
            result.text.len()
        );

        // Should be significantly smaller than the original content
        assert!(
            result.text.len() < content_len / 10,
            "Result should be much smaller than original content"
        );
    }

    fn outline_item(depth: usize, text: &str, rows: std::ops::Range<u32>) -> OutlineItem<Point> {
        OutlineItem {
            depth,
            range: Point::new(rows.start, 0)..Point::new(rows.end, 0),
            text: text.to_string().into(),
            ..Default::default()
        }
    }

    fn nested_outline() -> Vec<OutlineItem<Point>> {
        let mut items = Vec::new();
        for index in 0..20u32 {
            let start = index * 100;
            items.push(outline_item(
                0,
                &format!("impl Type{index}"),
                start..start + 99,
            ));
            for method in 0..5u32 {
                let row = start + 1 + method * 10;
                items.push(outline_item(
                    1,
                    &format!("fn method_{index}_{method}"),
                    row..row + 9,
                ));
            }
        }
        items
    }

    #[test]
    fn test_outline_within_budget_is_unchanged() {
        let items = nested_outline();
        let output = render_outline_within(&items, usize::MAX);
        assert!(output.contains("impl Type0 [L1-100]"), "{output}");
        assert!(output.contains(" fn method_19_4 [L1942-1951]"), "{output}");
        assert!(!output.contains("omitted"), "{output}");
    }

    #[test]
    fn test_outline_drops_deeper_symbols_first() {
        let items = nested_outline();
        let full = render_outline_within(&items, usize::MAX);
        let budget = full.len() / 2;
        let output = render_outline_within(&items, budget);

        assert!(output.len() <= budget, "{} > {budget}", output.len());
        for index in 0..20 {
            assert!(output.contains(&format!("impl Type{index} ")), "{output}");
        }
        assert!(!output.contains("fn method_"), "{output}");
        assert!(
            output.ends_with("[… 100 deeper symbols omitted to fit the outline budget]\n"),
            "{output}"
        );
    }

    #[test]
    fn test_outline_truncates_top_level_symbols_last() {
        let items = nested_outline();
        let output = render_outline_within(&items, 200);

        assert!(output.len() <= 200, "{} > 200", output.len());
        assert!(output.starts_with("impl Type0 [L1-100]\n"), "{output}");
        assert!(!output.contains("impl Type19"), "{output}");
        let kept = output
            .lines()
            .filter(|line| line.starts_with("impl"))
            .count();
        assert!(
            output.ends_with(&format!(
                "[… {} symbols omitted to fit the outline budget]\n",
                items.len() - kept
            )),
            "{output}"
        );
    }

    #[test]
    fn test_outline_truncation_keeps_multibyte_lines_whole() {
        let items = (0..50u32)
            .map(|index| outline_item(0, &format!("関数_{index}_日本語の名前"), index..index))
            .collect::<Vec<_>>();
        let output = render_outline_within(&items, 300);
        assert!(output.len() <= 300, "{} > 300", output.len());
        for line in output.lines() {
            assert!(
                line.starts_with("関数_") || line.starts_with("[…"),
                "a line was cut in the middle: {line:?}"
            );
        }
    }

    #[gpui::test]
    async fn test_attachment_content_notes_size_and_respects_limits(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let buffer = project
            .update(cx, |project, cx| project.create_buffer(None, true, cx))
            .await
            .expect("failed to create buffer");
        let content = "line of text\n".repeat(400);
        buffer.update(cx, |buffer, cx| buffer.set_text(content.clone(), cx));

        let inline = cx
            .spawn({
                let buffer = buffer.clone();
                |cx| async move {
                    get_attachment_content(buffer, Some("big.txt"), Some(content.len()), 512, &cx)
                        .await
                }
            })
            .await
            .unwrap();
        assert!(!inline.is_synthetic);
        assert_eq!(inline.text.len(), 400 * "line of text\n".len());

        let summarized = cx
            .spawn(|cx| async move {
                get_attachment_content(buffer, Some("big.txt"), Some(1024), 512, &cx).await
            })
            .await
            .unwrap();
        assert!(summarized.is_synthetic);
        assert!(
            summarized
                .text
                .starts_with("# `big.txt` (401 lines, 5.1 KB). Its full content was not attached."),
            "{}",
            summarized.text
        );
        assert!(
            summarized.text.len() < 2 * 1024,
            "{}",
            summarized.text.len()
        );
    }
}
