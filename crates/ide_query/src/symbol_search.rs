use std::{cmp::Ordering, path::PathBuf, pin::pin, sync::atomic::AtomicBool};

use anyhow::{Result, anyhow};
use collections::HashMap;
use futures::{
    StreamExt as _,
    future::{self, Either},
};
use fuzzy::StringMatchCandidate;
use gpui::{App, AppContext as _, AsyncApp, BackgroundExecutor, Entity, Task};
use language::{Buffer, BufferSnapshot, OutlineItem, Point, Rope, SymbolKind};
use project::{Project, ProjectPath, Symbol, lsp_store::SymbolLocation};

use crate::{
    LANGUAGE_SERVER_TIMEOUT, LineRange, MAX_QUERY_RESULTS, MAX_SIGNATURE_CHARS, SymbolHit,
    SymbolSearchOptions, is_in_project, truncate_chars, withheld_reason,
};

/// Files the outline scan considers, in the order it considers them.
const MAX_SCANNED_FILES: usize = 5_000;
/// Bytes read from disk to find the files that mention the query.
const MAX_SCANNED_BYTES: u64 = 64 * 1024 * 1024;
/// Larger files are usually generated or vendored, and one would dominate the budget.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Files that mention the query and get parsed for their outline.
const MAX_PARSED_FILES: usize = 300;
/// Over a remote connection a file can only be read by opening a buffer for it, which transfers
/// its whole content, so only files whose path names the query are opened, and few of them.
const MAX_REMOTE_OPENED_FILES: usize = 20;
const CONCURRENT_FILE_LOADS: usize = 16;

pub(crate) fn search_symbols(
    project: &Entity<Project>,
    query: &str,
    options: SymbolSearchOptions,
    cx: &mut App,
) -> Task<Result<Vec<SymbolHit>>> {
    let Some(query) = SymbolQuery::parse(query) else {
        return Task::ready(Err(anyhow!("検索語を指定してください")));
    };
    let limit = if options.limit == 0 {
        MAX_QUERY_RESULTS
    } else {
        options.limit.min(MAX_QUERY_RESULTS)
    };
    let path_prefix = options.path_prefix.as_deref().and_then(PathPrefix::parse);
    let language_server_request = project.update(cx, |project, cx| {
        project.symbols(&query.language_server_query(), cx)
    });
    let deadline = cx.background_executor().timer(LANGUAGE_SERVER_TIMEOUT);
    let project = project.clone();

    cx.spawn(async move |cx| {
        let mut candidates = outline_candidates(&project, &query, path_prefix.as_ref(), cx).await;

        // The deadline started with the request, so a scan that outlasted it leaves the
        // server no extra time; `select` still prefers an answer that is already there.
        let language_server_symbols =
            match future::select(pin!(language_server_request), pin!(deadline)).await {
                Either::Left((Ok(symbols), _)) => symbols,
                Either::Left((Err(error), _)) => {
                    log::warn!("workspace symbol search failed: {error:#}");
                    Vec::new()
                }
                Either::Right(_) => {
                    log::warn!(
                        "workspace symbol search took longer than {} seconds",
                        LANGUAGE_SERVER_TIMEOUT.as_secs()
                    );
                    Vec::new()
                }
            };
        candidates.extend(project.read_with(cx, |project, cx| {
            language_server_candidates(project, language_server_symbols, cx)
        }));
        align_language_server_rows(&mut candidates);
        candidates.retain(|candidate| {
            path_prefix
                .as_ref()
                .is_none_or(|prefix| prefix.matches(&candidate.full_path, &candidate.relative_path))
        });

        Ok(rank(candidates, &query, limit, cx.background_executor().clone()).await)
    })
}

pub(crate) struct SymbolQuery {
    trimmed: String,
    lowercase: String,
    words: Vec<String>,
    compact: String,
}

