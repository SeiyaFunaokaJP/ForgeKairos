mod git;
mod lexer;
mod placeholders;
mod restore_pairs;
mod token_diff;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use restore_pairs::{CommentCounts, PairKind, PairRecord, SourceFile};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(about = "Fork-only i18n tooling for zed-jp")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Recover (English original, Japanese replacement) pairs from the fork's diff.
    RestorePairs {
        #[arg(long, default_value = "de178dd")]
        base: String,
        #[arg(long, default_value = "HEAD")]
        head: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Print a reproducible random sample of pairs for manual review.
    Sample {
        /// The `--out` directory of `restore-pairs`.
        #[arg(long)]
        pairs: PathBuf,
        #[arg(long)]
        kind: String,
        #[arg(long, default_value_t = 60)]
        count: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Leave out pairs inside tests.
        #[arg(long)]
        skip_tests: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::RestorePairs {
            base,
            head,
            repo,
            out,
        } => restore(&repo, &base, &head, &out),
        Command::Sample {
            pairs,
            kind,
            count,
            seed,
            skip_tests,
        } => sample(&pairs, &kind, count, seed, skip_tests),
    }
}

fn crate_of(path: &str) -> Option<&str> {
    path.strip_prefix("crates/")?.split('/').next()
}

fn is_test_path(path: &str, test_module_files: &HashSet<String>) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    path.contains("/tests/")
        || file_name == "tests.rs"
        || file_name.ends_with("_tests.rs")
        || test_module_files.contains(path)
}

fn restore(repository: &Path, base: &str, head: &str, out: &Path) -> Result<()> {
    let files = git::changed_rust_files(repository, base, head, "crates/")?;
    let test_module_files: HashSet<String> = git::out_of_line_test_modules(repository, head)?
        .into_iter()
        .collect();
    let mut blobs = git::BlobReader::new(repository)?;
    let mut records_by_crate: BTreeMap<String, Vec<PairRecord>> = BTreeMap::new();
    let mut comments_by_crate: BTreeMap<String, CommentCounts> = BTreeMap::new();
    for (position, file) in files.iter().enumerate() {
        let Some(crate_name) = crate_of(&file.head_path) else {
            continue;
        };
        if position % 50 == 0 {
            eprintln!("[{position}/{}] {}", files.len(), file.head_path);
        }
        let head_text = blobs.read(head, &file.head_path)?;
        let head_source = SourceFile::parse(&head_text);
        let base_source = match &file.base_path {
            Some(base_path) => Some(SourceFile::parse(&blobs.read(base, base_path)?)),
            None => None,
        };
        let analysis = restore_pairs::analyze_file(
            crate_name,
            &file.head_path,
            base_source.as_ref(),
            &head_source,
            is_test_path(&file.head_path, &test_module_files),
        );
        let comment_counts = comments_by_crate.entry(crate_name.to_string()).or_default();
        comment_counts.line_or_block += analysis.comments.line_or_block;
        comment_counts.doc += analysis.comments.doc;
        if !analysis.records.is_empty() {
            records_by_crate
                .entry(crate_name.to_string())
                .or_default()
                .extend(analysis.records);
        }
    }

    let pairs_directory = out.join("pairs");
    if pairs_directory.exists() {
        fs::remove_dir_all(&pairs_directory)
            .with_context(|| format!("clearing {}", pairs_directory.display()))?;
    }
    fs::create_dir_all(&pairs_directory)?;
    for (crate_name, records) in &records_by_crate {
        let mut contents = String::new();
        for record in records {
            contents.push_str(&serde_json::to_string(record)?);
            contents.push('\n');
        }
        fs::write(
            pairs_directory.join(format!("{crate_name}.jsonl")),
            contents,
        )?;
    }
    let all_records: Vec<&PairRecord> = records_by_crate.values().flatten().collect();
    let japanese_to_english = translation_candidates(&all_records, Direction::JapaneseToEnglish);
    let english_to_japanese = translation_candidates(&all_records, Direction::EnglishToJapanese);
    fs::write(
        out.join("ja_to_en.json"),
        serde_json::to_string_pretty(&japanese_to_english)?,
    )?;
    fs::write(
        out.join("en_to_ja.json"),
        serde_json::to_string_pretty(&english_to_japanese)?,
    )?;
    fs::write(
        out.join("summary.md"),
        summary(
            base,
            head,
            &records_by_crate,
            &comments_by_crate,
            &japanese_to_english,
            &english_to_japanese,
        )?,
    )?;
    eprintln!(
        "wrote {} records for {} crates to {}",
        all_records.len(),
        records_by_crate.len(),
        out.display()
    );
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    JapaneseToEnglish,
    EnglishToJapanese,
}

#[derive(serde::Serialize)]
struct CandidateEntry {
    source: String,
    distinct: usize,
    total: usize,
    candidates: Vec<Candidate>,
}

