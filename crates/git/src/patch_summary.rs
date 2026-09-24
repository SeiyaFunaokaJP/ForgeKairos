//! Pure helpers for handing a unified diff (the output of `git diff`) to a
//! language model without spending its whole size on it: per-file statistics
//! counted from the patch text itself, and compression that keeps the patch
//! within a byte budget.

use collections::HashMap;
use std::fmt::Write as _;
use std::ops::Range;
use util::markdown::MarkdownCodeBlock;

/// Lines longer than this are cut when a patch is compressed: they are almost
/// always generated or minified content that tells a model nothing.
const MAX_LINE_BYTES: usize = 256;

/// Below this, a patch excerpt is too small to be worth its fence and is left
/// out of a summary altogether.
const MIN_PATCH_EXCERPT_BYTES: usize = 512;

/// Shrinks a patch towards `max_bytes` by cutting long lines and then dropping
/// the last hunks of the files that have the most, keeping at least one hunk
/// per file. The result can still exceed `max_bytes` when every file is down
/// to a single hunk.
///
/// This is what commit message generation has always sent; it only moved here
/// from the git panel so that other callers can share it.
pub fn compress_patch(diff_text: &str, max_bytes: usize) -> String {
    if diff_text.len() <= max_bytes {
        return diff_text.to_string();
    }

    let compressed = diff_text
        .lines()
        .map(|line| {
            if line.len() > MAX_LINE_BYTES {
                format!(
                    "{}...[truncated]\n",
                    &line[..line.floor_char_boundary(MAX_LINE_BYTES)]
                )
            } else {
                format!("{}\n", line)
            }
        })
        .collect::<Vec<_>>()
        .concat();

    if compressed.len() <= max_bytes {
        return compressed;
    }

    truncate_iteratively(&compressed, max_bytes)
}

fn split_patch(patch: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current_patch = String::new();

    for line in patch.lines() {
        if line.starts_with("---") && !current_patch.is_empty() {
            result.push(current_patch.trim_end_matches('\n').into());
            current_patch = String::new();
        }
        current_patch.push_str(line);
        current_patch.push('\n');
    }

    if !current_patch.is_empty() {
        result.push(current_patch.trim_end_matches('\n').into());
    }

    result
}