impl SymbolQuery {
    pub(crate) fn parse(query: &str) -> Option<Self> {
        let words = identifier_words(query);
        if words.is_empty() {
            return None;
        }
        let trimmed = query.split_whitespace().collect::<Vec<_>>().join(" ");
        Some(Self {
            lowercase: trimmed.to_lowercase(),
            trimmed,
            compact: words.concat(),
            words,
        })
    }

    /// Servers match `workspace/symbol` queries against single identifiers, mostly as a fuzzy
    /// subsequence, so words typed apart are sent joined up.
    fn language_server_query(&self) -> String {
        if self.trimmed.contains(' ') {
            self.compact.clone()
        } else {
            self.trimmed.clone()
        }
    }
}

/// Lowercase words of an identifier or a phrase: split at anything that is not a letter or a
/// digit and at `camelCase` / `HTTPServer` boundaries, so `validate_user_token`,
/// `UserTokenStore` and "user token" all yield `user` and `token`.
pub(crate) fn identifier_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous: Option<char> = None;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if !character.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous = None;
            continue;
        }
        if let Some(previous) = previous
            && character.is_uppercase()
        {
            let next_is_lowercase = characters.peek().is_some_and(|next| next.is_lowercase());
            let starts_word = previous.is_lowercase()
                || previous.is_numeric()
                || (previous.is_uppercase() && next_is_lowercase);
            if starts_word && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        }
        current.extend(character.to_lowercase());
        previous = Some(character);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MatchTier {
    /// Every word appears in the name, container, path or signature.
    Context = 1,
    /// Every word starts a word of the name, in any order.
    Words = 2,
    Substring = 3,
    Prefix = 4,
    Exact = 5,
}

pub(crate) fn match_tier(query: &SymbolQuery, name: &str, context: &[&str]) -> Option<MatchTier> {
    let name_lowercase = name.to_lowercase();
    let name_words = identifier_words(name);
    let name_compact = name_words.concat();

    if name_lowercase == query.lowercase
        || (!name_compact.is_empty() && name_compact == query.compact)
    {
        return Some(MatchTier::Exact);
    }
    if name_lowercase.starts_with(&query.lowercase) || words_start_with(&name_words, &query.words) {
        return Some(MatchTier::Prefix);
    }
    if name_lowercase.contains(&query.lowercase)
        || name_compact.contains(&query.compact)
        || contains_word_run(&name_words, &query.words)
    {
        return Some(MatchTier::Substring);
    }
    if query.words.iter().all(|word| {
        name_words
            .iter()
            .any(|name_word| name_word.starts_with(word.as_str()))
    }) {
        return Some(MatchTier::Words);
    }
    let context = context
        .iter()
        .map(|text| text.to_lowercase())
        .collect::<Vec<_>>();
    if query.words.iter().all(|word| {
        name_lowercase.contains(word.as_str())
            || context.iter().any(|text| text.contains(word.as_str()))
    }) {
        return Some(MatchTier::Context);
    }
    None
}

/// The name's words begin with the query's, the last of which may be cut short.
fn words_start_with(name_words: &[String], query_words: &[String]) -> bool {
    name_words.len() >= query_words.len() && word_run_matches(name_words, query_words)
}

fn contains_word_run(name_words: &[String], query_words: &[String]) -> bool {
    (0..name_words.len()).any(|start| {
        name_words.get(start..).is_some_and(|rest| {
            rest.len() >= query_words.len() && word_run_matches(rest, query_words)
        })
    })
}

fn word_run_matches(name_words: &[String], query_words: &[String]) -> bool {
    let Some((last_query_word, leading_query_words)) = query_words.split_last() else {
        return false;
    };
    leading_query_words
        .iter()
        .zip(name_words)
        .all(|(query_word, name_word)| query_word == name_word)
        && name_words
            .get(leading_query_words.len())
            .is_some_and(|name_word| name_word.starts_with(last_query_word.as_str()))
}