#[derive(serde::Serialize)]
struct Candidate {
    text: String,
    count: usize,
    outside_tests: usize,
    kinds: Vec<&'static str>,
}

/// Only pairs whose English side is known (`exact`, `format`, `plural_collapsed`) take part.
fn translation_candidates(records: &[&PairRecord], direction: Direction) -> Vec<CandidateEntry> {
    let mut grouped: BTreeMap<String, BTreeMap<String, (usize, usize, Vec<&'static str>)>> =
        BTreeMap::new();
    for record in records {
        if !matches!(
            record.kind,
            PairKind::Exact | PairKind::Format | PairKind::PluralCollapsed
        ) {
            continue;
        }
        let Some(en) = &record.en else {
            continue;
        };
        let ja = record
            .ja_as_msgstr
            .clone()
            .unwrap_or_else(|| record.ja.clone());
        let (source, target) = match direction {
            Direction::JapaneseToEnglish => (ja, en.clone()),
            Direction::EnglishToJapanese => (en.clone(), ja),
        };
        let entry = grouped
            .entry(source)
            .or_default()
            .entry(target)
            .or_insert((0, 0, Vec::new()));
        entry.0 += 1;
        if !record.in_test {
            entry.1 += 1;
        }
        if !entry.2.contains(&record.kind.name()) {
            entry.2.push(record.kind.name());
        }
    }
    let mut entries: Vec<CandidateEntry> = grouped
        .into_iter()
        .map(|(source, targets)| {
            let mut candidates: Vec<Candidate> = targets
                .into_iter()
                .map(|(text, (count, outside_tests, kinds))| Candidate {
                    text,
                    count,
                    outside_tests,
                    kinds,
                })
                .collect();
            candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.count));
            CandidateEntry {
                source,
                distinct: candidates.len(),
                total: candidates.iter().map(|candidate| candidate.count).sum(),
                candidates,
            }
        })
        .collect();
    entries.sort_by(|left, right| {
        right
            .distinct
            .cmp(&left.distinct)
            .then(right.total.cmp(&left.total))
            .then(left.source.cmp(&right.source))
    });
    entries
}

fn markdown_cell(text: &str) -> String {
    let flattened: String = text
        .replace('\n', "\\n")
        .replace('|', "\\|")
        .chars()
        .take(80)
        .collect();
    format!("`{flattened}`").replace("``", "` `")
}

