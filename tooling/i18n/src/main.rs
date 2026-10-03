mod brand;
mod check;
mod doc_catalogs;
mod git;
mod lexer;
mod placeholders;
mod po_document;
mod restore_pairs;
mod sources;
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
    /// List string literals with Japanese in them in `crates/`, split into test and non-test code.
    Scan {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Only these crates (the directory names under `crates/`). Repeatable.
        #[arg(long = "crate")]
        crates: Vec<String>,
        /// Print every literal with its file and line.
        #[arg(long)]
        list: bool,
        /// Exit with 1 when a literal outside tests remains, except in the i18n crates and
        /// allowed files.
        #[arg(long)]
        fail_on_prod: bool,
        /// A file (or a directory ending in `/`) whose literals are allowed, relative to the
        /// repository. Repeatable.
        #[arg(long)]
        allow: Vec<String>,
        /// A file listing allowed paths, one per line (`#` starts a comment).
        #[arg(long)]
        allow_list: Option<PathBuf>,
        /// Instead of Japanese, list the literals outside tests that name upstream's
        /// application, other than the messages of the i18n macros, and exit with 1 when one
        /// is missing from `tooling/i18n/brand_sources.txt` or a line there matches nothing.
        #[arg(long, conflicts_with_all = ["fail_on_prod", "allow", "allow_list"])]
        brand: bool,
    },
    /// Write a `.pot` template per crate from the `t!`, `tn!`, `tc!`, `tcn!` and `msg!` calls.
    Extract {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long = "crate")]
        crates: Vec<String>,
        /// Also write messages that only tests use.
        #[arg(long)]
        include_tests: bool,
    },
    /// Compare the messages in the sources with `locales/<language>/*.po`.
    Check {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Only these languages (directory names under `locales/`). Repeatable.
        #[arg(long = "lang")]
        languages: Vec<String>,
        /// Only these crates' PO files; the `_` files are checked only without this option.
        #[arg(long = "crate")]
        crates: Vec<String>,
        /// Also fail on entries nothing uses any more and on translations whose English has
        /// changed (untranslated messages fail without it).
        #[arg(long)]
        strict: bool,
        /// The output of `zed --dump-all-actions`, to compare `_action_docs.po` with the actions'
        /// doc comments.
        #[arg(long)]
        actions: Option<PathBuf>,
        /// The output of `zed --dump-settings-schema`, to compare `_settings_schema.po` with the
        /// schema's descriptions.
        #[arg(long)]
        settings_schema: Option<PathBuf>,
    },
    /// Put the product's own name in place of upstream's: write `locales/en/_brand.po` and
    /// rename the application in the other languages' translations, keeping what
    /// `locales/_brand_keep.txt` lists. Running it again changes nothing.
    Brand {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Write the files. Without it nothing is written.
        #[arg(long, conflicts_with = "check")]
        write: bool,
        /// Change nothing; exit with 1 when a file would change or the keep list has problems.
        #[arg(long)]
        check: bool,
    },
    /// Merge PO files into one, sorted. Exits with 1 without writing when they disagree.
    PoUnion {
        #[arg(required = true, num_args = 1..)]
        inputs: Vec<PathBuf>,
        /// May be one of the inputs.
        #[arg(long)]
        out: PathBuf,
    },
    /// Rewrite PO files in the repository's layout (sorted, one line per string, LF).
    PoFormat {
        /// Files, or directories whose `.po` files to format.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
        /// Change nothing; exit with 1 when a file would change.
        #[arg(long)]
        check: bool,
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
        Command::Scan {
            repo,
            crates,
            list,
            brand: true,
            ..
        } => {
            if !brand::check_sources(&repo, &crates, list)? {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Scan {
            repo,
            crates,
            list,
            fail_on_prod,
            allow,
            allow_list,
            brand: false,
        } => scan(
            &repo,
            &crates,
            list,
            fail_on_prod,
            allow,
            allow_list.as_deref(),
        ),
        Command::Extract {
            repo,
            out,
            crates,
            include_tests,
        } => extract(&repo, &out, &crates, include_tests),
        Command::Check {
            repo,
            languages,
            crates,
            strict,
            actions,
            settings_schema,
        } => run_check(
            &repo,
            &languages,
            &crates,
            strict,
            actions.as_deref(),
            settings_schema.as_deref(),
        ),
        Command::Brand { repo, write, .. } => {
            if !brand::run(&repo, branding::CURRENT.product_name, !write)? {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::PoUnion { inputs, out } => po_union(&inputs, &out),
        Command::PoFormat { paths, check } => po_format(&paths, check),
    }
}

/// Crates whose Japanese belongs to the translation machinery itself (test catalogs, examples).
const I18N_CRATES: [&str; 4] = ["i18n", "i18n_catalog", "i18n_settings", "i18n_macros"];

fn scan(
    repository: &Path,
    crates: &[String],
    list: bool,
    fail_on_prod: bool,
    mut allowed: Vec<String>,
    allow_list: Option<&Path>,
) -> Result<()> {
    if let Some(allow_list) = allow_list {
        let text = fs::read_to_string(allow_list)
            .with_context(|| format!("reading {}", allow_list.display()))?;
        allowed.extend(
            text.lines()
                .map(|line| line.split('#').next().unwrap_or_default().trim())
                .filter(|line| !line.is_empty())
                .map(str::to_string),
        );
    }
    let allowed: Vec<String> = allowed
        .into_iter()
        .map(|path| path.replace('\\', "/"))
        .collect();
    let is_allowed = |crate_name: &str, path: &str| {
        I18N_CRATES.contains(&crate_name)
            || allowed.iter().any(|allowed| {
                path == allowed || (allowed.ends_with('/') && path.starts_with(allowed.as_str()))
            })
    };
    let files = sources::load_crates(repository, crates)?;
    let literals = sources::japanese_literals(&files);
    let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for literal in &literals {
        let entry = counts.entry(literal.crate_name.as_str()).or_default();
        if literal.in_test {
            entry.1 += 1;
        } else {
            entry.0 += 1;
        }
    }
    if list {
        for literal in &literals {
            println!(
                "{}:{}: {} {:?}",
                literal.path,
                literal.line,
                if literal.in_test { "test" } else { "prod" },
                literal.value
            );
        }
        println!();
    }
    let width = counts
        .keys()
        .map(|name| name.len())
        .max()
        .unwrap_or(5)
        .max(5);
    println!("{:<width$} {:>6} {:>6}", "crate", "prod", "test");
    for (crate_name, (production, test)) in &counts {
        println!("{crate_name:<width$} {production:>6} {test:>6}");
    }
    let production_total: usize = counts.values().map(|(production, _)| production).sum();
    let test_total: usize = counts.values().map(|(_, test)| test).sum();
    println!("{:<width$} {production_total:>6} {test_total:>6}", "total");
    if fail_on_prod {
        let offending = literals
            .iter()
            .filter(|literal| !literal.in_test && !is_allowed(&literal.crate_name, &literal.path))
            .count();
        if offending > 0 {
            eprintln!("{offending} Japanese literals outside tests (run with --list to see them)");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn print_extract_warnings(extraction: &sources::Extraction) {
    for warning in &extraction.warnings {
        eprintln!(
            "warning: {}:{}: {}",
            warning.path, warning.line, warning.message
        );
    }
}

fn extract(repository: &Path, out: &Path, crates: &[String], include_tests: bool) -> Result<()> {
    let files = sources::load_crates(repository, crates)?;
    let extraction = sources::extract(&files);
    print_extract_warnings(&extraction);
    fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let mut written = 0;
    for (crate_name, messages) in &extraction.crates {
        let entries: Vec<po_document::DocumentEntry> = messages
            .iter()
            .filter(|message| include_tests || !message.only_in_tests())
            .filter(|message| !check::is_area_message(message))
            .map(|message| po_document::DocumentEntry {
                comments: Vec::new(),
                entry: i18n_catalog::PoEntry {
                    context: message.key.context.clone(),
                    msgid: message.key.msgid.clone(),
                    msgstr: if message.msgid_plural.is_some() {
                        vec![String::new(), String::new()]
                    } else {
                        vec![String::new()]
                    },
                    msgid_plural: message.msgid_plural.clone(),
                    fuzzy: false,
                    line: 0,
                },
            })
            .collect();
        if entries.is_empty() {
            continue;
        }
        let document = po_document::PoDocument {
            header: Some(vec![(
                "Content-Type".to_string(),
                "text/plain; charset=UTF-8".to_string(),
            )]),
            entries,
            ..po_document::PoDocument::default()
        };
        let path = out.join(format!("{crate_name}.pot"));
        let text = po_document::format_document(&path.display().to_string(), document)?;
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        written += 1;
    }
    eprintln!("wrote {written} templates to {}", out.display());
    Ok(())
}

fn run_check(
    repository: &Path,
    languages: &[String],
    crates: &[String],
    strict: bool,
    actions: Option<&Path>,
    settings_schema: Option<&Path>,
) -> Result<()> {
    let files = sources::load_crates(repository, crates)?;
    let extraction = sources::extract(&files);
    print_extract_warnings(&extraction);
    let action_documentation = actions
        .map(doc_catalogs::read_action_documentation)
        .transpose()?;
    let settings_schema_descriptions = settings_schema
        .map(doc_catalogs::read_schema_descriptions)
        .transpose()?;
    let action_argument_descriptions = actions
        .map(doc_catalogs::read_action_argument_descriptions)
        .transpose()?;
    let results = check::check(
        repository,
        &extraction,
        &check::CheckOptions {
            languages,
            crates,
            action_documentation: action_documentation.as_ref(),
            settings_schema_descriptions: settings_schema_descriptions.as_ref(),
            action_argument_descriptions: action_argument_descriptions.as_ref(),
        },
    )?;
    let (output, mut failed) = check::report(&results, strict);
    print!("{output}");
    // The brand file overlays every file of every language, so it is checked only when
    // the whole of them is.
    if crates.is_empty() && languages.is_empty() {
        println!("brand:");
        failed |= !brand::run(repository, branding::CURRENT.product_name, true)?;
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn read_po_document(path: &Path) -> Result<po_document::PoDocument> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    po_document::read_document(&path.display().to_string(), &text).map_err(|errors| {
        let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();
        anyhow::anyhow!("{}", messages.join("\n"))
    })
}

fn po_union(inputs: &[PathBuf], out: &Path) -> Result<()> {
    let mut documents = Vec::new();
    for input in inputs {
        documents.push((input.display().to_string(), read_po_document(input)?));
    }
    match po_document::union(&documents) {
        Ok(document) => {
            fs::write(out, po_document::render(&document))
                .with_context(|| format!("writing {}", out.display()))?;
            eprintln!(
                "wrote {} entries to {}",
                document.entries.len(),
                out.display()
            );
            Ok(())
        }
        Err(conflicts) => {
            for conflict in &conflicts {
                eprintln!("conflict: {}", conflict.description);
            }
            eprintln!("{} conflicts; nothing was written", conflicts.len());
            std::process::exit(1);
        }
    }
}

fn po_format(paths: &[PathBuf], check_only: bool) -> Result<()> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut found = Vec::new();
            for entry in fs::read_dir(path)? {
                let entry_path = entry?.path();
                if entry_path
                    .extension()
                    .is_some_and(|extension| extension == "po")
                {
                    found.push(entry_path);
                }
            }
            found.sort();
            files.extend(found);
        } else {
            files.push(path.clone());
        }
    }
    let mut changed = 0;
    for path in &files {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let formatted =
            po_document::format_document(&path.display().to_string(), read_po_document(path)?)?;
        if formatted == text {
            continue;
        }
        changed += 1;
        if check_only {
            println!("{} is not formatted", path.display());
        } else {
            fs::write(path, formatted).with_context(|| format!("writing {}", path.display()))?;
            println!("formatted {}", path.display());
        }
    }
    if check_only && changed > 0 {
        std::process::exit(1);
    }
    Ok(())
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