/// A tier is worth 100 and nothing within it adds up to that, so a better tier always wins.
fn tier_score(tier: MatchTier, query: &SymbolQuery, candidate: &Candidate) -> f64 {
    let name_length = identifier_words(&candidate.name)
        .concat()
        .chars()
        .count()
        .max(1);
    let closeness = (query.compact.chars().count() as f64 / name_length as f64).min(1.0);
    let mut score = tier as u8 as f64 * 100.0 + 40.0 * closeness;
    if candidate.name.contains(&query.trimmed) {
        score += 10.0;
    }
    if candidate.in_project {
        score += 20.0;
    }
    score
}

/// Below every tier: the letters of the query appear in the name in order, as in `vut` for
/// `validate_user_token`.
fn fuzzy_score(fuzzy_score: f64, candidate: &Candidate) -> f64 {
    let mut score = 40.0 * fuzzy_score.clamp(0.0, 1.0);
    if candidate.in_project {
        score += 20.0;
    }
    score
}

#[derive(Clone, Debug)]
struct Candidate {
    name: String,
    kind: String,
    container: Option<String>,
    path: ProjectPath,
    full_path: String,
    relative_path: String,
    lines: LineRange,
    name_row: u32,
    signature: String,
    in_project: bool,
    from_language_server: bool,
}

async fn rank(
    candidates: Vec<Candidate>,
    query: &SymbolQuery,
    limit: usize,
    executor: BackgroundExecutor,
) -> Vec<SymbolHit> {
    let mut scored = Vec::new();
    let mut unmatched = Vec::new();
    for candidate in candidates {
        let context = [
            candidate.container.as_deref().unwrap_or_default(),
            candidate.full_path.as_str(),
            candidate.signature.as_str(),
        ];
        match match_tier(query, &candidate.name, &context) {
            Some(tier) => scored.push((tier_score(tier, query, &candidate), candidate)),
            None => unmatched.push(Some(candidate)),
        }
    }

    if !unmatched.is_empty() {
        let fuzzy_candidates = unmatched
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                Some(StringMatchCandidate::new(index, &candidate.as_ref()?.name))
            })
            .collect::<Vec<_>>();
        let matches = fuzzy::match_strings(
            &fuzzy_candidates,
            &query.compact,
            false,
            true,
            limit,
            &AtomicBool::new(false),
            executor,
        )
        .await;
        for string_match in matches {
            if let Some(candidate) = unmatched
                .get_mut(string_match.candidate_id)
                .and_then(Option::take)
            {
                scored.push((fuzzy_score(string_match.score, &candidate), candidate));
            }
        }
    }

    let mut unique: Vec<(f64, Candidate)> = Vec::new();
    let mut index_by_location: HashMap<(ProjectPath, u32), usize> = HashMap::default();
    for (score, candidate) in scored {
        let key = (candidate.path.clone(), candidate.name_row);
        match index_by_location
            .get(&key)
            .and_then(|index| unique.get_mut(*index))
        {
            Some(kept) => merge_duplicate(kept, (score, candidate)),
            None => {
                index_by_location.insert(key, unique.len());
                unique.push((score, candidate));
            }
        }
    }

    unique.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| right.in_project.cmp(&left.in_project))
            .then_with(|| left.full_path.cmp(&right.full_path))
            .then_with(|| left.lines.start_line.cmp(&right.lines.start_line))
    });
    unique.truncate(limit);
    unique
        .into_iter()
        .map(|(score, candidate)| SymbolHit {
            name: candidate.name,
            kind: candidate.kind,
            container: candidate.container,
            path: candidate.path,
            lines: candidate.lines,
            signature: candidate.signature,
            in_project: candidate.in_project,
            score,
        })
        .collect()
}