fn truncate_iteratively(patch: &str, max_bytes: usize) -> String {
    let mut current_size = patch.len();
    if current_size <= max_bytes {
        return patch.to_string();
    }
    let file_patches = split_patch(patch);
    let mut file_infos: Vec<TruncatedPatch> = file_patches
        .iter()
        .filter_map(|patch| TruncatedPatch::from_unified_diff(patch))
        .collect();

    if file_infos.is_empty() {
        return patch.to_string();
    }

    current_size = file_infos.iter().map(|f| f.calculate_size()).sum::<usize>();
    while current_size > max_bytes {
        let file_idx = file_infos
            .iter()
            .enumerate()
            .filter(|(_, f)| f.hunks_to_keep > 1)
            .max_by_key(|(_, f)| f.hunks_to_keep)
            .map(|(idx, _)| idx);
        match file_idx {
            Some(idx) => {
                let file = &mut file_infos[idx];
                let size_before = file.calculate_size();
                file.hunks_to_keep -= 1;
                let size_after = file.calculate_size();
                let saved = size_before.saturating_sub(size_after);
                current_size = current_size.saturating_sub(saved);
            }
            None => {
                break;
            }
        }
    }

    file_infos
        .iter()
        .map(|info| info.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

struct TruncatedPatch {
    header: String,
    hunks: Vec<String>,
    hunks_to_keep: usize,
}

impl TruncatedPatch {
    fn from_unified_diff(patch_str: &str) -> Option<Self> {
        let lines: Vec<&str> = patch_str.lines().collect();
        if lines.len() < 2 {
            return None;
        }
        let header = format!("{}\n{}\n", lines[0], lines[1]);
        let mut hunks = Vec::new();
        let mut current_hunk = String::new();
        for line in &lines[2..] {
            if line.starts_with("@@") {
                if !current_hunk.is_empty() {
                    hunks.push(current_hunk);
                }
                current_hunk = format!("{}\n", line);
            } else if !current_hunk.is_empty() {
                current_hunk.push_str(line);
                current_hunk.push('\n');
            }
        }
        if !current_hunk.is_empty() {
            hunks.push(current_hunk);
        }
        if hunks.is_empty() {
            return None;
        }
        let hunks_to_keep = hunks.len();
        Some(TruncatedPatch {
            header,
            hunks,
            hunks_to_keep,
        })
    }

    fn calculate_size(&self) -> usize {
        let mut size = self.header.len();
        for (i, hunk) in self.hunks.iter().enumerate() {
            if i < self.hunks_to_keep {
                size += hunk.len();
            }
        }
        size
    }

    fn to_string(&self) -> String {
        let mut out = self.header.clone();
        for (i, hunk) in self.hunks.iter().enumerate() {
            if i < self.hunks_to_keep {
                out.push_str(hunk);
            }
        }
        let skipped_hunks = self.hunks.len() - self.hunks_to_keep;
        if skipped_hunks > 0 {
            out.push_str(&format!("[...skipped {} hunks...]\n", skipped_hunks));
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileChangeKind {
    Added,
    Deleted,
    Modified,
    Renamed { from: String },
    Copied { from: String },
}

/// One file's section of a unified diff.
#[derive(Clone, Debug)]
pub struct FilePatch<'a> {
    /// The path on the new side, or on the old side for a deleted file,
    /// relative to the repository root and with git's quoting undone.
    pub path: String,
    pub kind: FileChangeKind,
    pub is_binary: bool,
    pub added_lines: usize,
    pub removed_lines: usize,
    /// The rows (zero-based, end-exclusive) each hunk touches on the new side.
    /// A hunk that only removes lines touches the row just before them.
    pub new_rows: Vec<Range<u32>>,
    header: &'a str,
    hunks: Vec<&'a str>,
}

struct FilePatchBuilder {
    header_range: Range<usize>,
    hunk_ranges: Vec<Range<usize>>,
    fallback_path: Option<String>,
    old_path: Option<String>,
    new_path: Option<String>,
    rename_from: Option<String>,
    rename_to: Option<String>,
    copy_from: Option<String>,
    is_new: bool,
    is_deleted: bool,
    is_binary: bool,
    added_lines: usize,
    removed_lines: usize,
    new_rows: Vec<Range<u32>>,
}

impl FilePatchBuilder {
    fn new(start: usize, fallback_path: Option<String>) -> Self {
        Self {
            header_range: start..start,
            hunk_ranges: Vec::new(),
            fallback_path,
            old_path: None,
            new_path: None,
            rename_from: None,
            rename_to: None,
            copy_from: None,
            is_new: false,
            is_deleted: false,
            is_binary: false,
            added_lines: 0,
            removed_lines: 0,
            new_rows: Vec::new(),
        }
    }

    fn extend_to(&mut self, end: usize) {
        match self.hunk_ranges.last_mut() {
            Some(hunk) => hunk.end = end,
            None => self.header_range.end = end,
        }
    }

    fn finish(self, diff_text: &str) -> Option<FilePatch<'_>> {
        let path = self
            .rename_to
            .clone()
            .or_else(|| self.new_path.clone())
            .or_else(|| self.old_path.clone())
            .or_else(|| self.rename_from.clone())
            .or(self.fallback_path)?;
        let kind = if let Some(from) = self.rename_from {
            FileChangeKind::Renamed { from }
        } else if let Some(from) = self.copy_from {
            FileChangeKind::Copied { from }
        } else if self.is_new {
            FileChangeKind::Added
        } else if self.is_deleted {
            FileChangeKind::Deleted
        } else {
            FileChangeKind::Modified
        };
        Some(FilePatch {
            path,
            kind,
            is_binary: self.is_binary,
            added_lines: self.added_lines,
            removed_lines: self.removed_lines,
            new_rows: self.new_rows,
            header: diff_text.get(self.header_range)?,
            hunks: self
                .hunk_ranges
                .into_iter()
                .filter_map(|range| diff_text.get(range))
                .collect(),
        })
    }
}

/// Splits the output of `git diff` into its files, counting the lines each
/// one adds and removes.
pub fn parse_patch(diff_text: &str) -> Vec<FilePatch<'_>> {
    let mut files = Vec::new();
    let mut current: Option<FilePatchBuilder> = None;
    // Lines of the current hunk's old and new side still to come. Counting
    // them is the only way to tell an added line that starts with `++` from a
    // file header.
    let mut remaining: Option<(u32, u32)> = None;
    let mut offset = 0;

    for line in diff_text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let content = line.trim_end_matches(['\n', '\r']);

        if let (Some(builder), Some((old_remaining, new_remaining))) =
            (current.as_mut(), remaining.as_mut())
            && (*old_remaining > 0 || *new_remaining > 0)
        {
            let counted = match content.as_bytes().first() {
                Some(b'+') => {
                    *new_remaining = new_remaining.saturating_sub(1);
                    builder.added_lines += 1;
                    true
                }
                Some(b'-') => {
                    *old_remaining = old_remaining.saturating_sub(1);
                    builder.removed_lines += 1;
                    true
                }
                Some(b' ') | None => {
                    *old_remaining = old_remaining.saturating_sub(1);
                    *new_remaining = new_remaining.saturating_sub(1);
                    true
                }
                Some(b'\\') => true,
                Some(_) => false,
            };
            if counted {
                builder.extend_to(offset);
                continue;
            }
            remaining = None;
        }

        if let Some(rest) = content
            .strip_prefix("diff --git ")
            .or_else(|| content.strip_prefix("diff --cc "))
            .or_else(|| content.strip_prefix("diff --combined "))
        {
            if let Some(builder) = current.take() {
                files.extend(builder.finish(diff_text));
            }
            let mut builder = FilePatchBuilder::new(start, path_from_diff_line(rest));
            builder.extend_to(offset);
            current = Some(builder);
            remaining = None;
            continue;
        }

        let Some(builder) = current.as_mut() else {
            continue;
        };

        if content.starts_with("@@") {
            builder.hunk_ranges.push(start..offset);
            remaining = parse_hunk_header(content).map(|hunk| {
                builder.new_rows.push(hunk.new_rows);
                (hunk.old_len, hunk.new_len)
            });
            continue;
        }

        if builder.hunk_ranges.is_empty() {
            if content.starts_with("new file mode") {
                builder.is_new = true;
            } else if content.starts_with("deleted file mode") {
                builder.is_deleted = true;
            } else if let Some(path) = content.strip_prefix("rename from ") {
                builder.rename_from = Some(unquote_git_path(path));
            } else if let Some(path) = content.strip_prefix("rename to ") {
                builder.rename_to = Some(unquote_git_path(path));
            } else if let Some(path) = content.strip_prefix("copy from ") {
                builder.copy_from = Some(unquote_git_path(path));
            } else if let Some(path) = content.strip_prefix("copy to ") {
                builder.rename_to = Some(unquote_git_path(path));
            } else if content.starts_with("Binary files ") || content == "GIT binary patch" {
                builder.is_binary = true;
            } else if let Some(path) = content.strip_prefix("--- ") {
                match side_path(path) {
                    Some(path) => builder.old_path = Some(path),
                    None => builder.is_new = true,
                }
            } else if let Some(path) = content.strip_prefix("+++ ") {
                match side_path(path) {
                    Some(path) => builder.new_path = Some(path),
                    None => builder.is_deleted = true,
                }
            }
        }
        builder.extend_to(offset);
    }

    if let Some(builder) = current.take() {
        files.extend(builder.finish(diff_text));
    }
    files
}

struct HunkHeader {
    old_len: u32,
    new_len: u32,
    new_rows: Range<u32>,
}

fn parse_hunk_header(line: &str) -> Option<HunkHeader> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(' ')?;
    let new = rest.strip_prefix('+')?.split(' ').next()?;
    let parse_range = |range: &str| -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    let (_, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    let first_row = new_start.saturating_sub(1);
    let new_rows = if new_len == 0 {
        first_row..first_row.saturating_add(1)
    } else {
        first_row..first_row.saturating_add(new_len)
    };
    Some(HunkHeader {
        old_len,
        new_len,
        new_rows,
    })
}

/// The path on one side of a `---` / `+++` line, or `None` for `/dev/null`.
fn side_path(path: &str) -> Option<String> {
    // Plain (unquoted) paths that contain a space are followed by a tab.
    let path = path.split('\t').next().unwrap_or(path);
    if path == "/dev/null" {
        return None;
    }
    Some(strip_side_prefix(&unquote_git_path(path)).to_string())
}

fn strip_side_prefix(path: &str) -> &str {
    ["a/", "b/", "i/", "w/", "c/", "o/"]
        .iter()
        .find_map(|prefix| path.strip_prefix(prefix))
        .unwrap_or(path)
}

/// The path named on a `diff --git a/<old> b/<new>` line, used when the file
/// has no `---`/`+++` lines (binary files, mode changes, pure renames).
fn path_from_diff_line(rest: &str) -> Option<String> {
    if rest.starts_with('"') || rest.ends_with('"') {
        let tokens = split_quoted_tokens(rest);
        return tokens
            .last()
            .map(|token| strip_side_prefix(token).to_string());
    }
    // `a/<path> b/<path>` with the same path on both sides splits in the
    // middle, which is the only reliable split when the path contains " b/".
    let bytes = rest.as_bytes();
    if bytes.len() % 2 == 1 {
        let middle = bytes.len() / 2;
        if bytes.get(middle) == Some(&b' ')
            && let (Some(old), Some(new)) = (rest.get(..middle), rest.get(middle + 1..))
            && strip_side_prefix(old) == strip_side_prefix(new)
        {
            return Some(strip_side_prefix(new).to_string());
        }
    }
    rest.rsplit_once(" b/")
        .map(|(_, new)| new.to_string())
        .or_else(|| rest.split_whitespace().last().map(str::to_string))
}

fn split_quoted_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        if rest.starts_with('"') {
            let mut end = None;
            let mut escaped = false;
            for (index, character) in rest.char_indices().skip(1) {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    end = Some(index);
                    break;
                }
            }
            let Some(end) = end else {
                tokens.push(unquote_git_path(rest));
                break;
            };
            tokens.push(unquote_git_path(&rest[..=end]));
            rest = rest[end + 1..].trim_start();
        } else {
            let end = rest.find(' ').unwrap_or(rest.len());
            tokens.push(rest[..end].to_string());
            rest = rest[end..].trim_start();
        }
    }
    tokens
}

