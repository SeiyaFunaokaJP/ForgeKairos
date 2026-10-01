//! Measures the symbol index on a directory of C/C++ sources.
//!
//! ```text
//! cargo run --release -p symbol_index --example symbol_index_bench -- <source-root> <index-dir> [threads]
//! ```
//!
//! The index directory is deleted first. The source tree is only read; the incremental phase
//! substitutes one file's contents in memory.

// A standalone measuring tool: asking the OS for this process's memory counters through a
// child process is simpler than binding the platform APIs, and blocking is fine here.
#![allow(clippy::disallowed_methods)]

use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context as _, Result};
use symbol_index::{
    DirectorySource, FileEntry, FileSource, IndexConfig, SearchMode, SymbolIndex, scan_directory,
};

struct OverlaySource {
    base: DirectorySource,
    overlay: Mutex<HashMap<String, Vec<u8>>>,
}

impl FileSource for OverlaySource {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        if let Ok(overlay) = self.overlay.lock()
            && let Some(contents) = overlay.get(path)
        {
            return Ok(contents.clone());
        }
        self.base.read(path)
    }
}

fn memory() -> String {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let field = |name: &str| {
            status
                .lines()
                .find(|line| line.starts_with(name))
                .map(|line| line.trim_start_matches(name).trim().to_owned())
                .unwrap_or_default()
        };
        format!("rss {} (peak {})", field("VmRSS:"), field("VmHWM:"))
    }
    #[cfg(windows)]
    {
        let script = format!(
            "$p = Get-Process -Id {}; \"$($p.WorkingSet64) $($p.PrivateMemorySize64) $($p.PeakWorkingSet64)\"",
            std::process::id()
        );
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output();
        let Ok(output) = output else {
            return "unavailable".to_owned();
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let values: Vec<u64> = text
            .split_whitespace()
            .filter_map(|value| value.parse().ok())
            .collect();
        match values.as_slice() {
            [working_set, private, peak] => format!(
                "working set {} MiB, private {} MiB, peak working set {} MiB",
                working_set / 1_048_576,
                private / 1_048_576,
                peak / 1_048_576
            ),
            _ => format!("unavailable ({text})"),
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        "unavailable".to_owned()
    }
}

struct Timings {
    samples: Vec<Duration>,
}

impl Timings {
    fn new() -> Self {
        Self {
            samples: Vec::new(),
        }
    }

    fn measure<T>(&mut self, work: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let result = work();
        self.samples.push(started.elapsed());
        result
    }

    fn summary(&mut self) -> String {
        if self.samples.is_empty() {
            return "no samples".to_owned();
        }
        self.samples.sort();
        let count = self.samples.len();
        let total: Duration = self.samples.iter().sum();
        let pick = |index: usize| self.samples.get(index.min(count - 1)).copied();
        format!(
            "n={count} mean={:?} median={:?} p99={:?} max={:?}",
            total / u32::try_from(count).unwrap_or(u32::MAX),
            pick(count / 2).unwrap_or_default(),
            pick(count * 99 / 100).unwrap_or_default(),
            pick(count - 1).unwrap_or_default(),
        )
    }
}

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().collect();
    let (Some(root), Some(index_directory)) = (arguments.get(1), arguments.get(2)) else {
        anyhow::bail!("usage: symbol_index_bench <source-root> <index-dir> [threads]");
    };
    let root = PathBuf::from(root);
    let index_directory = PathBuf::from(index_directory);
    let threads = arguments.get(3).and_then(|threads| threads.parse().ok());
    let config = IndexConfig {
        parallelism: threads,
        ..IndexConfig::default()
    };
    if index_directory.exists() {
        std::fs::remove_dir_all(&index_directory)
            .with_context(|| format!("clearing {}", index_directory.display()))?;
    }
    println!("memory at start: {}", memory());

    let started = Instant::now();
    let files = scan_directory(&root)?;
    let source_bytes: u64 = files.iter().map(|file| file.size).sum();
    println!(
        "scan: {} files, {:.1} MiB in {:?}",
        files.len(),
        source_bytes as f64 / 1_048_576.0,
        started.elapsed()
    );

    let source = OverlaySource {
        base: DirectorySource::new(&root),
        overlay: Mutex::new(HashMap::new()),
    };
    let cancel = AtomicBool::new(false);
    {
        let index = SymbolIndex::open(&index_directory, config.clone())?;
        let stats = index.update(&files, &source, &|_| {}, &cancel)?;
        println!("initial index: {:?}", stats.elapsed);
        println!("  {stats:?}");
        println!("  {:?}", index.stats());
        println!("  memory: {}", memory());
    }

    let started = Instant::now();
    let index = SymbolIndex::open(&index_directory, config.clone())?;
    let opened = started.elapsed();
    let rescan_started = Instant::now();
    let files = scan_directory(&root)?;
    let rescan = rescan_started.elapsed();
    let stats = index.update(&files, &source, &|_| {}, &cancel)?;
    println!(
        "restart: open {opened:?} (checksums verified), rescan {rescan:?}, update {:?}, total {:?}",
        stats.elapsed,
        started.elapsed()
    );
    println!("  {stats:?}");
    println!("  memory after reopen: {}", memory());

    drop(index);
    let started = Instant::now();
    let index = SymbolIndex::open(
        &index_directory,
        IndexConfig {
            verify_checksums: false,
            ..config
        },
    )?;
    println!(
        "open without checksum verification: {:?}",
        started.elapsed()
    );

    // Sample function names from files spread over the tree.
    let indexed = index.files();
    let step = (indexed.len() / 500).max(1);
    let mut function_names = Vec::new();
    let mut function_ids = Vec::new();
    for path in indexed.iter().step_by(step) {
        if let Some(symbols) = index.file_symbols(path) {
            for definition in symbols.definitions.iter().take(3) {
                if definition.kind.is_function_like() && !definition.is_declaration {
                    function_names.push(definition.name.clone());
                    function_ids.push(definition.id);
                }
            }
        }
    }
    println!("query sample: {} functions", function_names.len());

    let mut definitions = Timings::new();
    let mut callers = Timings::new();
    let mut callees = Timings::new();
    let mut prefix = Timings::new();
    let mut substring = Timings::new();
    let mut fuzzy = Timings::new();
    let mut found_callers = 0usize;
    let mut truncated = 0usize;
    for name in &function_names {
        definitions.measure(|| index.definitions(name));
        let result = callers.measure(|| index.callers(name, 200));
        found_callers += result.calls.len();
        truncated += usize::from(result.truncated);
    }
    for id in &function_ids {
        callees.measure(|| index.callees(*id));
    }
    for name in function_names.iter().take(100) {
        let query: String = name.chars().take(4).collect();
        prefix.measure(|| index.search(&query, SearchMode::Prefix, 50));
        let query: String = name.chars().skip(1).take(5).collect();
        substring.measure(|| index.search(&query, SearchMode::Substring, 50));
        let query: String = name.chars().step_by(2).take(5).collect();
        fuzzy.measure(|| index.search(&query, SearchMode::Fuzzy, 50));
    }
    println!("definitions(name): {}", definitions.summary());
    println!(
        "callers(name, 200): {} ({} call sites, {} truncated)",
        callers.summary(),
        found_callers,
        truncated
    );
    println!("callees(id): {}", callees.summary());
    println!("search prefix: {}", prefix.summary());
    println!("search substring: {}", substring.summary());
    println!("search fuzzy: {}", fuzzy.summary());
    println!("memory after queries: {}", memory());

    // One edited file, as the file watcher would report it.
    if let Some(path) = indexed.get(indexed.len() / 2) {
        let original = source.base.read(path)?;
        let mut incremental = Timings::new();
        for iteration in 0..5 {
            // Different contents each time, so every round parses and writes a segment.
            let mut contents = original.clone();
            contents.extend_from_slice(
                format!("\nint bench_added_{iteration}(void) {{ return bench_callee(); }}\n")
                    .as_bytes(),
            );
            let entry = FileEntry {
                path: Arc::clone(path),
                mtime: Some(SystemTime::now()),
                size: contents.len() as u64,
            };
            if let Ok(mut overlay) = source.overlay.lock() {
                overlay.insert(path.to_string(), contents);
            }
            let stats =
                incremental.measure(|| index.update_files(&[entry], &[], &source, &cancel))?;
            if stats.files_parsed != 1 {
                println!("  unexpected: {stats:?}");
            }
        }
        let found = index.callers("bench_callee", 10).calls.len();
        println!(
            "update_files(1 changed file, {path}): {} -> callers of the added call: {found}",
            incremental.summary(),
        );
    }
    let stats = index.stats();
    println!("final: {stats:?}");
    println!(
        "index size: {:.1} MiB for {:.1} MiB of source",
        stats.disk_bytes as f64 / 1_048_576.0,
        source_bytes as f64 / 1_048_576.0
    );
    println!("memory at end: {}", memory());
    Ok(())
}