/// A language server may report a declaration's whole range, which starts at its doc comment or
/// attributes (rust-analyzer does), while the outline names the row of the name itself. Such a
/// report is moved onto the row of the outline item with the same name inside that range, so
/// that the two are recognised as one declaration.
fn align_language_server_rows(candidates: &mut [Candidate]) {
    let mut outline_rows: HashMap<(ProjectPath, String), Vec<u32>> = HashMap::default();
    for candidate in candidates
        .iter()
        .filter(|candidate| !candidate.from_language_server)
    {
        outline_rows
            .entry((candidate.path.clone(), candidate.name.clone()))
            .or_default()
            .push(candidate.name_row);
    }
    if outline_rows.is_empty() {
        return;
    }
    for candidate in candidates
        .iter_mut()
        .filter(|candidate| candidate.from_language_server)
    {
        let Some(rows) = outline_rows.get(&(candidate.path.clone(), candidate.name.clone())) else {
            continue;
        };
        if rows.contains(&candidate.name_row) {
            continue;
        }
        let first_row = candidate.lines.start_line.saturating_sub(1);
        let last_row = candidate.lines.end_line.saturating_sub(1);
        if let Some(row) = rows
            .iter()
            .copied()
            .filter(|row| (first_row..=last_row).contains(row))
            .min()
        {
            candidate.name_row = row;
        }
    }
}

/// A language server and the outline often report the same declaration. The better-scored
/// report is kept, but the outline's lines (the whole item) and signature (source text rather
/// than the server's label) are preferred.
fn merge_duplicate(kept: &mut (f64, Candidate), other: (f64, Candidate)) {
    let other_wins = match other.0.total_cmp(&kept.0) {
        Ordering::Greater => true,
        Ordering::Equal => other.1.from_language_server && !kept.1.from_language_server,
        Ordering::Less => false,
    };
    let loser = if other_wins {
        std::mem::replace(kept, other).1
    } else {
        other.1
    };
    let winner = &mut kept.1;
    if winner.container.is_none() {
        winner.container = loser.container;
    }
    if winner.from_language_server && !loser.from_language_server {
        winner.lines = loser.lines;
        if !loser.signature.is_empty() {
            winner.signature = loser.signature;
        }
    }
}

fn language_server_candidates(project: &Project, symbols: Vec<Symbol>, cx: &App) -> Vec<Candidate> {
    symbols
        .into_iter()
        .filter_map(|symbol| {
            // A symbol outside every worktree has no project path to report it by.
            let SymbolLocation::InProject(path) = symbol.path else {
                return None;
            };
            if withheld_reason(project, &path, cx).is_some() {
                return None;
            }
            let in_project = is_in_project(project, &path, cx);
            let worktree = project.worktree_for_id(path.worktree_id, cx)?;
            let worktree = worktree.read(cx);
            let start_row = symbol.range.start.0.row;
            let end_row = symbol.range.end.0.row.max(start_row);
            Some(Candidate {
                name: symbol.name,
                kind: symbol_kind_name(symbol.kind).to_string(),
                container: symbol
                    .container_name
                    .filter(|container| !container.is_empty()),
                full_path: worktree
                    .root_name()
                    .join(&path.path)
                    .as_unix_str()
                    .to_string(),
                relative_path: path.path.as_unix_str().to_string(),
                path,
                lines: LineRange {
                    start_line: start_row + 1,
                    end_line: end_row + 1,
                },
                name_row: start_row,
                signature: if in_project {
                    truncate_chars(symbol.label.text.trim(), MAX_SIGNATURE_CHARS)
                } else {
                    String::new()
                },
                in_project,
                from_language_server: true,
            })
        })
        .collect()
}

struct ScanFile {
    path: ProjectPath,
    full_path: String,
    relative_path: String,
    abs_path: PathBuf,
    size: u64,
}