/// Undoes git's C-style quoting of paths (`core.quotePath`), under which a
/// path with non-ASCII characters is written as octal escapes of its UTF-8
/// bytes.
fn unquote_git_path(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
    else {
        return path.to_string();
    };

    let mut bytes = Vec::with_capacity(inner.len());
    let mut characters = inner.bytes().peekable();
    while let Some(byte) = characters.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match characters.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'a') => bytes.push(0x07),
            Some(b'b') => bytes.push(0x08),
            Some(b'f') => bytes.push(0x0c),
            Some(b'v') => bytes.push(0x0b),
            Some(digit @ b'0'..=b'7') => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    match characters.peek() {
                        Some(next @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(next - b'0');
                            characters.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => bytes.push(other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Keeps only the files at `path` or under it (a repository-relative path; an
/// empty one keeps everything).
pub fn filter_patch(diff_text: &str, path: &str) -> String {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return diff_text.to_string();
    }
    let is_under = |candidate: &str| {
        candidate == path
            || candidate
                .strip_prefix(path)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    let mut output = String::new();
    for file in parse_patch(diff_text) {
        let from = match &file.kind {
            FileChangeKind::Renamed { from } | FileChangeKind::Copied { from } => Some(from),
            _ => None,
        };
        if is_under(&file.path) || from.is_some_and(|from| is_under(from)) {
            output.push_str(file.header);
            for hunk in &file.hunks {
                output.push_str(hunk);
            }
        }
    }
    output
}

/// Keeps only the files whose path, and for a rename or copy also the path it
/// came from, `keep` accepts, returning the kept patch and how many files were
/// dropped.
pub fn retain_files(diff_text: &str, mut keep: impl FnMut(&str) -> bool) -> (String, usize) {
    let mut output = String::new();
    let mut dropped = 0;
    for file in parse_patch(diff_text) {
        let from = match &file.kind {
            FileChangeKind::Renamed { from } | FileChangeKind::Copied { from } => Some(from),
            _ => None,
        };
        if keep(&file.path) && from.is_none_or(|from| keep(from)) {
            output.push_str(file.header);
            for hunk in &file.hunks {
                output.push_str(hunk);
            }
        } else {
            dropped += 1;
        }
    }
    (output, dropped)
}

fn cut_long_lines(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches('\n');
        if content.len() > MAX_LINE_BYTES {
            output.push_str(&content[..content.floor_char_boundary(MAX_LINE_BYTES)]);
            output.push_str("...[truncated]\n");
        } else {
            output.push_str(line);
            if !line.ends_with('\n') {
                output.push('\n');
            }
        }
    }
    output
}

/// Fits the files of a patch into at most `max_bytes`: long lines are cut,
/// then the last hunks of the files with the most hunks go, then whole files
/// from the end, and finally the one file left is cut at a line boundary.
/// Every cut leaves a `[...skipped ...]` marker behind.
fn fit_patch(files: &[FilePatch<'_>], max_bytes: usize) -> String {
    struct Section {
        header: String,
        hunks: Vec<String>,
        kept: usize,
    }

    impl Section {
        fn skipped_marker(&self) -> String {
            let skipped = self.hunks.len() - self.kept;
            if skipped == 0 {
                String::new()
            } else {
                format!("[...skipped {skipped} hunks...]\n")
            }
        }

        fn size(&self) -> usize {
            self.header.len()
                + self.hunks[..self.kept]
                    .iter()
                    .map(|hunk| hunk.len())
                    .sum::<usize>()
                + self.skipped_marker().len()
        }

        fn render(&self, output: &mut String) {
            output.push_str(&self.header);
            for hunk in &self.hunks[..self.kept] {
                output.push_str(hunk);
            }
            output.push_str(&self.skipped_marker());
        }
    }

    let mut sections = files
        .iter()
        .map(|file| {
            let hunks = file
                .hunks
                .iter()
                .map(|hunk| cut_long_lines(hunk))
                .collect::<Vec<_>>();
            Section {
                header: cut_long_lines(file.header),
                kept: hunks.len(),
                hunks,
            }
        })
        .collect::<Vec<_>>();

    let files_marker = |skipped: usize| {
        if skipped == 0 {
            String::new()
        } else {
            format!("[...skipped {skipped} more files...]\n")
        }
    };

    let mut total = sections.iter().map(Section::size).sum::<usize>();
    while total > max_bytes {
        let Some(index) = sections
            .iter()
            .enumerate()
            .filter(|(_, section)| section.kept > 1)
            .max_by_key(|(_, section)| section.kept)
            .map(|(index, _)| index)
        else {
            break;
        };
        let section = &mut sections[index];
        let before = section.size();
        section.kept -= 1;
        total = total - before + section.size();
    }

    let mut kept_files = sections.len();
    while kept_files > 1 && total + files_marker(sections.len() - kept_files).len() > max_bytes {
        kept_files -= 1;
        total -= sections[kept_files].size();
    }

    let mut output = String::new();
    for section in &sections[..kept_files] {
        section.render(&mut output);
    }
    let marker = files_marker(sections.len() - kept_files);

    if output.len() + marker.len() > max_bytes {
        const CUT_MARKER: &str = "[...truncated...]\n";
        let limit = max_bytes.saturating_sub(marker.len() + CUT_MARKER.len());
        let mut cut = 0;
        for line in output.split_inclusive('\n') {
            if cut + line.len() > limit {
                break;
            }
            cut += line.len();
        }
        output.truncate(cut);
        output.push_str(CUT_MARKER);
    }
    output.push_str(&marker);
    output
}

pub struct PatchSummaryOptions<'a> {
    /// What the diff compares, e.g. "Changes since the merge base with `main`".
    pub title: &'a str,
    /// How to get more of the diff than the summary carries.
    pub full_diff_hint: &'a str,
    /// Include an excerpt of the patch after the table, or only the table and
    /// the changed symbols.
    pub include_patch: bool,
    pub max_bytes: usize,
}

/// Summarizes a patch as a per-file table of added and removed lines, the
/// symbols each file's hunks touch (from `changed_symbols`, keyed by path), an
/// excerpt of the patch fitted into what is left of `max_bytes`, and a hint on
/// how to see the rest.
pub fn render_patch_summary(
    diff_text: &str,
    changed_symbols: &HashMap<String, Vec<String>>,
    options: &PatchSummaryOptions<'_>,
) -> String {
    let files = parse_patch(diff_text);
    let added = files.iter().map(|file| file.added_lines).sum::<usize>();
    let removed = files.iter().map(|file| file.removed_lines).sum::<usize>();

    let mut output = String::new();
    writeln!(
        output,
        "{}: {} file{} changed, +{added} -{removed}.\n",
        options.title,
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    )
    .ok();

    let table_budget = options.max_bytes / 4;
    output.push_str("| File | + | - |\n| --- | ---: | ---: |\n");
    let mut listed = 0;
    for file in &files {
        let mut row = String::new();
        write!(row, "| `{}`", file.path).ok();
        match &file.kind {
            FileChangeKind::Added => row.push_str(" (added)"),
            FileChangeKind::Deleted => row.push_str(" (deleted)"),
            FileChangeKind::Renamed { from } => {
                write!(row, " (renamed from `{from}`)").ok();
            }
            FileChangeKind::Copied { from } => {
                write!(row, " (copied from `{from}`)").ok();
            }
            FileChangeKind::Modified => {}
        }
        if file.is_binary {
            row.push_str(" (binary) | - | - |\n");
        } else {
            writeln!(row, " | {} | {} |", file.added_lines, file.removed_lines).ok();
        }
        if output.len() + row.len() > table_budget && listed > 0 {
            break;
        }
        output.push_str(&row);
        listed += 1;
    }
    if listed < files.len() {
        let rest = &files[listed..];
        writeln!(
            output,
            "\n…and {} more files (+{} -{}).",
            rest.len(),
            rest.iter().map(|file| file.added_lines).sum::<usize>(),
            rest.iter().map(|file| file.removed_lines).sum::<usize>(),
        )
        .ok();
    }

    let symbols_budget = output.len() + options.max_bytes / 8;
    let mut symbols_section = String::new();
    for file in &files {
        let Some(symbols) = changed_symbols.get(&file.path) else {
            continue;
        };
        if symbols.is_empty() {
            continue;
        }
        let line = format!("- `{}`: {}\n", file.path, symbols.join(", "));
        if output.len() + symbols_section.len() + line.len() > symbols_budget {
            symbols_section.push_str("- …\n");
            break;
        }
        symbols_section.push_str(&line);
    }
    if !symbols_section.is_empty() {
        output.push_str("\nSymbols touched by the changes (in the new version of each file):\n");
        output.push_str(&symbols_section);
    }

    let hint = format!("\n{}\n", options.full_diff_hint);
    if options.include_patch {
        const PATCH_HEADING: &str =
            "\nPatch (long lines and some hunks cut to fit; `[...skipped ...]` marks where):\n";
        // The fence's backticks and newlines around the excerpt.
        const FENCE_OVERHEAD: usize = 16;
        let patch_budget = options
            .max_bytes
            .saturating_sub(output.len() + hint.len() + PATCH_HEADING.len() + FENCE_OVERHEAD);
        if patch_budget >= MIN_PATCH_EXCERPT_BYTES {
            let patch = fit_patch(&files, patch_budget);
            output.push_str(PATCH_HEADING);
            write!(
                output,
                "{}",
                MarkdownCodeBlock {
                    tag: "diff",
                    text: patch.trim_end_matches('\n'),
                }
            )
            .ok();
        } else {
            output.push_str("\nThe patch itself was left out to stay within the size budget.\n");
        }
    }
    output.push_str(&hint);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch_text(lines: &[&str]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    fn two_files() -> String {
        patch_text(&[
            r#"diff --git a/src/lib.rs b/src/lib.rs"#,
            r#"index 1111111..2222222 100644"#,
            r#"--- a/src/lib.rs"#,
            r#"+++ b/src/lib.rs"#,
            r#"@@ -1,3 +1,4 @@"#,
            r#" fn one() {}"#,
            r#"-fn two() {}"#,
            r#"+fn two() -> u32 { 2 }"#,
            r#"+fn three() {}"#,
            r#" fn four() {}"#,
            r#"@@ -10,2 +11,3 @@ impl Config {"#,
            r#"     fn new() {}"#,
            r#"+    fn with_name() {}"#,
            r#" }"#,
            r#"diff --git a/README.md b/README.md"#,
            r#"deleted file mode 100644"#,
            r#"index 3333333..0000000"#,
            r#"--- a/README.md"#,
            r#"+++ /dev/null"#,
            r#"@@ -1,2 +0,0 @@"#,
            r#"-# Title"#,
            r#"-text"#,
        ])
    }

    #[test]
    fn test_parse_patch_counts_lines_per_file() {
        let patch = two_files();
        let files = parse_patch(&patch);
        assert_eq!(files.len(), 2);

        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(files[0].kind, FileChangeKind::Modified);
        assert_eq!((files[0].added_lines, files[0].removed_lines), (3, 1));
        assert_eq!(files[0].new_rows, vec![0..4, 10..13]);

        assert_eq!(files[1].path, "README.md");
        assert_eq!(files[1].kind, FileChangeKind::Deleted);
        assert_eq!((files[1].added_lines, files[1].removed_lines), (0, 2));

        let rebuilt = files
            .iter()
            .map(|file| format!("{}{}", file.header, file.hunks.concat()))
            .collect::<String>();
        assert_eq!(rebuilt, patch);
    }

    #[test]
    fn test_parse_patch_does_not_mistake_added_lines_for_headers() {
        let patch = patch_text(&[
            "diff --git a/notes.txt b/notes.txt",
            "--- a/notes.txt",
            "+++ b/notes.txt",
            "@@ -1,1 +1,3 @@",
            " keep",
            "+++ added line that starts with pluses",
            "+--- and one that starts with dashes",
        ]);
        let files = parse_patch(&patch);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "notes.txt");
        assert_eq!((files[0].added_lines, files[0].removed_lines), (2, 0));
    }

    #[test]
    fn test_parse_patch_multibyte_and_quoted_paths() {
        let patch = patch_text(&[
            r#"diff --git "a/\346\227\245\346\234\254/\350\252\254\346\230\216.md" "b/\346\227\245\346\234\254/\350\252\254\346\230\216.md""#,
            r#"--- "a/\346\227\245\346\234\254/\350\252\254\346\230\216.md""#,
            r#"+++ "b/\346\227\245\346\234\254/\350\252\254\346\230\216.md""#,
            r#"@@ -1 +1 @@"#,
            r#"-古い"#,
            r#"+新しい"#,
            r#"diff --git "a/\346\226\260.png" "b/\346\226\260.png""#,
            r#"new file mode 100644"#,
            r#"index 0000000..4444444"#,
            r#"Binary files /dev/null and "b/\346\226\260.png" differ"#,
        ]);
        let files = parse_patch(&patch);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "日本/説明.md");
        assert_eq!((files[0].added_lines, files[0].removed_lines), (1, 1));
        assert_eq!(files[1].path, "新.png");
        assert!(files[1].is_binary);
        assert_eq!(files[1].kind, FileChangeKind::Added);
        assert_eq!(files[1].added_lines, 0);
    }

    #[test]
    fn test_parse_patch_rename_without_content_changes() {
        let patch = patch_text(&[
            "diff --git a/old name.rs b/new name.rs",
            "similarity index 100%",
            "rename from old name.rs",
            "rename to new name.rs",
        ]);
        let files = parse_patch(&patch);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "new name.rs");
        assert_eq!(
            files[0].kind,
            FileChangeKind::Renamed {
                from: "old name.rs".into()
            }
        );
        assert!(files[0].new_rows.is_empty());
    }

    #[test]
    fn test_parse_patch_of_nothing() {
        assert!(parse_patch("").is_empty());
        let summary = render_patch_summary(
            "",
            &HashMap::default(),
            &PatchSummaryOptions {
                title: "Changes",
                full_diff_hint: "hint",
                include_patch: true,
                max_bytes: 4096,
            },
        );
        assert!(
            summary.starts_with("Changes: 0 files changed, +0 -0."),
            "{summary}"
        );
    }

    #[test]
    fn test_filter_patch_keeps_files_under_the_path() {
        let patch = two_files();
        let filtered = filter_patch(&patch, "src");
        assert!(filtered.contains("src/lib.rs"), "{filtered}");
        assert!(!filtered.contains("README.md"), "{filtered}");
        assert_eq!(filter_patch(&patch, "src/lib.rs"), filtered);
        assert!(filter_patch(&patch, "sr").is_empty());
        assert_eq!(filter_patch(&patch, ""), patch);
    }

    fn many_hunks_patch(files: usize, hunks: usize) -> String {
        let mut patch = String::new();
        for file in 0..files {
            write!(
                patch,
                "diff --git a/file{file}.rs b/file{file}.rs\n--- a/file{file}.rs\n+++ b/file{file}.rs\n"
            )
            .ok();
            for hunk in 0..hunks {
                let line = hunk * 10 + 1;
                write!(
                    patch,
                    "@@ -{line},2 +{line},2 @@\n context {hunk}\n-old {file} {hunk}\n+new {file} {hunk}\n"
                )
                .ok();
            }
        }
        patch
    }

    #[test]
    fn test_fit_patch_stays_within_budget() {
        let patch = many_hunks_patch(10, 20);
        let files = parse_patch(&patch);
        for budget in [200, 1_000, 4_000] {
            let fitted = fit_patch(&files, budget);
            assert!(
                fitted.len() <= budget,
                "{} > {budget}:\n{fitted}",
                fitted.len()
            );
            assert!(fitted.contains("[...skipped"), "{fitted}");
        }
        // Whole files go before any file is cut in the middle.
        let fitted = fit_patch(&files, 200);
        assert!(
            fitted.starts_with("diff --git a/file0.rs b/file0.rs\n"),
            "{fitted}"
        );
        assert!(
            fitted.ends_with("[...skipped 9 more files...]\n"),
            "{fitted}"
        );
        assert!(!fitted.contains("[...truncated...]"), "{fitted}");
        assert_eq!(fit_patch(&files, patch.len()), patch);
    }

    #[test]
    fn test_fit_patch_cuts_long_multibyte_lines_on_char_boundaries() {
        let long_line = "日本語".repeat(200);
        let patch = format!(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-x\n+{long_line}\n"
        );
        let files = parse_patch(&patch);
        let fitted = fit_patch(&files, 1_000);
        assert!(fitted.contains("...[truncated]"), "{fitted}");
        assert!(fitted.len() < patch.len());
    }

    #[test]
    fn test_summary_lists_files_symbols_and_hint() {
        let mut symbols = HashMap::default();
        symbols.insert(
            "src/lib.rs".to_string(),
            vec![
                "fn two [L2]".to_string(),
                "impl Config [L10-13]".to_string(),
            ],
        );
        let summary = render_patch_summary(
            &two_files(),
            &symbols,
            &PatchSummaryOptions {
                title: "Changes since the merge base with `main`",
                full_diff_hint: "Run `git diff --merge-base main -- <path>` for the full diff.",
                include_patch: true,
                max_bytes: 16 * 1024,
            },
        );
        assert!(
            summary
                .starts_with("Changes since the merge base with `main`: 2 files changed, +3 -3.\n"),
            "{summary}"
        );
        assert!(summary.contains("| `src/lib.rs` | 3 | 1 |"), "{summary}");
        assert!(
            summary.contains("| `README.md` (deleted) | 0 | 2 |"),
            "{summary}"
        );
        assert!(
            summary.contains("- `src/lib.rs`: fn two [L2], impl Config [L10-13]"),
            "{summary}"
        );
        assert!(summary.contains("```diff\n"), "{summary}");
        assert!(summary.contains("+fn three() {}"), "{summary}");
        assert!(summary.ends_with("for the full diff.\n"), "{summary}");

        let without_patch = render_patch_summary(
            &two_files(),
            &symbols,
            &PatchSummaryOptions {
                title: "Changes",
                full_diff_hint: "hint",
                include_patch: false,
                max_bytes: 16 * 1024,
            },
        );
        assert!(!without_patch.contains("```diff"), "{without_patch}");
        assert!(without_patch.contains("| `src/lib.rs` | 3 | 1 |"));
    }

    #[test]
    fn test_summary_respects_budget_for_large_patches() {
        let patch = many_hunks_patch(400, 30);
        let budget = 16 * 1024;
        let summary = render_patch_summary(
            &patch,
            &HashMap::default(),
            &PatchSummaryOptions {
                title: "Changes",
                full_diff_hint: "hint",
                include_patch: true,
                max_bytes: budget,
            },
        );
        assert!(summary.len() <= budget, "{} > {budget}", summary.len());
        assert!(summary.contains("more files (+"), "{summary}");
        assert!(
            summary.contains("400 files changed, +12000 -12000"),
            "{summary}"
        );
    }

    #[test]
    fn test_malformed_patches_do_not_panic() {
        let long_multibyte_line = format!("+{}", "あ".repeat(100_000));
        let malformed = [
            "@@ -1 +1 @@\n+orphan hunk\n".to_string(),
            "diff --git a/x b/x\n@@ -4294967295,4294967295 +4294967295,4294967295 @@\n+a\n"
                .to_string(),
            "diff --git a/x b/x\n@@ -99999999999 +1 @@\n+a\n".to_string(),
            "diff --git \"a/\\777\\ b/\\\" \"b/\\\n--- \"a/\\\n+++ \"b/\\q\\\"\n@@ -1 +1 @@\n-a\n+b\n"
                .to_string(),
            "diff --git a/crlf.txt b/crlf.txt\r\n--- a/crlf.txt\r\n+++ b/crlf.txt\r\n@@ -1 +1 @@\r\n-a\r\n+b\r\n"
                .to_string(),
            format!("diff --git a/big b/big\n--- a/big\n+++ b/big\n@@ -1 +1 @@\n-x\n{long_multibyte_line}\n"),
            "diff --git \"a/\\346\\227\" b/\n".to_string(),
        ];
        for patch in &malformed {
            let files = parse_patch(patch);
            let summary = render_patch_summary(
                patch,
                &HashMap::default(),
                &PatchSummaryOptions {
                    title: "Changes",
                    full_diff_hint: "hint",
                    include_patch: true,
                    max_bytes: 2048,
                },
            );
            assert!(summary.len() <= 2048, "{} > 2048: {summary}", summary.len());
            filter_patch(patch, "x");
            retain_files(patch, |_| false);
            compress_patch(patch, 100);
            fit_patch(&files, 600);
        }

        let files = parse_patch(&malformed[1]);
        assert_eq!(files[0].new_rows, vec![u32::MAX - 1..u32::MAX]);
        let files = parse_patch(&malformed[4]);
        assert_eq!(files[0].path, "crlf.txt");
        assert_eq!((files[0].added_lines, files[0].removed_lines), (1, 1));
    }

    #[test]
    fn test_retain_files_checks_both_sides_of_a_rename() {
        let patch = patch_text(&[
            "diff --git a/secret.env b/public.txt",
            "similarity index 90%",
            "rename from secret.env",
            "rename to public.txt",
            "--- a/secret.env",
            "+++ b/public.txt",
            "@@ -1 +1 @@",
            "-TOKEN=1",
            "+TOKEN=2",
            "diff --git a/src/lib.rs b/src/lib.rs",
            "--- a/src/lib.rs",
            "+++ b/src/lib.rs",
            "@@ -1 +1 @@",
            "-a",
            "+b",
        ]);
        let (kept, dropped) = retain_files(&patch, |path| !path.ends_with(".env"));
        assert_eq!(dropped, 1);
        assert!(!kept.contains("TOKEN"), "{kept}");
        assert!(kept.starts_with("diff --git a/src/lib.rs"), "{kept}");
        assert_eq!(retain_files(&patch, |_| true), (patch.clone(), 0));
    }

    #[test]
    fn test_compress_patch_matches_the_commit_message_behavior() {
        let diff = patch_text(&[
            "--- a/file.txt",
            "+++ b/file.txt",
            "@@ -1,2 +1,2 @@",
            " context",
            "-old1",
            "+new1",
            "@@ -5,2 +5,2 @@",
            " context 2",
            "-old2",
            "+new2",
            "@@ -10,2 +10,2 @@",
            " context 3",
            "-old3",
            "+new3",
        ]);
        let result = compress_patch(&diff, 100);
        let expected = patch_text(&[
            "--- a/file.txt",
            "+++ b/file.txt",
            "@@ -1,2 +1,2 @@",
            " context",
            "-old1",
            "+new1",
            "[...skipped 2 hunks...]",
        ]);
        assert_eq!(result, expected);
    }
}