fn summary(
    base: &str,
    head: &str,
    records_by_crate: &BTreeMap<String, Vec<PairRecord>>,
    comments_by_crate: &BTreeMap<String, CommentCounts>,
    japanese_to_english: &[CandidateEntry],
    english_to_japanese: &[CandidateEntry],
) -> Result<String> {
    let mut output = String::new();
    let all: Vec<&PairRecord> = records_by_crate.values().flatten().collect();
    let count_kind = |records: &[&PairRecord], kind: PairKind, in_test: Option<bool>| {
        records
            .iter()
            .filter(|record| record.kind == kind)
            .filter(|record| in_test.is_none_or(|in_test| record.in_test == in_test))
            .count()
    };
    writeln!(output, "# Japanese literal pairs ({base}..{head})\n")?;
    writeln!(output, "Total Japanese literals: {}\n", all.len())?;
    writeln!(output, "| kind | total | outside tests | in tests |")?;
    writeln!(output, "|---|---:|---:|---:|")?;
    for kind in PairKind::ALL {
        writeln!(
            output,
            "| {} | {} | {} | {} |",
            kind.name(),
            count_kind(&all, kind, None),
            count_kind(&all, kind, Some(false)),
            count_kind(&all, kind, Some(true)),
        )?;
    }

    writeln!(output, "\n## By crate\n")?;
    writeln!(
        output,
        "| crate | total | in tests | exact | format | plural_collapsed | ambiguous | fork_new | comments | doc comments |"
    )?;
    writeln!(output, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")?;
    let mut crate_names: Vec<&String> = comments_by_crate
        .iter()
        .filter(|(name, counts)| {
            records_by_crate.contains_key(*name) || counts.doc + counts.line_or_block > 0
        })
        .map(|(name, _)| name)
        .collect();
    crate_names.sort_by_key(|name| {
        std::cmp::Reverse(records_by_crate.get(*name).map(Vec::len).unwrap_or(0))
    });
    for crate_name in crate_names {
        let records: Vec<&PairRecord> = records_by_crate
            .get(crate_name)
            .map(|records| records.iter().collect())
            .unwrap_or_default();
        let comments = comments_by_crate
            .get(crate_name)
            .cloned()
            .unwrap_or_default();
        writeln!(
            output,
            "| {crate_name} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            records.len(),
            records.iter().filter(|record| record.in_test).count(),
            count_kind(&records, PairKind::Exact, None),
            count_kind(&records, PairKind::Format, None),
            count_kind(&records, PairKind::PluralCollapsed, None),
            count_kind(&records, PairKind::Ambiguous, None),
            count_kind(&records, PairKind::ForkNew, None),
            comments.line_or_block,
            comments.doc,
        )?;
    }
    let total_comments: usize = comments_by_crate
        .values()
        .map(|counts| counts.line_or_block)
        .sum();
    let total_doc: usize = comments_by_crate.values().map(|counts| counts.doc).sum();
    writeln!(
        output,
        "\nJapanese comments added by the fork (not translated, counted only): {total_comments} line/block, {total_doc} doc.\n"
    )?;

    writeln!(output, "## By call (exact + format + plural_collapsed)\n")?;
    let mut calls: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for record in &all {
        let call = record.call.clone().unwrap_or_else(|| "(none)".to_string());
        let entry = calls.entry(call).or_default();
        if matches!(
            record.kind,
            PairKind::Exact | PairKind::Format | PairKind::PluralCollapsed
        ) {
            entry.0 += 1;
        }
        entry.1 += 1;
    }
    let mut calls: Vec<(String, (usize, usize))> = calls.into_iter().collect();
    calls.sort_by(|left, right| right.1.1.cmp(&left.1.1).then(left.0.cmp(&right.0)));
    writeln!(output, "| call | paired | all Japanese literals |")?;
    writeln!(output, "|---|---:|---:|")?;
    for (call, (paired, total)) in calls.iter().take(80) {
        writeln!(output, "| {} | {paired} | {total} |", markdown_cell(call))?;
    }

    writeln!(output, "\n## Ambiguous reasons\n")?;
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    for record in all
        .iter()
        .filter(|record| record.kind == PairKind::Ambiguous)
    {
        let reason = record.reason.clone().unwrap_or_default();
        let key = reason.split(':').next().unwrap_or_default().to_string();
        *reasons.entry(key).or_default() += 1;
    }
    let mut reasons: Vec<(String, usize)> = reasons.into_iter().collect();
    reasons.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (reason, count) in reasons {
        writeln!(output, "- {reason}: {count}")?;
    }

    for (title, entries) in [
        (
            "One Japanese text from several English originals",
            japanese_to_english,
        ),
        (
            "One English original with several Japanese texts",
            english_to_japanese,
        ),
    ] {
        let multiple: Vec<&CandidateEntry> =
            entries.iter().filter(|entry| entry.distinct > 1).collect();
        writeln!(
            output,
            "\n## {title} ({} entries, top 50)\n",
            multiple.len()
        )?;
        writeln!(output, "| source | candidates |")?;
        writeln!(output, "|---|---|")?;
        for entry in multiple.iter().take(50) {
            let candidates = entry
                .candidates
                .iter()
                .map(|candidate| format!("{} ×{}", markdown_cell(&candidate.text), candidate.count))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                output,
                "| {} | {candidates} |",
                markdown_cell(&entry.source)
            )?;
        }
    }
    Ok(output)
}

/// SplitMix64: a tiny deterministic generator so samples can be reproduced from the seed.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }
}

fn sample(pairs: &Path, kind: &str, count: usize, seed: u64, skip_tests: bool) -> Result<()> {
    let mut records: Vec<PairRecord> = Vec::new();
    let mut paths: Vec<PathBuf> = fs::read_dir(pairs.join("pairs"))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    paths.sort();
    for path in paths {
        let contents = fs::read_to_string(&path)?;
        for line in contents.lines() {
            let record: PairRecord = serde_json::from_str(line)
                .with_context(|| format!("parsing a record in {}", path.display()))?;
            if record.kind.name() == kind && !(skip_tests && record.in_test) {
                records.push(record);
            }
        }
    }
    let mut generator = SplitMix64(seed);
    for index in (1..records.len()).rev() {
        let other = (generator.next() % (index as u64 + 1)) as usize;
        records.swap(index, other);
    }
    for (position, record) in records.iter().take(count).enumerate() {
        println!(
            "=== #{position} {}:{} (base line {}) call={} in_test={} reason={}",
            record.file,
            record.head_line,
            record
                .base_line
                .map(|line| line.to_string())
                .unwrap_or_else(|| "-".to_string()),
            record.call.as_deref().unwrap_or("-"),
            record.in_test,
            record.reason.as_deref().unwrap_or("-"),
        );
        println!("en: {:?}", record.en);
        println!("ja: {:?}", record.ja);
        println!("msgstr: {:?}", record.ja_as_msgstr);
        println!(
            "--- base\n{}\n--- head\n{}",
            record.base_snippet.as_deref().unwrap_or("-"),
            record.head_snippet
        );
    }
    eprintln!("{} records of kind {kind}", records.len());
    Ok(())
}