async fn outline_candidates(
    project: &Entity<Project>,
    query: &SymbolQuery,
    path_prefix: Option<&PathPrefix>,
    cx: &mut AsyncApp,
) -> Vec<Candidate> {
    let (worktrees, open_buffers, language_registry, fs, is_local) =
        project.read_with(cx, |project, cx| {
            let worktrees = project
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).snapshot())
                .collect::<Vec<_>>();
            let open_buffers = project
                .opened_buffers(cx)
                .into_iter()
                .filter_map(|buffer| {
                    let file = buffer.read(cx).file()?;
                    let path = ProjectPath::from_file(file.as_ref(), cx);
                    (!file.is_private() && withheld_reason(project, &path, cx).is_none())
                        .then_some((path, buffer))
                })
                .collect::<HashMap<_, _>>();
            (
                worktrees,
                open_buffers,
                project.languages().clone(),
                project.fs().clone(),
                project.is_local(),
            )
        });

    let files = cx
        .background_spawn({
            let language_registry = language_registry.clone();
            let path_prefix = path_prefix.cloned();
            async move {
                let mut files = Vec::new();
                for snapshot in &worktrees {
                    for entry in snapshot.files(false, 0) {
                        if entry.is_private || entry.is_external || entry.size > MAX_FILE_BYTES {
                            continue;
                        }
                        let full_path = snapshot.root_name().join(&entry.path);
                        let full_path = full_path.as_unix_str();
                        let relative_path = entry.path.as_unix_str();
                        if path_prefix
                            .as_ref()
                            .is_some_and(|prefix| !prefix.matches(full_path, relative_path))
                        {
                            continue;
                        }
                        if language_registry
                            .language_for_file_path(entry.path.as_std_path())
                            .is_none()
                        {
                            continue;
                        }
                        files.push(ScanFile {
                            path: ProjectPath {
                                worktree_id: snapshot.id(),
                                path: entry.path.clone(),
                            },
                            full_path: full_path.to_string(),
                            relative_path: relative_path.to_string(),
                            abs_path: snapshot.absolutize(&entry.path),
                            size: entry.size,
                        });
                    }
                }
                // Shallow files first, as the trunk of a project is what a search is most
                // often after when the caps cut the scan short.
                files.sort_by(|left, right| {
                    let left_depth = left.full_path.matches('/').count();
                    let right_depth = right.full_path.matches('/').count();
                    left_depth
                        .cmp(&right_depth)
                        .then_with(|| left.full_path.cmp(&right.full_path))
                });
                files.truncate(MAX_SCANNED_FILES);
                files
            }
        })
        .await;

    let mut open_snapshots: Vec<(ScanFile, BufferSnapshot)> = Vec::new();
    let mut unopened_files = Vec::new();
    for file in files {
        match open_buffers.get(&file.path) {
            Some(buffer) => {
                buffer
                    .read_with(cx, |buffer, _| buffer.parsing_idle())
                    .await;
                let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
                open_snapshots.push((file, snapshot));
            }
            None => unopened_files.push(file),
        }
    }
    // Copying out and lowercasing every open file is too much work for the foreground.
    let mut snapshots = cx
        .background_spawn({
            let words = query.words.clone();
            async move {
                open_snapshots
                    .into_iter()
                    .filter(|(file, snapshot)| {
                        words_mentioned(&snapshot.text(), &file.full_path, &words)
                    })
                    .take(MAX_PARSED_FILES)
                    .collect::<Vec<_>>()
            }
        })
        .await;

    if is_local {
        let words = query.words.clone();
        let remaining = MAX_PARSED_FILES.saturating_sub(snapshots.len());
        let matching_files = cx
            .background_spawn(async move {
                let mut scanned_bytes = 0;
                let files_within_budget = unopened_files
                    .into_iter()
                    .take_while(|file| {
                        scanned_bytes += file.size;
                        scanned_bytes <= MAX_SCANNED_BYTES
                    })
                    .collect::<Vec<_>>();
                futures::stream::iter(files_within_budget)
                    .map(|file| {
                        let fs = fs.clone();
                        let words = words.clone();
                        async move {
                            let text = fs.load(&file.abs_path).await.ok()?;
                            words_mentioned(&text, &file.full_path, &words)
                                .then(|| (file, Rope::from(text.as_str())))
                        }
                    })
                    .buffered(CONCURRENT_FILE_LOADS)
                    .filter_map(future::ready)
                    .take(remaining)
                    .collect::<Vec<_>>()
                    .await
            })
            .await;

        for (file, text) in matching_files {
            let Some(language) = language_registry
                .load_language_for_file_path(&file.abs_path)
                .await
                .ok()
            else {
                continue;
            };
            if language
                .grammar()
                .and_then(|grammar| grammar.outline_config.as_ref())
                .is_none()
            {
                continue;
            }
            let build_snapshot = cx.update(|cx| {
                Buffer::build_snapshot(
                    text,
                    Some(language),
                    Some(language_registry.clone()),
                    None,
                    cx,
                )
            });
            let snapshot = cx.background_spawn(build_snapshot).await;
            snapshots.push((file, snapshot));
        }
    } else {
        let mut opened_files = 0;
        for file in unopened_files {
            if opened_files >= MAX_REMOTE_OPENED_FILES || snapshots.len() >= MAX_PARSED_FILES {
                break;
            }
            let full_path = file.full_path.to_lowercase();
            if !query
                .words
                .iter()
                .any(|word| full_path.contains(word.as_str()))
            {
                continue;
            }
            // A remote worktree's entries never carry `is_private` (the protocol does not send
            // it), so only the settings can tell.
            let withheld = project.read_with(cx, |project, cx| {
                withheld_reason(project, &file.path, cx).is_some()
            });
            if withheld {
                continue;
            }
            opened_files += 1;
            let open_buffer =
                project.update(cx, |project, cx| project.open_buffer(file.path.clone(), cx));
            let buffer = match open_buffer.await {
                Ok(buffer) => buffer,
                Err(error) => {
                    log::warn!(
                        "could not open {} for a symbol search: {error:#}",
                        file.full_path
                    );
                    continue;
                }
            };
            buffer
                .read_with(cx, |buffer, _| buffer.parsing_idle())
                .await;
            let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
            snapshots.push((file, snapshot));
        }
    }

    cx.background_spawn(async move {
        let mut candidates = Vec::new();
        for (file, snapshot) in snapshots {
            let outline =
                snapshot.outline_items_as_points_containing(0..snapshot.len(), true, None);
            for symbol in outline_symbols(&outline) {
                candidates.push(Candidate {
                    name: symbol.name,
                    kind: symbol.kind.to_string(),
                    container: symbol.container,
                    path: file.path.clone(),
                    full_path: file.full_path.clone(),
                    relative_path: file.relative_path.clone(),
                    lines: symbol.lines,
                    name_row: symbol.name_row,
                    signature: symbol.signature,
                    in_project: true,
                    from_language_server: false,
                });
            }
        }
        candidates
    })
    .await
}

/// Whether a file can hold a match at all: every word has to appear in it or in its path.
fn words_mentioned(text: &str, full_path: &str, words: &[String]) -> bool {
    let text = text.to_lowercase();
    let full_path = full_path.to_lowercase();
    words
        .iter()
        .all(|word| text.contains(word.as_str()) || full_path.contains(word.as_str()))
}

#[derive(Clone, Debug)]
pub(crate) struct PathPrefix(String);

impl PathPrefix {
    /// `None` when the prefix names the whole project.
    pub(crate) fn parse(prefix: &str) -> Option<Self> {
        let normalized = prefix.trim().replace('\\', "/");
        let normalized = normalized.trim_start_matches("./").trim_matches('/');
        (!normalized.is_empty() && normalized != ".").then(|| Self(normalized.to_string()))
    }

    /// Accepts the prefix either with the worktree's root name in front, as agents write
    /// paths, or relative to the worktree root.
    pub(crate) fn matches(&self, full_path: &str, relative_path: &str) -> bool {
        [full_path, relative_path].into_iter().any(|path| {
            path.strip_prefix(self.0.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }
}

pub(crate) struct OutlineSymbol {
    pub name: String,
    pub kind: &'static str,
    pub container: Option<String>,
    pub lines: LineRange,
    pub name_row: u32,
    pub signature: String,
}

pub(crate) fn outline_symbols(items: &[OutlineItem<Point>]) -> Vec<OutlineSymbol> {
    let mut ancestors: Vec<(usize, String, &'static str)> = Vec::new();
    let mut symbols = Vec::new();
    for item in items {
        while ancestors
            .last()
            .is_some_and(|(depth, _, _)| *depth >= item.depth)
        {
            ancestors.pop();
        }
        let name = outline_item_name(item);
        if name.is_empty() {
            continue;
        }
        let parent = ancestors.last();
        let kind = outline_item_kind(item, parent.map(|(_, _, kind)| *kind));
        let start_row = item.range.start.row;
        symbols.push(OutlineSymbol {
            name: name.clone(),
            kind,
            container: parent.map(|(_, name, _)| name.clone()),
            lines: LineRange {
                start_line: start_row + 1,
                end_line: item.range.end.row.max(start_row) + 1,
            },
            name_row: item.selection_range.start.row,
            signature: truncate_chars(item.text.trim(), MAX_SIGNATURE_CHARS),
        });
        ancestors.push((item.depth, name, kind));
    }
    symbols
}

/// From the first name capture to the last, so that `impl Display for Config` is named
/// `Display for Config` rather than just one of its types.
fn outline_item_name(item: &OutlineItem<Point>) -> String {
    let start = item.name_ranges.first().map(|range| range.start);
    let end = item.name_ranges.last().map(|range| range.end);
    match start
        .zip(end)
        .and_then(|(start, end)| item.text.get(start..end))
    {
        Some(name) => name.trim().to_string(),
        None => item.text.trim().to_string(),
    }
}

fn outline_item_kind(item: &OutlineItem<Point>, parent_kind: Option<&str>) -> &'static str {
    let context = item
        .name_ranges
        .first()
        .and_then(|range| item.text.get(..range.start))
        .unwrap_or_default();
    for token in context.split_whitespace().rev() {
        let kind = match token.trim_end_matches(['!', ':']) {
            "fn" | "def" | "func" | "function" | "fun" => {
                if matches!(
                    parent_kind,
                    Some("impl" | "trait" | "class" | "interface" | "struct")
                ) {
                    "method"
                } else {
                    "function"
                }
            }
            "struct" => "struct",
            "enum" => "enum",
            "union" => "union",
            "trait" => "trait",
            "impl" => "impl",
            "class" => "class",
            "interface" => "interface",
            "mod" | "module" | "namespace" | "package" => "module",
            "type" | "typedef" => "type",
            "const" => "constant",
            "static" => "static",
            "macro_rules" | "macro" => "macro",
            "let" | "var" | "val" => "variable",
            _ => continue,
        };
        return kind;
    }
    match parent_kind {
        Some("struct" | "class" | "union") => "field",
        Some("enum") => "variant",
        _ => "symbol",
    }
}

pub(crate) fn symbol_kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::File => "file",
        SymbolKind::Module => "module",
        SymbolKind::Namespace => "namespace",
        SymbolKind::Package => "package",
        SymbolKind::Class => "class",
        SymbolKind::Method => "method",
        SymbolKind::Property => "property",
        SymbolKind::Field => "field",
        SymbolKind::Constructor => "constructor",
        SymbolKind::Enum => "enum",
        SymbolKind::Interface => "interface",
        SymbolKind::Function => "function",
        SymbolKind::Variable => "variable",
        SymbolKind::Constant => "constant",
        SymbolKind::String => "string",
        SymbolKind::Number => "number",
        SymbolKind::Boolean => "boolean",
        SymbolKind::Array => "array",
        SymbolKind::Object => "object",
        SymbolKind::Key => "key",
        SymbolKind::Null => "null",
        SymbolKind::EnumMember => "enum member",
        SymbolKind::Struct => "struct",
        SymbolKind::Event => "event",
        SymbolKind::Operator => "operator",
        SymbolKind::TypeParameter => "type parameter",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str) -> SymbolQuery {
        SymbolQuery::parse(text).expect("the query has words")
    }

    #[test]
    fn identifiers_split_into_lowercase_words() {
        assert_eq!(
            identifier_words("validate_user_token"),
            ["validate", "user", "token"]
        );
        assert_eq!(
            identifier_words("UserTokenStore"),
            ["user", "token", "store"]
        );
        assert_eq!(identifier_words("HTTPServer"), ["http", "server"]);
        assert_eq!(identifier_words("utf8Decoder"), ["utf8", "decoder"]);
        assert_eq!(identifier_words("  user   token "), ["user", "token"]);
        assert_eq!(identifier_words("設定::読み込み"), ["設定", "読み込み"]);
        assert!(identifier_words(" :: ").is_empty());
    }

    #[test]
    fn names_match_in_tiers() {
        let user_token = query("user token");
        let tier = |name: &str| match_tier(&user_token, name, &[]);
        assert_eq!(tier("UserToken"), Some(MatchTier::Exact));
        assert_eq!(tier("user_token"), Some(MatchTier::Exact));
        assert_eq!(tier("UserTokenStore"), Some(MatchTier::Prefix));
        assert_eq!(tier("validate_user_token"), Some(MatchTier::Substring));
        assert_eq!(tier("validateUserTokens"), Some(MatchTier::Substring));
        assert_eq!(tier("token_for_user"), Some(MatchTier::Words));
        assert_eq!(tier("load"), None);
        assert_eq!(
            match_tier(
                &user_token,
                "load",
                &["UserTokenCache", "src/cache.rs", "fn load"]
            ),
            Some(MatchTier::Context)
        );
        assert_eq!(
            match_tier(
                &user_token,
                "load",
                &["UserCache", "src/cache.rs", "fn load"]
            ),
            None,
            "every word has to appear"
        );

        let single = query("parse");
        assert_eq!(match_tier(&single, "parse", &[]), Some(MatchTier::Exact));
        assert_eq!(match_tier(&single, "Parse", &[]), Some(MatchTier::Exact));
        assert_eq!(match_tier(&single, "parser", &[]), Some(MatchTier::Prefix));
        assert_eq!(
            match_tier(&single, "reparse", &[]),
            Some(MatchTier::Substring)
        );
        assert_eq!(
            match_tier(&single, "try_parse_all", &[]),
            Some(MatchTier::Substring)
        );

        assert!(MatchTier::Exact > MatchTier::Prefix);
        assert!(MatchTier::Prefix > MatchTier::Substring);
        assert!(MatchTier::Substring > MatchTier::Words);
        assert!(MatchTier::Words > MatchTier::Context);
    }

    #[test]
    fn language_servers_get_the_words_joined_up() {
        assert_eq!(query("user token").language_server_query(), "usertoken");
        assert_eq!(query(" UserToken ").language_server_query(), "UserToken");
        assert_eq!(query("Foo::bar").language_server_query(), "Foo::bar");
    }

    #[test]
    fn path_prefixes_match_whole_components() {
        assert!(PathPrefix::parse("").is_none());
        assert!(PathPrefix::parse(" . ").is_none());
        assert!(PathPrefix::parse("./").is_none());
        let prefix = PathPrefix::parse("./src\\sub/").expect("a prefix");
        assert!(prefix.matches("root/src/sub/a.rs", "src/sub/a.rs"));
        assert!(prefix.matches("root/src/sub", "src/sub"));
        assert!(!prefix.matches("root/src/subtle.rs", "src/subtle.rs"));
        let prefix = PathPrefix::parse("root/src").expect("a prefix");
        assert!(prefix.matches("root/src/a.rs", "src/a.rs"));
        assert!(!prefix.matches("other/src/a.rs", "src/a.rs"));
    }
}
