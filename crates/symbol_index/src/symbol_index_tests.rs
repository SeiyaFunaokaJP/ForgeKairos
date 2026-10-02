use std::{
    collections::BTreeMap,
    sync::atomic::AtomicBool,
    time::{Duration, UNIX_EPOCH},
};

use super::*;

#[derive(Default)]
struct MemorySource {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemorySource {
    fn set(&self, path: &str, contents: &str) {
        self.files
            .lock()
            .insert(path.to_owned(), contents.as_bytes().to_vec());
    }

    fn remove(&self, path: &str) {
        self.files.lock().remove(path);
    }

    /// Entries with fixed timestamps far in the past, so that no entry is racy.
    fn entries(&self, mtime_seconds: u64) -> Vec<FileEntry> {
        self.files
            .lock()
            .iter()
            .map(|(path, contents)| entry(path, contents.len(), mtime_seconds))
            .collect()
    }
}

impl FileSource for MemorySource {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.files
            .lock()
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

fn entry(path: &str, size: usize, mtime_seconds: u64) -> FileEntry {
    FileEntry {
        path: path.into(),
        mtime: Some(UNIX_EPOCH + Duration::from_secs(mtime_seconds)),
        size: size as u64,
    }
}

fn config() -> IndexConfig {
    IndexConfig {
        parallelism: Some(2),
        ..IndexConfig::default()
    }
}

fn full_update(index: &SymbolIndex, source: &MemorySource, mtime_seconds: u64) -> UpdateStats {
    index
        .update(
            &source.entries(mtime_seconds),
            source,
            &|_| {},
            &AtomicBool::new(false),
        )
        .expect("update")
}

fn change_file(
    index: &SymbolIndex,
    source: &MemorySource,
    path: &str,
    contents: &str,
    mtime_seconds: u64,
) -> UpdateStats {
    source.set(path, contents);
    index
        .update_files(
            &[entry(path, contents.len(), mtime_seconds)],
            &[],
            source,
            &AtomicBool::new(false),
        )
        .expect("update_files")
}

fn remove_file(index: &SymbolIndex, source: &MemorySource, path: &str) -> UpdateStats {
    source.remove(path);
    index
        .update_files(&[], &[path], source, &AtomicBool::new(false))
        .expect("update_files")
}

fn definition_places(index: &SymbolIndex, name: &str) -> Vec<(String, DefinitionKind, bool, u32)> {
    index
        .definitions(name)
        .into_iter()
        .map(|definition| {
            (
                definition.path.to_string(),
                definition.kind,
                definition.is_declaration,
                definition.name_range.start.row,
            )
        })
        .collect()
}

fn caller_names(index: &SymbolIndex, name: &str) -> Vec<(String, Option<String>)> {
    index
        .callers(name, usize::MAX)
        .calls
        .into_iter()
        .map(|call| (call.path.to_string(), call.caller.map(|caller| caller.name)))
        .collect()
}

/// Everything the index answers, without ids (which differ between equivalent indexes).
fn dump(index: &SymbolIndex) -> String {
    let mut output = String::new();
    let mut names = std::collections::BTreeSet::new();
    for path in index.files() {
        let symbols = index.file_symbols(&path).expect("listed file");
        output.push_str(&format!(
            "file {path} {:?} errors={} failed={}\n",
            symbols.language, symbols.has_syntax_errors, symbols.parse_failed
        ));
        for definition in &symbols.definitions {
            names.insert(definition.name.clone());
            output.push_str(&format!(
                "  def {} {:?} static={} decl={} {:?} {:?}\n",
                definition.name,
                definition.kind,
                definition.is_static,
                definition.is_declaration,
                definition.name_range,
                definition.item_range
            ));
            let callees = index.callees(definition.id).expect("live definition");
            for callee in callees {
                output.push_str(&format!(
                    "    calls {} {:?} {:?} macro={} {:?}\n",
                    callee.callee, callee.kind, callee.target, callee.in_macro_body, callee.range
                ));
            }
        }
        for call in &symbols.calls {
            names.insert(call.callee.clone());
            output.push_str(&format!(
                "  call {} {:?} from={:?} {:?}\n",
                call.callee,
                call.kind,
                call.caller.as_ref().map(|caller| &caller.name),
                call.range
            ));
        }
        for assignment in &symbols.assignments {
            names.insert(assignment.field.clone());
            output.push_str(&format!(
                "  wire {}.{} = {} {:?} {:?}\n",
                assignment.container.as_deref().unwrap_or("?"),
                assignment.field,
                assignment.function,
                assignment.kind,
                assignment.range
            ));
        }
    }
    for name in names {
        output.push_str(&format!("name {name}\n"));
        for definition in index.definitions(&name) {
            output.push_str(&format!(
                "  defined {} {:?}\n",
                definition.path, definition.name_range
            ));
        }
        for call in index.callers(&name, usize::MAX).calls {
            output.push_str(&format!(
                "  called {} {:?} from {:?}\n",
                call.path,
                call.range,
                call.caller.map(|caller| caller.name)
            ));
        }
        for assignment in index.assignments(&name) {
            output.push_str(&format!(
                "  field {} = {}\n",
                assignment.path, assignment.function
            ));
        }
        for assignment in index.assignments_of_function(&name) {
            output.push_str(&format!(
                "  stored {} .{}\n",
                assignment.path, assignment.field
            ));
        }
    }
    let stats = index.stats();
    output.push_str(&format!(
        "files={} definitions={} calls={} assignments={} errors={} failed={}\n",
        stats.files,
        stats.definitions,
        stats.calls,
        stats.assignments,
        stats.files_with_syntax_errors,
        stats.files_parse_failed
    ));
    output
}

const DRIVER: &str = "#include \"ops.h\"\n\
    static int helper(void) { return 1; }\n\
    static int foo_open(struct inode *inode) { return helper() + shared_util(inode); }\n\
    static const struct file_operations foo_fops = { .open = foo_open };\n\
    int foo_register(void) { return register_ops(&foo_fops); }\n";

const OTHER: &str = "static int helper(void) { return 2; }\n\
    int shared_util(struct inode *inode) { return helper(); }\n\
    #define CALL_OPEN(ops, inode) ((ops)->open(inode))\n\
    #define MAKE(x) make_thing(x)\n\
    int dispatch(struct file_operations *ops, struct inode *inode) {\n\
    \treturn CALL_OPEN(ops, inode) + ops->open(inode) + MAKE(1) + printf(\"x\");\n\
    }\n";

const HEADER: &str = "struct file_operations { int (*open)(struct inode *); };\n\
    int shared_util(struct inode *inode);\n\
    int register_ops(const struct file_operations *ops);\n";

fn sample_source() -> MemorySource {
    let source = MemorySource::default();
    source.set("drivers/foo.c", DRIVER);
    source.set("lib/other.c", OTHER);
    source.set("include/ops.h", HEADER);
    source
}

#[test]
fn queries_answer_from_the_index() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = sample_source();
    let stats = full_update(&index, &source, 1_000);
    assert_eq!(stats.files_parsed, 3);
    assert!(!stats.cancelled);

    // Two static functions with the same name stay distinct.
    let helpers = index.definitions("helper");
    assert_eq!(helpers.len(), 2);
    assert!(helpers.iter().all(|definition| definition.is_static));
    assert_eq!(
        helpers
            .iter()
            .map(|definition| definition.path.as_ref())
            .collect::<Vec<_>>(),
        vec!["drivers/foo.c", "lib/other.c"]
    );

    // Definitions sort before declarations.
    assert_eq!(
        definition_places(&index, "shared_util"),
        vec![
            ("lib/other.c".to_owned(), DefinitionKind::Function, false, 1),
            (
                "include/ops.h".to_owned(),
                DefinitionKind::Function,
                true,
                1
            ),
        ]
    );

    assert_eq!(
        caller_names(&index, "helper"),
        vec![
            ("drivers/foo.c".to_owned(), Some("foo_open".to_owned())),
            ("lib/other.c".to_owned(), Some("shared_util".to_owned())),
        ]
    );

    let limited = index.callers("helper", 1);
    assert_eq!(limited.calls.len(), 1);
    assert!(limited.truncated);
    assert!(!index.callers("helper", 2).truncated);

    let dispatch = index
        .definitions("dispatch")
        .into_iter()
        .next()
        .expect("dispatch");
    let callees: Vec<(String, CalleeTarget)> = index
        .callees(dispatch.id)
        .expect("live")
        .into_iter()
        .map(|callee| (callee.callee, callee.target))
        .collect();
    assert_eq!(
        callees,
        vec![
            ("CALL_OPEN".to_owned(), CalleeTarget::Macro),
            ("open".to_owned(), CalleeTarget::Indirect),
            ("MAKE".to_owned(), CalleeTarget::Macro),
            ("printf".to_owned(), CalleeTarget::Unresolved),
        ]
    );
    let foo_open = index
        .definitions("foo_open")
        .into_iter()
        .next()
        .expect("foo_open");
    let callees: Vec<(String, CalleeTarget)> = index
        .callees(foo_open.id)
        .expect("live")
        .into_iter()
        .map(|callee| (callee.callee, callee.target))
        .collect();
    assert_eq!(
        callees,
        vec![
            ("helper".to_owned(), CalleeTarget::Function),
            ("shared_util".to_owned(), CalleeTarget::Function),
        ]
    );

    // Calls in macro bodies belong to the macro.
    let make_callers = index.callers("make_thing", 10);
    assert_eq!(make_callers.calls.len(), 1);
    assert!(make_callers.calls[0].in_macro_body);
    assert_eq!(
        make_callers.calls[0]
            .caller
            .as_ref()
            .map(|caller| caller.kind),
        Some(DefinitionKind::FunctionMacro)
    );

    let wiring = index.assignments("open");
    assert_eq!(wiring.len(), 1);
    assert_eq!(wiring[0].function, "foo_open");
    assert_eq!(wiring[0].container.as_deref(), Some("file_operations"));
    assert_eq!(wiring[0].path.as_ref(), "drivers/foo.c");
    let stored = index.assignments_of_function("foo_open");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].field, "open");

    let enclosing = index
        .enclosing_definition(
            "lib/other.c",
            OTHER.find("MAKE(1)").expect("in source") as u32,
        )
        .expect("inside dispatch");
    assert_eq!(enclosing.name, "dispatch");
    assert!(
        index
            .enclosing_definition("lib/other.c", 0)
            .is_some_and(|definition| definition.name == "helper")
    );
    assert!(index.enclosing_definition("missing.c", 0).is_none());

    let stats = index.stats();
    assert_eq!(stats.files, 3);
    assert!(stats.disk_bytes > 0);
}

#[test]
fn search_modes() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = MemorySource::default();
    source.set(
        "a.c",
        "int ext4_file_open(void) { return 0; }\n\
         int ext4_file_write(void) { return 0; }\n\
         int Ext4Other(void) { return 0; }\n\
         int open(void) { return 0; }\n\
         int reopen_file(void) { return 0; }\n\
         int called_only(void) { return not_defined_anywhere(); }\n",
    );
    full_update(&index, &source, 1_000);
    let names = |mode: SearchMode, query: &str| -> Vec<String> {
        index
            .search(query, mode, 10)
            .into_iter()
            .map(|found| found.definition.name)
            .collect()
    };
    assert_eq!(
        names(SearchMode::Prefix, "EXT4"),
        vec!["Ext4Other", "ext4_file_open", "ext4_file_write"]
    );
    assert_eq!(names(SearchMode::Prefix, "open"), vec!["open"]);
    assert_eq!(names(SearchMode::Prefix, "zzz"), Vec::<String>::new());
    assert_eq!(
        names(SearchMode::Substring, "open"),
        vec!["open", "ext4_file_open", "reopen_file"]
    );
    let fuzzy = names(SearchMode::Fuzzy, "efo");
    assert_eq!(fuzzy.first().map(String::as_str), Some("ext4_file_open"));
    assert!(!fuzzy.contains(&"open".to_owned()));
    // Names that are only called are not search results.
    assert!(names(SearchMode::Substring, "not_defined").is_empty());
    assert_eq!(index.search("", SearchMode::Prefix, 2).len(), 2);
}

#[test]
fn reopening_gives_the_same_answers_without_reparsing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let source = sample_source();
    let before = {
        let index = SymbolIndex::open(directory.path(), config()).expect("open");
        full_update(&index, &source, 1_000);
        dump(&index)
    };
    let index = SymbolIndex::open(directory.path(), config()).expect("reopen");
    assert_eq!(index.reset_reason(), None);
    assert_eq!(dump(&index), before);
    let stats = full_update(&index, &source, 1_000);
    assert_eq!(stats.files_unchanged, 3);
    assert_eq!(stats.files_parsed, 0);
    assert_eq!(dump(&index), before);
}

#[test]
fn timestamp_only_changes_do_not_reparse() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = sample_source();
    full_update(&index, &source, 1_000);
    let before = dump(&index);
    let stats = full_update(&index, &source, 2_000);
    assert_eq!(stats.files_touched, 3);
    assert_eq!(stats.files_parsed, 0);
    assert_eq!(dump(&index), before);
    // The new timestamps were recorded, including across a reopen.
    drop(index);
    let index = SymbolIndex::open(directory.path(), config()).expect("reopen");
    let stats = full_update(&index, &source, 2_000);
    assert_eq!(stats.files_unchanged, 3);
}

#[test]
fn recently_modified_files_are_rehashed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = MemorySource::default();
    source.set("a.c", "int first(void) { return 0; }\n");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    full_update(&index, &source, now);
    // Same size, same timestamp, different contents: only the racy check catches this.
    source.set("a.c", "int other(void) { return 0; }\n");
    let stats = full_update(&index, &source, now);
    assert_eq!(stats.files_parsed, 1);
    assert_eq!(definition_places(&index, "first"), vec![]);
    assert_eq!(definition_places(&index, "other").len(), 1);
}

#[test]
fn incremental_updates_replace_and_remove_files() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = sample_source();
    full_update(&index, &source, 1_000);
    let segments_before = index.stats().segments;

    let stats = change_file(
        &index,
        &source,
        "lib/other.c",
        "int shared_util(struct inode *inode) { return renamed(); }\n",
        1_500,
    );
    assert_eq!(stats.files_parsed, 1);
    assert_eq!(stats.segments_written, 1);
    assert_eq!(index.stats().segments, segments_before + 1);
    assert_eq!(index.definitions("helper").len(), 1);
    assert_eq!(index.definitions("dispatch"), vec![]);
    assert_eq!(
        caller_names(&index, "renamed"),
        vec![("lib/other.c".to_owned(), Some("shared_util".to_owned()))]
    );
    assert_eq!(
        caller_names(&index, "helper"),
        vec![("drivers/foo.c".to_owned(), Some("foo_open".to_owned()))]
    );

    let stats = remove_file(&index, &source, "drivers/foo.c");
    assert_eq!(stats.files_removed, 1);
    assert_eq!(index.definitions("helper"), vec![]);
    assert_eq!(index.callers("helper", 10).calls, vec![]);
    assert_eq!(index.assignments("open"), vec![]);
    assert_eq!(index.search("foo", SearchMode::Prefix, 10), vec![]);
    assert!(index.file_symbols("drivers/foo.c").is_none());
    assert_eq!(index.stats().files, 2);

    // A full update notices files that disappeared without an event.
    source.remove("include/ops.h");
    let stats = full_update(&index, &source, 1_500);
    assert_eq!(stats.files_removed, 1);
    assert_eq!(definition_places(&index, "register_ops"), vec![]);
    assert_eq!(index.files(), vec![Arc::<str>::from("lib/other.c")]);

    // And the removals survive a reopen.
    drop(index);
    let index = SymbolIndex::open(directory.path(), config()).expect("reopen");
    assert_eq!(index.files(), vec![Arc::<str>::from("lib/other.c")]);
    assert_eq!(index.definitions("helper"), vec![]);
}

#[test]
fn stale_definition_ids_return_nothing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = sample_source();
    full_update(&index, &source, 1_000);
    let dispatch = index.definitions("dispatch").remove(0);
    assert!(index.definition(dispatch.id).is_some());
    change_file(&index, &source, "lib/other.c", "int unrelated;\n", 1_100);
    assert_eq!(index.definition(dispatch.id), None);
    assert_eq!(index.callees(dispatch.id), None);
}

#[test]
fn oversized_and_unsupported_files_are_skipped_and_counted() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(
        directory.path(),
        IndexConfig {
            max_file_bytes: 64,
            ..config()
        },
    )
    .expect("open");
    let source = MemorySource::default();
    source.set("small.c", "int small(void) { return 0; }\n");
    source.set("big.c", &"int big(void) { return 0; }\n".repeat(10));
    source.set("notes.txt", "int text(void);\n");
    let stats = full_update(&index, &source, 1_000);
    assert_eq!(stats.files_parsed, 1);
    assert_eq!(stats.files_skipped_too_large, 1);
    assert_eq!(stats.files_skipped_unsupported, 1);
    assert_eq!(index.definitions("big"), vec![]);
    let totals = index.stats();
    assert_eq!(totals.skipped_too_large, 1);
    assert_eq!(totals.skipped_unsupported, 1);

    // A file that grows past the limit leaves the index.
    let grown = "int small(void) { return 0; }\n".repeat(10);
    let stats = change_file(&index, &source, "small.c", &grown, 1_100);
    assert_eq!(stats.files_skipped_too_large, 1);
    assert_eq!(index.definitions("small"), vec![]);
}

fn corrupt(path: &Path, mutate: impl FnOnce(&mut Vec<u8>)) {
    let mut bytes = fs::read(path).expect("read");
    mutate(&mut bytes);
    fs::write(path, bytes).expect("write");
}

fn segment_paths(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(directory)
        .expect("list")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(parse_segment_file_name)
                .is_some()
        })
        .collect();
    paths.sort();
    paths
}

#[test]
fn damaged_or_outdated_indexes_are_rebuilt() {
    let source = sample_source();
    let cases: Vec<(&str, Box<dyn Fn(&Path)>)> = vec![
        (
            "garbage manifest",
            Box::new(|directory: &Path| {
                fs::write(directory.join(MANIFEST_FILE_NAME), b"not an index").expect("write")
            }),
        ),
        (
            "flipped manifest byte",
            Box::new(|directory: &Path| {
                corrupt(&directory.join(MANIFEST_FILE_NAME), |bytes| bytes[20] ^= 1)
            }),
        ),
        (
            "manifest from another version",
            Box::new(|directory: &Path| {
                let path = directory.join(MANIFEST_FILE_NAME);
                corrupt(&path, |bytes| {
                    bytes[8..12].copy_from_slice(&(storage::FORMAT_VERSION + 1).to_le_bytes());
                    let body_length = bytes.len() - 8;
                    let sum = storage::checksum(&bytes[..body_length]);
                    bytes[body_length..].copy_from_slice(&sum.to_le_bytes());
                })
            }),
        ),
        (
            "segment from another version",
            Box::new(|directory: &Path| {
                corrupt(&segment_paths(directory)[0], |bytes| {
                    bytes[8..12].copy_from_slice(&(storage::FORMAT_VERSION + 1).to_le_bytes())
                })
            }),
        ),
        (
            "index from format version 2",
            Box::new(|directory: &Path| {
                // Version 2 indexes lack what the extraction now records, so they must not be
                // read as current ones.
                let previous = 2u32.to_le_bytes();
                corrupt(&directory.join(MANIFEST_FILE_NAME), |bytes| {
                    bytes[8..12].copy_from_slice(&previous);
                    refresh_manifest_checksum(bytes);
                });
                for path in segment_paths(directory) {
                    corrupt(&path, |bytes| {
                        bytes[8..12].copy_from_slice(&previous);
                        refresh_segment_checksum(bytes);
                    });
                }
            }),
        ),
        (
            "truncated segment",
            Box::new(|directory: &Path| {
                corrupt(&segment_paths(directory)[0], |bytes| {
                    bytes.truncate(bytes.len() / 2)
                })
            }),
        ),
        (
            "flipped segment byte",
            Box::new(|directory: &Path| {
                corrupt(&segment_paths(directory)[0], |bytes| {
                    let middle = bytes.len() / 2;
                    bytes[middle] ^= 0x40;
                })
            }),
        ),
        (
            "missing segment",
            Box::new(|directory: &Path| {
                fs::remove_file(&segment_paths(directory)[0]).expect("remove")
            }),
        ),
    ];
    for (name, damage) in cases {
        let directory = tempfile::tempdir().expect("tempdir");
        {
            let index = SymbolIndex::open(directory.path(), config()).expect("open");
            full_update(&index, &source, 1_000);
        }
        damage(directory.path());
        let index = SymbolIndex::open(directory.path(), config())
            .unwrap_or_else(|error| panic!("{name}: open failed: {error:#}"));
        assert!(index.reset_reason().is_some(), "{name}: not detected");
        if name == "index from format version 2" {
            let reason = index.reset_reason().unwrap_or_default();
            assert!(reason.contains("format version 2"), "{reason}");
        }
        assert_eq!(index.stats().files, 0, "{name}");
        assert_eq!(index.definitions("helper"), vec![], "{name}");
        let stats = full_update(&index, &source, 1_000);
        assert_eq!(stats.files_parsed, 3, "{name}");
        assert_eq!(index.definitions("helper").len(), 2, "{name}");
    }
}

#[test]
fn a_second_open_of_the_same_directory_is_refused() {
    let directory = tempfile::tempdir().expect("tempdir");
    let _index = SymbolIndex::open(directory.path(), config()).expect("open");
    let error = SymbolIndex::open(directory.path(), config())
        .err()
        .expect("second open must fail");
    assert!(format!("{error:#}").contains("in use"), "{error:#}");
}

#[test]
fn cancellation_keeps_committed_work_and_resumes() {
    let directory = tempfile::tempdir().expect("tempdir");
    let small_segments = IndexConfig {
        segment_file_limit: 2,
        parallelism: Some(1),
        ..IndexConfig::default()
    };
    let index = SymbolIndex::open(directory.path(), small_segments.clone()).expect("open");
    let source = MemorySource::default();
    for number in 0..10 {
        source.set(
            &format!("file{number:02}.c"),
            &format!("int function{number}(void) {{ return callee{number}(); }}\n"),
        );
    }

    let cancel = AtomicBool::new(true);
    let stats = index
        .update(&source.entries(1_000), &source, &|_| {}, &cancel)
        .expect("update");
    assert!(stats.cancelled);
    assert_eq!(index.stats().files, 0);

    let cancel = AtomicBool::new(false);
    let stats = index
        .update(
            &source.entries(1_000),
            &source,
            &|progress| {
                if progress.completed >= 5 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        )
        .expect("update");
    assert!(stats.cancelled);
    let partial = index.stats().files;
    assert!(partial >= 2 && partial < 10, "{partial} files committed");
    assert_eq!(index.stats().files as usize, stats.files_parsed);

    // The committed part survives a restart, and the next update finishes the rest.
    drop(index);
    let index = SymbolIndex::open(directory.path(), small_segments).expect("reopen");
    assert_eq!(index.stats().files, partial);
    let stats = full_update(&index, &source, 1_000);
    assert!(!stats.cancelled);
    assert_eq!(stats.files_parsed, 10 - partial);
    assert_eq!(stats.files_unchanged, partial);
    assert_eq!(index.stats().files, 10);
    for number in 0..10 {
        assert_eq!(
            caller_names(&index, &format!("callee{number}")),
            vec![(
                format!("file{number:02}.c"),
                Some(format!("function{number}"))
            )]
        );
    }
}

/// A small deterministic generator so the property test needs no extra dependency.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

fn random_file(random: &mut Random, number: u64) -> String {
    let mut text = String::new();
    let functions = 1 + random.below(4);
    for function in 0..functions {
        let name = format!("fn_{}_{}", random.below(6), function);
        let is_static = if random.below(2) == 0 { "static " } else { "" };
        text.push_str(&format!("{is_static}int {name}(struct ops *ops) {{\n"));
        for _ in 0..random.below(4) {
            match random.below(3) {
                0 => text.push_str(&format!("\tcommon_{}();\n", random.below(5))),
                1 => text.push_str(&format!("\tops->slot_{}(1);\n", random.below(3))),
                _ => text.push_str(&format!("\tMACRO_{}(x);\n", random.below(3))),
            }
        }
        text.push_str("\treturn 0;\n}\n");
    }
    if random.below(2) == 0 {
        text.push_str(&format!(
            "#define MACRO_{}(x) helper_{}(x)\n",
            random.below(3),
            number % 3
        ));
    }
    if random.below(2) == 0 {
        text.push_str(&format!(
            "static const struct ops table_{number} = {{ .slot_{} = fn_{}_0 }};\n",
            random.below(3),
            random.below(6)
        ));
    }
    if random.below(4) == 0 {
        text.push_str("int broken( {{ \n");
    }
    text
}

#[test]
fn incremental_updates_match_a_rebuild_from_scratch() {
    let small_segments = IndexConfig {
        segment_file_limit: 8,
        parallelism: Some(2),
        ..IndexConfig::default()
    };
    for seed in [1u64, 7, 42] {
        let mut random = Random(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let directory = tempfile::tempdir().expect("tempdir");
        let mut index = SymbolIndex::open(directory.path(), small_segments.clone()).expect("open");
        let source = MemorySource::default();
        for number in 0..20 {
            source.set(
                &format!("src/file{number:02}.c"),
                &random_file(&mut random, number),
            );
        }
        full_update(&index, &source, 1_000);
        let mut compactions = 0;
        for step in 0..60u64 {
            let number = random.below(26);
            let path = format!("src/file{number:02}.c");
            let mtime = 2_000 + step;
            let stats = match random.below(5) {
                0 => remove_file(&index, &source, &path),
                1 => {
                    // Several files at once, as after a branch switch.
                    let mut entries = Vec::new();
                    for offset in 0..3 {
                        let number = (number + offset) % 26;
                        let path = format!("src/file{number:02}.c");
                        let contents = random_file(&mut random, number);
                        source.set(&path, &contents);
                        entries.push(entry(&path, contents.len(), mtime));
                    }
                    index
                        .update_files(&entries, &[], &source, &AtomicBool::new(false))
                        .expect("update_files")
                }
                2 if step % 10 == 0 => full_update(&index, &source, mtime),
                _ => {
                    let contents = random_file(&mut random, number);
                    change_file(&index, &source, &path, &contents, mtime)
                }
            };
            compactions += stats.segments_compacted;
            if step % 20 == 19 {
                // Reopening in the middle must not change anything either.
                let before = dump(&index);
                drop(index);
                index =
                    SymbolIndex::open(directory.path(), small_segments.clone()).expect("reopen");
                assert_eq!(dump(&index), before, "seed {seed} step {step}");
            }
        }
        assert!(compactions > 0, "seed {seed}: compaction never ran");
        compare_with_rebuild(&index, &source, &small_segments, seed);
    }
}

fn compare_with_rebuild(
    index: &SymbolIndex,
    source: &MemorySource,
    config: &IndexConfig,
    seed: u64,
) {
    let fresh_directory = tempfile::tempdir().expect("tempdir");
    let fresh = SymbolIndex::open(fresh_directory.path(), config.clone()).expect("open fresh");
    full_update(&fresh, source, 1_000);
    assert_eq!(dump(index), dump(&fresh), "seed {seed}");
}

#[test]
fn recorded_timestamps_survive_compaction() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(
        directory.path(),
        IndexConfig {
            segment_file_limit: 8,
            ..config()
        },
    )
    .expect("open");
    let source = MemorySource::default();
    for name in ["a", "b", "c", "d", "e"] {
        source.set(
            &format!("{name}.c"),
            &format!("int {name}(void) {{ return 0; }}\n"),
        );
    }
    full_update(&index, &source, 1_000);
    assert_eq!(full_update(&index, &source, 2_000).files_touched, 5);

    let mut compacted = 0;
    for name in ["c", "d", "e"] {
        let stats = change_file(
            &index,
            &source,
            &format!("{name}.c"),
            &format!("int {name}_changed(void) {{ return 0; }}\n"),
            2_000,
        );
        compacted += stats.segments_compacted;
    }
    assert!(compacted > 0, "the first segment was not compacted");
    let stats = full_update(&index, &source, 2_000);
    assert_eq!(stats.files_unchanged, 5, "{stats:?}");
    assert_eq!(stats.files_touched, 0, "{stats:?}");
}

#[test]
fn callees_of_a_function_with_a_nested_method() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = MemorySource::default();
    source.set(
        "local.cpp",
        "void outer() {\n\
         \tbefore();\n\
         \tstruct Local { void inner() { nested(); } };\n\
         \tafter();\n\
         }\n",
    );
    full_update(&index, &source, 1_000);
    let callees = |name: &str| -> Vec<String> {
        let definition = index.definitions(name).remove(0);
        index
            .callees(definition.id)
            .expect("live")
            .into_iter()
            .map(|callee| callee.callee)
            .collect()
    };
    assert_eq!(callees("outer"), vec!["before", "after"]);
    assert_eq!(callees("inner"), vec!["nested"]);
}

#[test]
fn search_limit_is_not_taken_by_superseded_names() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = MemorySource::default();
    source.set("a.c", "int alpha_a(void) { return 0; }\n");
    source.set("b.c", "int alpha_b(void) { return 0; }\n");
    full_update(&index, &source, 1_000);
    change_file(&index, &source, "a.c", "int unrelated;\n", 1_100);
    for mode in [SearchMode::Prefix, SearchMode::Substring, SearchMode::Fuzzy] {
        let found: Vec<String> = index
            .search("alpha", mode, 1)
            .into_iter()
            .map(|found| found.definition.name)
            .collect();
        assert_eq!(found, vec!["alpha_b"], "{mode:?}");
    }
}

const SYNTHETIC_WORDS: &[&str] = &[
    "drm", "gem", "object", "init", "lock", "unlock", "page", "alloc", "free", "dev", "device",
    "net", "skb", "buf", "queue", "irq", "handler", "pci", "usb", "read", "write", "open", "close",
    "release", "probe", "remove", "set", "get", "put", "list", "head", "node", "tree", "map",
    "unmap", "dma", "mem", "cache", "flush", "inode", "file", "dentry", "super", "block", "bio",
    "request", "sched", "task", "thread", "timer", "work", "event", "notify", "power", "clock",
    "reg", "config", "state", "status", "error", "reset", "start", "stop", "enable", "disable",
    "count", "ref", "mutex", "spin", "atomic", "wait", "wake", "sync", "copy", "user", "kernel",
    "sys", "call", "ops", "info", "data", "ctx", "priv", "hw", "fw", "phy", "link", "port", "mode",
    "type", "flags", "attr", "show", "store", "debug", "trace", "stat", "rx", "tx", "ring", "desc",
    "entry", "table", "hash", "key", "value", "slot", "zone",
];

/// Writes segments of synthetic files straight to `directory` (skipping the parser), so that
/// an index with millions of names can be built in seconds. Names look like C identifiers:
/// mostly lower case with underscores, some upper-case macros and some CamelCase.
fn build_synthetic_index(directory: &Path, segments: u64, files: u64, names_per_file: u64) {
    let mut random = Random(0x2545_f491_4f6c_dd1d);
    let mut manifest = Manifest::default();
    let mut serial = 0u64;
    for segment in 0..segments {
        let mut inputs = Vec::new();
        for file in 0..files {
            let mut extract = ExtractedFile::default();
            for number in 0..names_per_file {
                let parts = 2 + random.below(3);
                let words: Vec<&str> = (0..parts)
                    .map(|_| SYNTHETIC_WORDS[random.below(SYNTHETIC_WORDS.len() as u64) as usize])
                    .collect();
                let name = match random.below(10) {
                    0 => format!("{}_{serial}", words.join("_").to_ascii_uppercase()),
                    1 => {
                        let mut camel = String::new();
                        for word in &words {
                            let mut characters = word.chars();
                            if let Some(first) = characters.next() {
                                camel.push(first.to_ascii_uppercase());
                                camel.extend(characters);
                            }
                        }
                        format!("{camel}{serial}")
                    }
                    _ => format!("{}_{serial}", words.join("_")),
                };
                serial += 1;
                let position = Position {
                    byte: number as u32 * 40,
                    row: number as u32,
                    column: 4,
                };
                // Half of the names are only called, as with library functions.
                if number % 2 == 0 {
                    extract.definitions.push(extract::ExtractedDefinition {
                        name,
                        kind: DefinitionKind::Function,
                        is_static: false,
                        is_declaration: false,
                        name_range: Range::token(position, 8),
                        item_range: Range::token(position, 8),
                    });
                } else {
                    extract.calls.push(extract::ExtractedCall {
                        callee: name,
                        caller: None,
                        kind: CallKind::Direct,
                        in_macro_body: false,
                        position,
                    });
                }
            }
            inputs.push(SegmentFileInput {
                path: format!("dir{segment}/file{file}.c").into(),
                mtime: 1_000_000_000,
                size: 1000,
                verified_at: 100_000_000_000,
                hash: [0; 16],
                language: SourceLanguage::C,
                extract,
            });
        }
        storage::write_segment(&Segment::path(directory, segment), 0, &inputs)
            .expect("writing a segment");
        manifest.segments.push(ManifestSegment {
            id: segment,
            file_count: files as u32,
            dead: vec![0; (files as usize).div_ceil(64)],
        });
    }
    manifest.next_segment_id = segments;
    write_atomically(&directory.join(MANIFEST_FILE_NAME), &manifest.encode())
        .expect("writing the manifest");
}

/// Measures substring and fuzzy search over about four million names. Run with
/// `cargo test --profile release-fast -p symbol_index --lib search_speed -- --ignored --nocapture`.
#[test]
#[ignore]
fn search_speed_over_four_million_names() {
    let directory = tempfile::tempdir().expect("tempdir");
    let started = Instant::now();
    build_synthetic_index(directory.path(), 40, 1000, 100);
    let index = SymbolIndex::open(
        directory.path(),
        IndexConfig {
            verify_checksums: false,
            ..IndexConfig::default()
        },
    )
    .expect("open");
    assert_eq!(index.reset_reason(), None);
    let stats = index.stats();
    let names: u64 = index
        .current()
        .segments
        .iter()
        .map(|view| u64::from(view.segment.name_count()))
        .sum();
    println!(
        "built {names} names in {} segments ({} MiB) in {:?}",
        stats.segments,
        stats.disk_bytes / 1_048_576,
        started.elapsed()
    );
    let queries = [
        (SearchMode::Substring, "lock"),
        (SearchMode::Substring, "page_alloc"),
        (SearchMode::Substring, "qzx"),
        (SearchMode::Substring, "e"),
        (SearchMode::Fuzzy, "pgal"),
        (SearchMode::Fuzzy, "drmgemi"),
        (SearchMode::Fuzzy, "kq"),
        (SearchMode::Prefix, "drm_g"),
    ];
    for (mode, query) in queries {
        let found = index.search(query, mode, 50).len();
        let mut timings: Vec<Duration> = (0..7)
            .map(|_| {
                let started = Instant::now();
                index.search(query, mode, 50);
                started.elapsed()
            })
            .collect();
        timings.sort();
        println!(
            "{mode:?} {query:?}: median {:?}, best {:?} ({found} results)",
            timings[timings.len() / 2],
            timings[0]
        );
    }
}

/// Substring and fuzzy search only score names containing the query's rarest byte, found by
/// scanning the packed name text; they must still return exactly the names that scoring every
/// name would, in the same order.
#[test]
fn substring_and_fuzzy_search_find_exactly_the_matching_names() {
    let directory = tempfile::tempdir().expect("tempdir");
    build_synthetic_index(directory.path(), 3, 20, 40);
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    assert_eq!(index.reset_reason(), None);
    let defined: std::collections::BTreeSet<String> = index
        .files()
        .iter()
        .flat_map(|path| index.file_symbols(path).expect("symbols").definitions)
        .map(|definition| definition.name)
        .collect();
    assert_eq!(defined.len(), 1200);

    let mut queries: Vec<String> = ["", "_", "Q", "zz", "9", "lock_", "LOCK", "UnLock", "ä"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    // Text spanning two neighbouring names in the packed text, which no single name contains.
    for view in &index.current().segments {
        let segment = &view.segment;
        let text = segment.names_text();
        for name_index in (0..segment.name_count() - 1).step_by(37) {
            let name = segment.name(name_index).expect("name");
            let end = (name.text.0 + name.text.1) as usize;
            queries.push(String::from_utf8_lossy(&text[end - 2..end + 2]).into_owned());
        }
    }
    let names: Vec<&String> = defined.iter().collect();
    let mut random = Random(5);
    for _ in 0..200 {
        let name = names[random.below(names.len() as u64) as usize].as_bytes();
        let start = random.below(name.len() as u64) as usize;
        let length = 1 + random.below((name.len() - start).min(8) as u64) as usize;
        let mut piece: Vec<u8> = name[start..start + length].to_vec();
        for byte in &mut piece {
            if random.below(3) == 0 {
                *byte = if byte.is_ascii_lowercase() {
                    byte.to_ascii_uppercase()
                } else {
                    byte.to_ascii_lowercase()
                };
            }
        }
        // Fuzzy queries skip some of the bytes.
        if random.below(2) == 0 && piece.len() > 2 {
            piece.remove(1);
        }
        queries.push(String::from_utf8(piece).expect("ASCII"));
    }

    let mut nonempty = 0;
    for query in &queries {
        let folded = query.to_ascii_lowercase();
        for mode in [SearchMode::Substring, SearchMode::Fuzzy] {
            let mut expected: Vec<(String, u32)> = defined
                .iter()
                .filter_map(|name| {
                    let score = match mode {
                        SearchMode::Substring => {
                            substring_score(name.as_bytes(), query.as_bytes(), folded.as_bytes())
                        }
                        _ => fuzzy_score(name.as_bytes(), folded.as_bytes()),
                    };
                    Some((name.clone(), score?))
                })
                .collect();
            expected.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
            let found = |limit: usize| -> Vec<(String, u32)> {
                index
                    .search(query, mode, limit)
                    .into_iter()
                    .map(|found| (found.definition.name, found.score))
                    .collect()
            };
            assert_eq!(found(100_000), expected, "{mode:?} {query:?}");
            expected.truncate(7);
            assert_eq!(found(7), expected, "{mode:?} {query:?} limited");
            if !expected.is_empty() {
                nonempty += 1;
            }
        }
    }
    assert!(nonempty > 300, "only {nonempty} queries matched anything");
}

/// Every name with a live definition in some segment, read record by record rather than from
/// the packed name text.
fn live_names(index: &SymbolIndex) -> std::collections::BTreeSet<String> {
    let snapshot = index.current();
    let mut names = std::collections::BTreeSet::new();
    for view in &snapshot.segments {
        let segment = &view.segment;
        for name_index in 0..segment.name_count() {
            let record = segment.name(name_index).expect("name record");
            let live = segment
                .definition_postings(record.definitions)
                .any(|definition| {
                    segment
                        .definition(definition)
                        .is_some_and(|definition| view.is_live(definition.file))
                });
            if live {
                let name = segment.string(record.text).expect("UTF-8 name");
                names.insert(name.to_owned());
            }
        }
    }
    names
}

/// Search as it was before it scanned the packed name text: every live name is scored, and the
/// names are ranked by score and then by name. Qualified names answer qualified queries only.
fn rank_every_name(
    names: &std::collections::BTreeSet<String>,
    query: &str,
    mode: SearchMode,
) -> Vec<(String, u32)> {
    let folded: Vec<u8> = query
        .bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .collect();
    let mut ranked: Vec<(String, u32)> = names
        .iter()
        .filter(|name| query.contains("::") || !name.contains(':'))
        .filter_map(|name| {
            let name_bytes = name.as_bytes();
            let score = match mode {
                SearchMode::Prefix => starts_with_folded(name_bytes, &folded)
                    .then(|| prefix_score(name_bytes, query.as_bytes()))
                    .flatten(),
                SearchMode::Substring => substring_score(name_bytes, query.as_bytes(), &folded),
                SearchMode::Fuzzy => fuzzy_score(name_bytes, &folded),
            };
            Some((name.clone(), score?))
        })
        .collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked
}

fn expand_ranked(
    index: &SymbolIndex,
    query: &str,
    ranked: &[(String, u32)],
    limit: usize,
) -> Vec<(Definition, u32)> {
    if query.contains("::") {
        return merge_places(index, query, ranked, limit);
    }
    let mut matches = Vec::new();
    for (name, score) in ranked {
        for definition in index.definitions(name) {
            if matches.len() >= limit {
                return matches;
            }
            matches.push((definition, *score));
        }
    }
    matches
}

/// A qualified query lists each place (file and name range) once, under its name closest to the
/// query: the query itself, else the best scored, else the shortest (else the first by name).
/// Places are ordered by that name's score, then its length and the name, then as
/// [`SymbolIndex::definitions`] orders them.
fn merge_places(
    index: &SymbolIndex,
    query: &str,
    ranked: &[(String, u32)],
    limit: usize,
) -> Vec<(Definition, u32)> {
    type Closeness = (
        bool,
        u32,
        std::cmp::Reverse<usize>,
        std::cmp::Reverse<String>,
    );
    let mut places: BTreeMap<(Arc<str>, Position, Position), (Closeness, usize, Definition, u32)> =
        BTreeMap::new();
    for (name, score) in ranked {
        for (order, definition) in index.definitions(name).into_iter().enumerate() {
            let closeness = (
                name == query,
                *score,
                std::cmp::Reverse(name.len()),
                std::cmp::Reverse(name.clone()),
            );
            let place = (
                definition.path.clone(),
                definition.name_range.start,
                definition.name_range.end,
            );
            let closer = places
                .get(&place)
                .is_none_or(|(kept, ..)| closeness > *kept);
            if closer {
                places.insert(place, (closeness, order, definition, *score));
            }
        }
    }
    let mut kept: Vec<(Closeness, usize, Definition, u32)> = places.into_values().collect();
    kept.sort_by(|left, right| {
        (
            std::cmp::Reverse(left.3),
            left.2.name.len(),
            &left.2.name,
            left.1,
        )
            .cmp(&(
                std::cmp::Reverse(right.3),
                right.2.name.len(),
                &right.2.name,
                right.1,
            ))
    });
    kept.into_iter()
        .take(limit)
        .map(|(_, _, definition, score)| (definition, score))
        .collect()
}

/// The packed-text scan must agree with scoring every name on names that make it hard: letter
/// case, `_`, digits, multi-byte characters, empty and very long names, the same name in several
/// files and segments, names that are only called or assigned, deleted and replaced files, and a
/// segment large enough to be split into several runs; and C++ functions recorded again under
/// qualified names, which answer qualified queries only and are listed once per place.
#[test]
fn search_agrees_with_scoring_every_name() {
    const SCOPES: &[&str] = &["Box", "outer", "ns", "Ä", "box"];
    const PIECES: &[&str] = &[
        "a", "e", "z", "q", "Q", "E", "_", "__", "0", "7", "ä", "Ä", "漢", "😀", "lock", "LOCK",
        "Lock", "x_y",
    ];
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    let piece = |random: &mut Random| PIECES[random.below(PIECES.len() as u64) as usize];
    let pool: Vec<String> = (0..40_000)
        .map(|_| match random.below(300) {
            0 => String::new(),
            1..=3 => piece(&mut random).repeat(120),
            _ => (0..1 + random.below(6))
                .map(|_| piece(&mut random))
                .collect(),
        })
        .collect();

    let directory = tempfile::tempdir().expect("tempdir");
    let mut manifest = Manifest::default();
    let mut live_paths: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut dead: Vec<Vec<u64>> = Vec::new();
    let shapes: Vec<(usize, usize)> = [(100, 900)]
        .into_iter()
        .chain(std::iter::repeat_n((8, 60), 5))
        .collect();
    for (segment, &(files, entries)) in shapes.iter().enumerate() {
        dead.push(vec![0; files.div_ceil(64)]);
        let mut inputs = Vec::new();
        for file in 0..files {
            let earlier: Vec<&String> = live_paths
                .iter()
                .filter(|(_, location)| location.0 != segment)
                .map(|(path, _)| path)
                .collect();
            let reused = earlier
                .get(random.below(earlier.len().max(1) as u64) as usize)
                .filter(|_| random.below(4) == 0)
                .map(|path| (*path).clone());
            let path = match reused {
                Some(path) => {
                    if let Some((old_segment, old_file)) = live_paths.remove(&path) {
                        dead[old_segment][old_file / 64] |= 1 << (old_file % 64);
                    }
                    path
                }
                None => format!("s{segment}/f{file}.c"),
            };
            live_paths.insert(path.clone(), (segment, file));
            let mut extract = ExtractedFile::default();
            for number in 0..entries {
                let name = pool[random.below(pool.len() as u64) as usize].clone();
                let position = Position {
                    byte: number as u32 * 10,
                    row: number as u32,
                    column: 0,
                };
                match random.below(10) {
                    0..=5 => {
                        let definition = extract::ExtractedDefinition {
                            name,
                            kind: DefinitionKind::Function,
                            is_static: false,
                            is_declaration: random.below(4) == 0,
                            name_range: Range::token(position, 4),
                            item_range: Range::token(position, 4),
                        };
                        // As the extraction records a C++ member: its own name, then the same
                        // place under up to four qualified names, innermost scope first.
                        let scopes: Vec<&str> = match random.below(3) {
                            0 => (0..1 + random.below(MAX_QUALIFIED_NAMES as u64))
                                .map(|_| SCOPES[random.below(SCOPES.len() as u64) as usize])
                                .collect(),
                            _ => Vec::new(),
                        };
                        let qualified: Vec<String> = (0..scopes.len())
                            .rev()
                            .map(|start| {
                                format!("{}::{}", scopes[start..].join("::"), definition.name)
                            })
                            .collect();
                        extract.definitions.push(definition.clone());
                        for name in qualified {
                            extract.definitions.push(extract::ExtractedDefinition {
                                name,
                                ..definition.clone()
                            });
                        }
                    }
                    6..=8 => extract.calls.push(extract::ExtractedCall {
                        callee: name,
                        caller: None,
                        kind: CallKind::Direct,
                        in_macro_body: false,
                        position,
                    }),
                    _ => extract.assignments.push(extract::ExtractedAssignment {
                        field: name,
                        value: pool[random.below(pool.len() as u64) as usize].clone(),
                        container: None,
                        kind: AssignmentKind::Assignment,
                        position,
                    }),
                }
            }
            inputs.push(SegmentFileInput {
                path: path.into(),
                mtime: 1_000_000_000,
                size: 1000,
                verified_at: 100_000_000_000,
                hash: [0; 16],
                language: SourceLanguage::C,
                extract,
            });
        }
        storage::write_segment(&Segment::path(directory.path(), segment as u64), 0, &inputs)
            .expect("writing a segment");
        // Files are deleted at random, too.
        if segment + 1 < shapes.len() {
            for file in 0..files {
                if random.below(6) == 0 {
                    dead[segment][file / 64] |= 1 << (file % 64);
                    live_paths.retain(|_, location| *location != (segment, file));
                }
            }
        }
    }
    for (segment, &(files, _)) in shapes.iter().enumerate() {
        manifest.segments.push(ManifestSegment {
            id: segment as u64,
            file_count: files as u32,
            dead: dead[segment].clone(),
        });
    }
    manifest.next_segment_id = shapes.len() as u64;
    write_atomically(
        &directory.path().join(MANIFEST_FILE_NAME),
        &manifest.encode(),
    )
    .expect("writing the manifest");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    assert_eq!(index.reset_reason(), None);
    let snapshot = index.current();
    let names = live_names(&index);
    assert!(snapshot.segments[0].segment.name_count() > SEARCH_RUN_NAMES);
    assert!(
        snapshot
            .segments
            .iter()
            .filter(|view| view.has_dead)
            .count()
            >= 3
    );

    let mut queries: Vec<String> = [
        "",
        "a",
        "A",
        "_",
        "ä",
        "Ä",
        "漢",
        "😀",
        "lock",
        "LOCK",
        "z_",
        "0",
        "q",
        "ä_",
        "漢q",
        "aaaa",
        "Ä😀",
        "__",
        "x_ylock",
        "::",
        "Box::",
        "box::",
        "::a",
        "outer::Box::",
        "ns::Ä::",
        "x::B",
        "Box::a",
        "BOX::LOCK",
        "s::o",
        ":",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for view in &snapshot.segments {
        let segment = &view.segment;
        let text = segment.names_text();
        let count = segment.name_count();
        for name_index in (0..count).step_by(4_001).chain([count - 1]) {
            let name = segment.name(name_index).expect("name");
            let end = (name.text.0 + name.text.1) as usize;
            // Across the boundary to the next name, and the end of the last name.
            for (before, after) in [(2, 2), (1, 1), (1, 0)] {
                if let Some(bytes) = text.get(end.saturating_sub(before)..end + after)
                    && let Ok(query) = std::str::from_utf8(bytes)
                {
                    queries.push(query.to_owned());
                }
            }
        }
    }
    // The names on either side of each split between runs, and the last of each segment.
    let mut live_at_splits = 0;
    for view in &snapshot.segments {
        let count = view.segment.name_count();
        let splits = (SEARCH_RUN_NAMES..count).step_by(SEARCH_RUN_NAMES as usize);
        for name_index in splits
            .flat_map(|split| [split - 1, split])
            .chain([count - 1])
        {
            let name = view.segment.name_text(name_index).expect("name");
            if names.contains(name) {
                live_at_splits += 1;
                queries.push(name.to_owned());
            }
        }
    }
    assert!(live_at_splits >= 6, "{live_at_splits} live names at splits");
    let qualified_names: Vec<&String> = names.iter().filter(|name| name.contains("::")).collect();
    assert!(qualified_names.len() > 1_000);
    for _ in 0..12 {
        let name = qualified_names[random.below(qualified_names.len() as u64) as usize];
        queries.push(name.clone());
        if let Some((_, unqualified)) = name.rsplit_once("::") {
            queries.push(format!("::{unqualified}"));
        }
    }
    for _ in 0..35 {
        let name: Vec<char> = pool[random.below(pool.len() as u64) as usize]
            .chars()
            .collect();
        if name.is_empty() {
            continue;
        }
        let start = random.below(name.len() as u64) as usize;
        let length = 1 + random.below((name.len() - start).min(6) as u64) as usize;
        let mut piece: Vec<char> = name[start..start + length]
            .iter()
            .map(|&character| match random.below(3) {
                0 => character.to_ascii_uppercase(),
                1 => character.to_ascii_lowercase(),
                _ => character,
            })
            .collect();
        if piece.len() > 2 && random.below(2) == 0 {
            piece.remove(1);
        }
        queries.push(piece.into_iter().collect());
    }

    let mut matched = 0;
    let mut qualified_matched = 0;
    for (query_number, query) in queries.iter().enumerate() {
        for mode in [SearchMode::Prefix, SearchMode::Substring, SearchMode::Fuzzy] {
            let ranked = rank_every_name(&names, query, mode);
            // Merging by place looks up every ranked name, which takes too long in a debug build
            // for qualified queries that match most qualified names.
            if query.contains("::") && ranked.len() > 1_000 {
                continue;
            }
            let mut limits = vec![0, 1, 7, 64];
            if query_number % 4 == 0 && ranked.len() <= 3_000 {
                limits.push(1_000_000);
            }
            for limit in limits {
                let found: Vec<(Definition, u32)> = index
                    .search(query, mode, limit)
                    .into_iter()
                    .map(|found| (found.definition, found.score))
                    .collect();
                let expected = expand_ranked(&index, query, &ranked, limit);
                assert_eq!(
                    found.len(),
                    expected.len(),
                    "{mode:?} {query:?} limit {limit}"
                );
                for (position, (found, expected)) in found.iter().zip(&expected).enumerate() {
                    assert_eq!(
                        found, expected,
                        "{mode:?} {query:?} limit {limit}, match {position}"
                    );
                }
            }
            if !ranked.is_empty() {
                matched += 1;
                if query.contains("::") {
                    qualified_matched += 1;
                }
            }
        }
    }
    assert!(matched > 150, "only {matched} queries matched anything");
    assert!(
        qualified_matched > 30,
        "only {qualified_matched} qualified queries matched anything"
    );
}

/// A file source whose reads wait until released, holding the indexing workers busy.
#[derive(Default)]
struct BlockingSource {
    files: MemorySource,
    entered: AtomicUsize,
    release: AtomicBool,
}

impl FileSource for BlockingSource {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.files.read(path)
    }
}

#[test]
fn searches_do_not_wait_for_indexing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = Arc::new(SymbolIndex::open(directory.path(), config()).expect("open"));
    let source = MemorySource::default();
    source.set("a.c", "int searchable_name(void) { return 0; }\n");
    full_update(&index, &source, 1_000);

    let blocking = Arc::new(BlockingSource::default());
    for number in 0..8 {
        blocking.files.set(
            &format!("b{number}.c"),
            &format!("int blocked_{number}(void);\n"),
        );
    }
    let updater = std::thread::spawn({
        let index = index.clone();
        let blocking = blocking.clone();
        move || {
            index.update_files(
                &blocking.files.entries(1_000),
                &[],
                &*blocking,
                &AtomicBool::new(false),
            )
        }
    });
    let waiting_since = Instant::now();
    while blocking.entered.load(Ordering::SeqCst) < config().parallelism.unwrap_or(1) {
        assert!(
            waiting_since.elapsed() < Duration::from_secs(60),
            "the indexing workers never started"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    // Every indexing worker is now busy with a file.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let found = [SearchMode::Substring, SearchMode::Fuzzy]
            .map(|mode| index.search("searchable", mode, 10).len());
        sender.send(found).ok();
    });
    let found = receiver.recv_timeout(Duration::from_secs(10));
    blocking.release.store(true, Ordering::SeqCst);
    let stats = updater
        .join()
        .expect("the update panicked")
        .expect("update_files");
    assert_eq!(stats.files_parsed, 8);
    assert_eq!(
        found.expect("a search waited for the indexing workers"),
        [1, 1]
    );
}

/// A branch switch changes many files in quick succession. The manifest is not rewritten for
/// each update, yet a copy of the directory taken at any moment (as a crash leaves it) opens
/// without being discarded and catches up with one full update.
#[test]
fn manifest_writes_are_batched_and_every_moment_is_recoverable() {
    let started = Instant::now();
    let directory = tempfile::tempdir().expect("tempdir");
    let small_segments = IndexConfig {
        segment_file_limit: 4,
        ..config()
    };
    let index = SymbolIndex::open(directory.path(), small_segments.clone()).expect("open");
    let source = MemorySource::default();
    let contents = |number: usize, version: usize| {
        format!(
            "int stable_{number}(void) {{ return changing_{version}(); }}\n\
             int version_{number}_{version}(void) {{ return stable_{number}(); }}\n"
        )
    };
    for number in 0..12 {
        source.set(&format!("f{number:02}.c"), &contents(number, 0));
    }
    // Three segments, written once at the end of the full update.
    full_update(&index, &source, 1_000);
    assert_eq!(index.writer.lock().manifest_writes, 1);

    let mut compactions = 0;
    for step in 1..=20usize {
        let changed: Vec<FileEntry> = (0..3)
            .map(|offset| {
                let number = (step * 7 + offset * 5) % 12;
                let path = format!("f{number:02}.c");
                let text = contents(number, step);
                source.set(&path, &text);
                entry(&path, text.len(), 2_000 + step as u64)
            })
            .collect();
        let stats = index
            .update_files(&changed, &[], &source, &AtomicBool::new(false))
            .expect("update_files");
        compactions += stats.segments_compacted;

        let copy = tempfile::tempdir().expect("tempdir");
        copy_directory(directory.path(), copy.path());
        let recovered = SymbolIndex::open(copy.path(), small_segments.clone()).expect("open copy");
        assert_eq!(recovered.reset_reason(), None, "step {step}");
        full_update(&recovered, &source, 9_000);
        compare_with_rebuild(&recovered, &source, &small_segments, step as u64);
    }
    assert!(compactions > 0);
    let allowed = 1 + started.elapsed().as_secs() / MANIFEST_WRITE_INTERVAL.as_secs();
    let writes = index.writer.lock().manifest_writes;
    assert!(
        writes <= 1 + allowed as usize,
        "{writes} manifest writes for 20 updates"
    );

    // Closing the index writes what is still unwritten.
    let before = dump(&index);
    drop(index);
    let index = SymbolIndex::open(directory.path(), small_segments).expect("reopen");
    assert_eq!(index.reset_reason(), None);
    assert_eq!(dump(&index), before);
}

/// An update that comes soon after the last write leaves the manifest unwritten; `save` writes
/// it, once.
#[test]
fn save_writes_what_updates_left_unwritten() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = sample_source();
    full_update(&index, &source, 1_000);
    change_file(
        &index,
        &source,
        "lib/other.c",
        "int saved_later(void) { return 0; }
",
        2_000,
    );
    assert_eq!(index.writer.lock().manifest_writes, 1);
    let on_disk = |name: &str| {
        let copy = tempfile::tempdir().expect("tempdir");
        copy_directory(directory.path(), copy.path());
        let copied = SymbolIndex::open(copy.path(), config()).expect("open copy");
        assert_eq!(copied.reset_reason(), None);
        copied.definitions(name).len()
    };
    assert_eq!(on_disk("saved_later"), 0);

    index.save().expect("save");
    assert_eq!(index.writer.lock().manifest_writes, 2);
    assert_eq!(on_disk("saved_later"), 1);
    index.save().expect("save");
    assert_eq!(
        index.writer.lock().manifest_writes,
        2,
        "nothing was left to write"
    );
}

/// A crash before the manifest is written leaves segments that the manifest does not name. They
/// are removed when the index is opened again, but their ids are not given to new segments: a
/// definition id naming one must stay stale.
#[test]
fn segments_lost_in_a_crash_do_not_lend_their_ids() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = sample_source();
    full_update(&index, &source, 1_000);
    let saved = segment_ids(directory.path());
    change_file(
        &index,
        &source,
        "lib/other.c",
        "int lost_in_the_crash(void) { return 0; }\n",
        2_000,
    );
    let lost = index.definitions("lost_in_the_crash");
    assert_eq!(lost.len(), 1);
    let crashed = tempfile::tempdir().expect("tempdir");
    copy_directory(directory.path(), crashed.path());
    assert!(
        segment_ids(crashed.path()).contains(&lost[0].id.segment),
        "the update was not written as a segment"
    );
    assert!(!saved.contains(&lost[0].id.segment));

    let recovered = SymbolIndex::open(crashed.path(), config()).expect("open");
    assert_eq!(recovered.reset_reason(), None);
    assert_eq!(segment_ids(crashed.path()), saved);
    assert_eq!(recovered.definition(lost[0].id), None);
    change_file(
        &recovered,
        &source,
        "lib/other.c",
        "int written_after_the_crash(void) { return 0; }\n",
        3_000,
    );
    assert_eq!(recovered.definition(lost[0].id), None);
    let rewritten = recovered.definitions("written_after_the_crash");
    assert_eq!(rewritten.len(), 1);
    assert!(rewritten[0].id.segment > lost[0].id.segment);
}

#[cfg(unix)]
#[test]
fn index_files_are_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt as _;
    let parent = tempfile::tempdir().expect("tempdir");
    let directory = parent.path().join("index");
    let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
    {
        let index = SymbolIndex::open(&directory, config()).expect("open");
        full_update(&index, &sample_source(), 1_000);
    }
    assert_eq!(mode(&directory), 0o700);
    let mut files = 0;
    for entry in fs::read_dir(&directory).expect("list") {
        let path = entry.expect("entry").path();
        assert_eq!(mode(&path) & 0o077, 0, "{}", path.display());
        files += 1;
    }
    assert!(files >= 4, "{files} files");

    // A directory left readable by others (an older version, a loose umask) is tightened.
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).expect("chmod");
    let _index = SymbolIndex::open(&directory, config()).expect("reopen");
    assert_eq!(mode(&directory), 0o700);
}

#[test]
fn skipped_counts_follow_the_latest_full_update() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = MemorySource::default();
    source.set("a.c", "int a(void) { return 0; }\n");
    source.set("notes.txt", "text\n");
    full_update(&index, &source, 1_000);
    assert_eq!(index.stats().skipped_unsupported, 1);
    // Nothing to parse this time, but the counts still change.
    source.remove("notes.txt");
    let stats = full_update(&index, &source, 1_000);
    assert_eq!(stats.files_parsed, 0);
    assert_eq!(index.stats().skipped_unsupported, 0);
    drop(index);
    let index = SymbolIndex::open(directory.path(), config()).expect("reopen");
    assert_eq!(index.stats().skipped_unsupported, 0);
}

// Helpers for damaging an index while keeping its checksums valid, so that the structural
// checks rather than the checksums are what stands between the damage and a panic.

fn refresh_segment_checksum(bytes: &mut [u8]) {
    let sum = storage::checksum(&bytes[storage::HEADER_SIZE..]);
    bytes[32..40].copy_from_slice(&sum.to_le_bytes());
}

fn refresh_manifest_checksum(bytes: &mut [u8]) {
    let body_length = bytes.len() - 8;
    let sum = storage::checksum(&bytes[..body_length]);
    bytes[body_length..].copy_from_slice(&sum.to_le_bytes());
}

/// `(offset, count)` of a section, read from a segment's header.
fn section_bounds(bytes: &[u8], section: usize) -> (usize, usize) {
    let entry = 40 + section * 16;
    let read = |offset: usize| {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("8 bytes")) as usize
    };
    (read(entry), read(entry + 8))
}

const FILES_SECTION: usize = 1;
const DEFINITIONS_SECTION: usize = 2;
const NAMES_SECTION: usize = 5;

fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn segment_ids(directory: &Path) -> FxHashSet<u64> {
    segment_paths(directory)
        .iter()
        .filter_map(|path| path.file_name()?.to_str().and_then(parse_segment_file_name))
        .collect()
}

/// Runs every kind of query against the index and returns a count of the answers.
fn exercise_all_queries(index: &SymbolIndex) -> usize {
    let mut answers = 0;
    let names = [
        "helper",
        "foo_open",
        "shared_util",
        "dispatch",
        "open",
        "register_ops",
        "file_operations",
        "MAKE",
        "",
    ];
    for name in names {
        for definition in index.definitions(name) {
            answers += 1;
            answers += index
                .callees(definition.id)
                .map_or(0, |callees| callees.len());
            answers += usize::from(index.definition(definition.id).is_some());
        }
        answers += index.callers(name, 50).calls.len();
        answers += index.assignments(name).len();
        answers += index.assignments_of_function(name).len();
        for mode in [SearchMode::Prefix, SearchMode::Substring, SearchMode::Fuzzy] {
            answers += index.search(name, mode, 20).len();
        }
    }
    for path in index.files() {
        if let Some(symbols) = index.file_symbols(&path) {
            answers += symbols.definitions.len() + symbols.calls.len();
            for definition in &symbols.definitions {
                answers += index
                    .callees(definition.id)
                    .map_or(0, |callees| callees.len());
            }
        }
        for byte in [0, 10, 100, 1000] {
            answers += usize::from(index.enclosing_definition(&path, byte).is_some());
        }
    }
    answers + index.stats().files
}

/// Runs `work` on another thread and fails if it panics or does not finish in time, so that a
/// damaged count that makes a query walk billions of records fails the test instead of
/// hanging it.
fn within_time_limit<T: Send + 'static>(
    what: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = work();
        sender.send(result).ok();
    });
    receiver
        .recv_timeout(Duration::from_secs(60))
        .unwrap_or_else(|error| panic!("{what}: did not finish ({error})"))
}

/// An index with two segments, a superseded file and a recorded timestamp, so that
/// tombstones and overrides are part of what gets damaged.
fn build_layered_index(directory: &Path) {
    let index = SymbolIndex::open(directory, config()).expect("open");
    let source = sample_source();
    full_update(&index, &source, 1_000);
    change_file(
        &index,
        &source,
        "lib/other.c",
        &format!("{OTHER}int extra(void) {{ return helper(); }}\n"),
        1_100,
    );
    full_update(&index, &source, 1_200);
    let manifest = &index.writer.lock().manifest;
    assert_eq!(manifest.segments.len(), 2);
    assert!(!manifest.overrides.is_empty());
}

fn copy_directory(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).expect("list") {
        let entry = entry.expect("entry");
        if entry.file_name() == LOCK_FILE_NAME {
            continue;
        }
        fs::copy(entry.path(), to.join(entry.file_name())).expect("copy");
    }
}

#[test]
fn damaged_counts_with_valid_checksums_do_not_stall_queries() {
    let pristine = tempfile::tempdir().expect("tempdir");
    build_layered_index(pristine.path());
    type Damage = Box<dyn Fn(&mut Vec<u8>)>;
    let damages: Vec<(&str, Damage)> = vec![
        (
            "definition call runs",
            Box::new(|bytes: &mut Vec<u8>| {
                let (offset, count) = section_bounds(bytes, DEFINITIONS_SECTION);
                for record in 0..count {
                    let start = offset + record * storage::DEFINITION_RECORD_SIZE;
                    set_u32(bytes, start + 60, 0);
                    set_u32(bytes, start + 64, u32::MAX);
                }
            }),
        ),
        (
            "name posting runs",
            Box::new(|bytes: &mut Vec<u8>| {
                let (offset, count) = section_bounds(bytes, NAMES_SECTION);
                for record in 0..count {
                    let start = offset + record * storage::NAME_RECORD_SIZE;
                    for field in [8, 16, 24, 32] {
                        set_u32(bytes, start + field, 1);
                        set_u32(bytes, start + field + 4, u32::MAX);
                    }
                }
            }),
        ),
        (
            "file record runs",
            Box::new(|bytes: &mut Vec<u8>| {
                let (offset, count) = section_bounds(bytes, FILES_SECTION);
                for record in 0..count {
                    let start = offset + record * storage::FILE_RECORD_SIZE;
                    for field in [48, 56, 64] {
                        set_u32(bytes, start + field + 4, u32::MAX);
                    }
                }
            }),
        ),
    ];
    for (name, damage) in damages {
        let directory = tempfile::tempdir().expect("tempdir");
        copy_directory(pristine.path(), directory.path());
        for path in segment_paths(directory.path()) {
            corrupt(&path, |bytes| {
                damage(bytes);
                refresh_segment_checksum(bytes);
            });
        }
        let path = directory.path().to_path_buf();
        within_time_limit(name, move || {
            let index = SymbolIndex::open(&path, config()).expect("open");
            assert_eq!(index.reset_reason(), None, "the damage is structural only");
            exercise_all_queries(&index)
        });
    }
}

#[test]
fn randomly_damaged_indexes_never_panic() {
    let pristine = tempfile::tempdir().expect("tempdir");
    build_layered_index(pristine.path());
    let mut random = Random(0x2545_f491_4f6c_dd1d);
    let interesting = [0, 1, 2, 63, 64, 0x7fff_ffff, u32::MAX - 1, u32::MAX];
    let mut accepted = 0;
    for case in 0..160 {
        let (first, second, third, kind) =
            (random.next(), random.next(), random.next(), random.below(5));
        let directory = tempfile::tempdir().expect("tempdir");
        copy_directory(pristine.path(), directory.path());
        let segments = segment_paths(directory.path());
        let manifest = directory.path().join(MANIFEST_FILE_NAME);
        let segment = &segments[first as usize % segments.len()];
        match kind {
            // A flipped bit anywhere in a segment, including its header.
            0 => corrupt(segment, |bytes| {
                let position = second as usize % bytes.len();
                bytes[position] ^= 1 << (third % 8);
                if !(32..40).contains(&position) {
                    refresh_segment_checksum(bytes);
                }
            }),
            // An aligned field set to a value likely to be an edge case.
            1 => corrupt(segment, |bytes| {
                let position = (second as usize % (bytes.len() / 4)) * 4;
                if !(32..40).contains(&position) {
                    set_u32(
                        bytes,
                        position,
                        interesting[third as usize % interesting.len()],
                    );
                    refresh_segment_checksum(bytes);
                }
            }),
            // A shortened segment whose header agrees with its new length.
            2 => corrupt(segment, |bytes| {
                let length =
                    storage::HEADER_SIZE + second as usize % (bytes.len() - storage::HEADER_SIZE);
                bytes.truncate(length);
                bytes[16..24].copy_from_slice(&(length as u64).to_le_bytes());
                refresh_segment_checksum(bytes);
            }),
            // A manifest field set to an edge value, with the checksum recomputed.
            3 => corrupt(&manifest, |bytes| {
                let position = 16 + (second as usize % ((bytes.len() - 24) / 4)) * 4;
                set_u32(
                    bytes,
                    position,
                    interesting[third as usize % interesting.len()],
                );
                refresh_manifest_checksum(bytes);
            }),
            // A shortened or extended manifest with a valid checksum.
            _ => corrupt(&manifest, |bytes| {
                let body_length = bytes.len() - 8;
                let mut body = bytes[..body_length].to_vec();
                if first % 2 == 0 {
                    body.truncate(second as usize % body_length);
                } else {
                    body.extend_from_slice(&third.to_le_bytes());
                }
                let sum = storage::checksum(&body);
                body.extend_from_slice(&sum.to_le_bytes());
                *bytes = body;
            }),
        }
        let path = directory.path().to_path_buf();
        let reset = within_time_limit(&format!("case {case} (kind {kind})"), move || {
            let index = SymbolIndex::open(&path, config()).expect("open");
            exercise_all_queries(&index);
            // Updating on top of the damage (compaction reads the damaged records) must not
            // panic either; an error is acceptable.
            let source = sample_source();
            index
                .update(
                    &source.entries(3_000),
                    &source,
                    &|_| {},
                    &AtomicBool::new(false),
                )
                .ok();
            exercise_all_queries(&index);
            index.reset_reason().is_some()
        });
        accepted += usize::from(!reset);
    }
    // Most damage is structural and gets past the checks at open; make sure the queries
    // really ran against damaged data rather than an emptied index.
    assert!(
        accepted > 40,
        "only {accepted} damaged indexes were opened as is"
    );
}

#[test]
fn a_manifest_listing_a_segment_twice_is_rebuilt() {
    let directory = tempfile::tempdir().expect("tempdir");
    build_layered_index(directory.path());
    let path = directory.path().join(MANIFEST_FILE_NAME);
    let mut manifest = storage::Manifest::decode(&fs::read(&path).expect("read")).expect("decode");
    let first = manifest.segments[0].clone();
    manifest.segments.push(first);
    fs::write(&path, manifest.encode()).expect("write");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    assert!(index.reset_reason().is_some());
    assert_eq!(index.stats().files, 0);
}

#[test]
fn a_manifest_with_an_impossible_tombstone_count_is_rejected() {
    let manifest = storage::Manifest {
        next_segment_id: 1,
        segments: vec![storage::ManifestSegment {
            id: 0,
            file_count: 64,
            dead: vec![0],
        }],
        ..storage::Manifest::default()
    };
    let mut bytes = manifest.encode();
    // The file count of the only segment, then its tombstone word count, as if the segment
    // held four billion files.
    let segment_entry = 8 + 4 + 4 + 8 * 5;
    set_u32(&mut bytes, segment_entry + 8, u32::MAX);
    set_u32(&mut bytes, segment_entry + 12, u32::MAX.div_ceil(64));
    refresh_manifest_checksum(&mut bytes);
    let error = storage::Manifest::decode(&bytes).expect_err("impossible manifest");
    assert!(format!("{error:#}").contains("truncated"), "{error:#}");
}

#[test]
fn an_exhausted_segment_id_counter_is_rebuilt() {
    let directory = tempfile::tempdir().expect("tempdir");
    build_layered_index(directory.path());
    let path = directory.path().join(MANIFEST_FILE_NAME);
    corrupt(&path, |bytes| {
        bytes[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        refresh_manifest_checksum(bytes);
    });
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    assert!(index.reset_reason().is_some());
    let source = sample_source();
    let stats = full_update(&index, &source, 1_000);
    assert_eq!(stats.files_parsed, 3);
}

#[test]
fn definition_ids_from_before_a_reset_stay_stale() {
    let directory = tempfile::tempdir().expect("tempdir");
    let source = sample_source();
    let old_id = {
        let index = SymbolIndex::open(directory.path(), config()).expect("open");
        full_update(&index, &source, 1_000);
        index.definitions("dispatch").remove(0).id
    };
    fs::write(directory.path().join(MANIFEST_FILE_NAME), b"damaged").expect("write");
    let index = SymbolIndex::open(directory.path(), config()).expect("reopen");
    assert!(index.reset_reason().is_some());
    full_update(&index, &source, 1_000);
    assert_eq!(index.definitions("dispatch").len(), 1);
    assert_eq!(index.definition(old_id), None);
    assert_eq!(index.callees(old_id), None);
}

#[test]
fn paths_that_leave_the_root_are_not_indexed_or_read() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = MemorySource::default();
    let mut bad_paths = vec![
        "../escape.c",
        "a/../../escape.c",
        "/absolute.c",
        "a//b.c",
        "./a.c",
        "a/./b.c",
        "nul\0.c",
    ];
    if cfg!(windows) {
        bad_paths.extend(["a\\..\\..\\escape.c", "C:/escape.c", "C:escape.c"]);
    }
    for path in &bad_paths {
        source.set(path, "int escaped(void) { return 0; }\n");
    }
    source.set("good.c", "int kept(void) { return 0; }\n");
    let stats = full_update(&index, &source, 1_000);
    assert_eq!(stats.files_parsed, 1, "{stats:?}");
    assert_eq!(stats.files_skipped_unsupported, bad_paths.len());
    assert_eq!(index.definitions("escaped"), vec![]);
    let changed: Vec<FileEntry> = bad_paths
        .iter()
        .map(|path| entry(path, 10, 1_000))
        .collect();
    let stats = index
        .update_files(&changed, &[], &source, &AtomicBool::new(false))
        .expect("update_files");
    assert_eq!(stats.files_parsed, 0);
    assert_eq!(index.definitions("escaped"), vec![]);
    assert_eq!(index.definitions("kept").len(), 1);

    let root = tempfile::tempdir().expect("tempdir");
    fs::create_dir(root.path().join("inner")).expect("mkdir");
    fs::write(root.path().join("outside.c"), "int x;").expect("write");
    fs::write(root.path().join("inner").join("inside.c"), "int y;").expect("write");
    let directory_source = DirectorySource::new(root.path().join("inner"));
    let error = directory_source
        .read("../outside.c")
        .expect_err("reading outside the root");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        directory_source.read("inside.c").expect("inside"),
        b"int y;"
    );
}

#[test]
fn a_damaged_stored_path_is_not_returned() {
    let directory = tempfile::tempdir().expect("tempdir");
    {
        let index = SymbolIndex::open(directory.path(), config()).expect("open");
        full_update(&index, &sample_source(), 1_000);
    }
    // Same length as `lib/other.c`, so only the text changes.
    for path in segment_paths(directory.path()) {
        corrupt(&path, |bytes| {
            let position = bytes
                .windows(b"lib/other.c".len())
                .position(|window| window == b"lib/other.c")
                .expect("path stored");
            bytes[position..position + 11].copy_from_slice(b"../other.cc");
            refresh_segment_checksum(bytes);
        });
    }
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    assert_eq!(index.reset_reason(), None);
    let mut all_paths: Vec<String> = index.files().iter().map(|path| path.to_string()).collect();
    for name in ["helper", "dispatch", "shared_util"] {
        all_paths.extend(
            index
                .definitions(name)
                .into_iter()
                .map(|definition| definition.path.to_string()),
        );
        all_paths.extend(
            index
                .callers(name, 10)
                .calls
                .into_iter()
                .map(|call| call.path.to_string()),
        );
    }
    assert!(
        all_paths.iter().all(|path| !path.contains("..")),
        "{all_paths:?}"
    );
    assert!(all_paths.iter().any(|path| path == "drivers/foo.c"));
}

#[test]
fn a_cancelled_compaction_does_not_reuse_segment_ids() {
    let directory = tempfile::tempdir().expect("tempdir");
    let small_segments = IndexConfig {
        segment_file_limit: 3,
        parallelism: Some(1),
        ..IndexConfig::default()
    };
    let index = SymbolIndex::open(directory.path(), small_segments.clone()).expect("open");
    let source = MemorySource::default();
    for number in 0..12 {
        source.set(
            &format!("file{number:02}.c"),
            &format!("int function{number}(void) {{ return callee{number}(); }}\n"),
        );
    }
    full_update(&index, &source, 1_000);
    assert_eq!(index.stats().segments, 4);
    // Leaving one file in each segment makes all four mostly superseded, and their four
    // remaining files take two compaction chunks.
    for number in 0..12 {
        if number % 3 != 0 {
            source.remove(&format!("file{number:02}.c"));
        }
    }
    // The first chunk of the compaction below takes this id; the cancellation, noticed before
    // the second chunk, abandons it.
    let abandoned = index.writer.lock().manifest.next_segment_id;
    let cancel = AtomicBool::new(false);
    let stats = index
        .update(
            &source.entries(1_000),
            &source,
            &|progress| {
                if progress.phase == ProgressPhase::Compacting {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        )
        .expect("update");
    assert_eq!(stats.files_removed, 8);
    assert_eq!(stats.segments_compacted, 0);
    assert!(!segment_ids(directory.path()).contains(&abandoned));
    assert!(index.writer.lock().manifest.next_segment_id > abandoned);

    // Even if the abandoned segment could not be deleted, a later segment must not take its
    // place and then be deleted as if it were the abandoned one.
    index.writer.lock().pending_deletions.push(abandoned);
    fs::write(
        Segment::path(directory.path(), abandoned),
        b"undeletable leftover",
    )
    .expect("write");
    change_file(
        &index,
        &source,
        "file00.c",
        "int function0(void) { return renamed(); }\n",
        2_000,
    );
    drop(index);
    let index = SymbolIndex::open(directory.path(), small_segments).expect("reopen");
    assert_eq!(index.reset_reason(), None);
    assert_eq!(index.stats().files, 4);
    assert_eq!(caller_names(&index, "renamed").len(), 1);
}

#[test]
fn live_segments_are_never_deleted() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    full_update(&index, &sample_source(), 1_000);
    let mut writer = index.writer.lock();
    let live = writer.manifest.segments[0].id;
    writer.pending_deletions.push(live);
    index.delete_pending(&mut writer);
    assert!(writer.pending_deletions.is_empty());
    drop(writer);
    assert!(Segment::path(directory.path(), live).exists());
    assert_eq!(index.definitions("helper").len(), 2);
}

#[test]
fn queries_run_consistently_while_updates_and_compactions_replace_segments() {
    let directory = tempfile::tempdir().expect("tempdir");
    let small_segments = IndexConfig {
        segment_file_limit: 4,
        parallelism: Some(2),
        ..IndexConfig::default()
    };
    let index = Arc::new(SymbolIndex::open(directory.path(), small_segments).expect("open"));
    let source = MemorySource::default();
    let file_count = 12;
    let contents = |number: usize, version: usize| {
        format!(
            "int stable_{number}(void) {{ return changing_{version}(); }}\n\
             int version_{number}_{version}(void) {{ return stable_{number}(); }}\n"
        )
    };
    for number in 0..file_count {
        source.set(&format!("f{number:02}.c"), &contents(number, 0));
    }
    full_update(&index, &source, 1_000);

    let done = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|reader| {
            let index = index.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let mut rounds = 0;
                while !done.load(Ordering::Relaxed) || rounds == 0 {
                    for number in 0..file_count {
                        let name = format!("stable_{number}");
                        // Each commit swaps in a complete snapshot, so a file is never seen
                        // twice or not at all.
                        let definitions = index.definitions(&name);
                        assert_eq!(definitions.len(), 1, "reader {reader}: {name}");
                        if let Some(callees) = index.callees(definitions[0].id) {
                            assert_eq!(callees.len(), 1, "reader {reader}: {name}");
                        }
                        assert_eq!(index.callers(&name, 10).calls.len(), 1, "{name}");
                        index.search(&name, SearchMode::Fuzzy, 5);
                    }
                    rounds += 1;
                }
                rounds
            })
        })
        .collect();

    let mut random = Random(99);
    let mut compactions = 0;
    for step in 1..=80 {
        let number = random.below(file_count as u64) as usize;
        let stats = change_file(
            &index,
            &source,
            &format!("f{number:02}.c"),
            &contents(number, step),
            2_000 + step as u64,
        );
        compactions += stats.segments_compacted;
    }
    done.store(true, Ordering::Relaxed);
    for reader in readers {
        assert!(reader.join().expect("reader panicked") > 0);
    }
    assert!(compactions > 0);

    // With no reader holding an old snapshot, the next update deletes every superseded
    // segment, including any that could not be deleted while it was mapped.
    full_update(&index, &source, 5_000);
    let referenced: FxHashSet<u64> = index
        .writer
        .lock()
        .manifest
        .segments
        .iter()
        .map(|segment| segment.id)
        .collect();
    assert_eq!(segment_ids(directory.path()), referenced);
}

#[test]
fn names_differing_only_in_case_stay_apart() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = MemorySource::default();
    // Enough names that the binary search probes the case variants in both directions.
    let mut text = String::new();
    for prefix in ["a", "m", "z"] {
        for name in ["value", "Value", "VALUE", "vAlUe"] {
            text.push_str(&format!(
                "int {prefix}{name}(void) {{ return {prefix}{name}_callee(); }}\n"
            ));
        }
    }
    source.set("case.c", &text);
    full_update(&index, &source, 1_000);
    for prefix in ["a", "m", "z"] {
        for name in ["value", "Value", "VALUE", "vAlUe"] {
            let name = format!("{prefix}{name}");
            let definitions = index.definitions(&name);
            assert_eq!(definitions.len(), 1, "{name}");
            assert_eq!(definitions[0].name, name);
            let callers = index.callers(&format!("{name}_callee"), 10);
            assert_eq!(callers.calls.len(), 1, "{name}");
            assert_eq!(
                callers.calls[0]
                    .caller
                    .as_ref()
                    .map(|caller| caller.name.as_str()),
                Some(name.as_str())
            );
        }
    }
    assert_eq!(index.search("MVALUE", SearchMode::Prefix, 10).len(), 4);
}

#[test]
fn checksums_cover_every_byte() {
    for length in 1..=24usize {
        let bytes: Vec<u8> = (0..length).map(|index| index as u8).collect();
        let original = storage::checksum(&bytes);
        for position in 0..length {
            let mut damaged = bytes.clone();
            damaged[position] ^= 0x10;
            assert_ne!(
                storage::checksum(&damaged),
                original,
                "length {length}, byte {position}"
            );
        }
        assert_ne!(storage::checksum(&bytes[..length - 1]), original);
    }
}

#[test]
fn enclosing_definition_is_the_innermost() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = MemorySource::default();
    let text = "void outer() {\n\
                \tbefore();\n\
                \tstruct Local { void inner() { nested(); } };\n\
                \tafter();\n\
                }\n";
    source.set("local.cpp", text);
    full_update(&index, &source, 1_000);
    let at = |needle: &str| {
        index
            .enclosing_definition("local.cpp", text.find(needle).expect("in source") as u32)
            .map(|definition| definition.name)
    };
    assert_eq!(at("nested").as_deref(), Some("inner"));
    assert_eq!(at("before").as_deref(), Some("outer"));
    assert_eq!(at("after").as_deref(), Some("outer"));
}

#[test]
fn a_path_listed_twice_is_indexed_once() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = sample_source();
    let mut entries = source.entries(1_000);
    entries.extend(source.entries(1_000));
    let stats = index
        .update(&entries, &source, &|_| {}, &AtomicBool::new(false))
        .expect("update");
    assert_eq!(stats.files_parsed, 3);
    assert_eq!(index.definitions("dispatch").len(), 1);

    let changed = OTHER.replace("dispatch", "dispatch_renamed");
    source.set("lib/other.c", &changed);
    let twice = [
        entry("lib/other.c", changed.len(), 2_000),
        entry("lib/other.c", changed.len(), 2_000),
    ];
    let stats = index
        .update_files(&twice, &[], &source, &AtomicBool::new(false))
        .expect("update_files");
    assert_eq!(stats.files_parsed, 1);
    assert_eq!(index.definitions("dispatch_renamed").len(), 1);
    assert_eq!(index.definitions("helper").len(), 2);
    assert_eq!(index.stats().files, 3);
}

#[test]
fn a_changed_file_that_vanished_is_removed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = sample_source();
    full_update(&index, &source, 1_000);
    // The watcher reported a change, but the file was deleted before it was read.
    source.remove("lib/other.c");
    let stats = index
        .update_files(
            &[entry("lib/other.c", OTHER.len() + 1, 2_000)],
            &[],
            &source,
            &AtomicBool::new(false),
        )
        .expect("update_files");
    assert_eq!(stats.files_removed, 1);
    assert_eq!(index.definitions("dispatch"), vec![]);
    assert!(index.file_symbols("lib/other.c").is_none());
    assert_eq!(index.stats().files, 2);
}

#[test]
fn cpp_functions_are_found_by_qualified_and_plain_names() {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(directory.path(), config()).expect("open");
    let source = MemorySource::default();
    source.set(
        "src/widget.hpp",
        "namespace ui {\n\
         class Widget {\n\
         public:\n\
         \tstatic Widget make();\n\
         \tvoid draw();\n\
         };\n\
         }\n",
    );
    source.set(
        "src/widget.cpp",
        "#include \"widget.hpp\"\n\
         namespace ui {\n\
         void Widget::draw() { this->paint(); detail::helper(); }\n\
         Widget Widget::make() { return Widget(); }\n\
         namespace detail { int helper() { return 0; } }\n\
         }\n",
    );
    source.set(
        "src/main.cpp",
        "int main() { ui::Widget::make().draw(); return ui::detail::helper(); }\n",
    );
    full_update(&index, &source, 1_000);

    let places = |name: &str| definition_places(&index, name);
    let draw = vec![
        (
            "src/widget.cpp".to_owned(),
            DefinitionKind::Method,
            false,
            2,
        ),
        ("src/widget.hpp".to_owned(), DefinitionKind::Method, true, 4),
    ];
    assert_eq!(places("draw"), draw);
    assert_eq!(places("Widget::draw"), draw);
    assert_eq!(places("ui::Widget::draw"), draw);
    assert_eq!(
        places("detail::helper"),
        vec![(
            "src/widget.cpp".to_owned(),
            DefinitionKind::Function,
            false,
            4
        )]
    );
    assert_eq!(places("ui::detail::helper"), places("helper"));
    assert!(places("Widget::helper").is_empty());

    // A static member function is callable from every file.
    assert!(
        index
            .definitions("make")
            .iter()
            .all(|definition| !definition.is_static)
    );

    // Calls are recorded under the called name alone and belong to the function's own
    // definition, which also owns the calls in its body.
    assert_eq!(
        caller_names(&index, "helper"),
        vec![
            ("src/main.cpp".to_owned(), Some("main".to_owned())),
            ("src/widget.cpp".to_owned(), Some("draw".to_owned())),
        ]
    );
    let widget = source.read("src/widget.cpp").expect("widget.cpp");
    let paint = widget
        .windows(5)
        .position(|window| window == b"paint")
        .expect("paint call");
    let body = index
        .enclosing_definition("src/widget.cpp", paint as u32)
        .expect("inside draw");
    assert_eq!(body.name, "draw");
    let callees = index
        .callees(body.id)
        .expect("live definition")
        .into_iter()
        .map(|callee| (callee.callee, callee.kind))
        .collect::<Vec<_>>();
    assert_eq!(
        callees,
        vec![
            ("paint".to_owned(), CallKind::Member),
            ("helper".to_owned(), CallKind::Direct),
        ]
    );

    // A plain query lists each function once; a qualified one finds the qualified names.
    for mode in [SearchMode::Prefix, SearchMode::Substring, SearchMode::Fuzzy] {
        let names = index
            .search("draw", mode, 20)
            .into_iter()
            .map(|found| found.definition.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["draw", "draw"], "{mode:?}");
    }
    let names = index
        .search("Widget::", SearchMode::Prefix, 20)
        .into_iter()
        .map(|found| found.definition.name)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        names,
        ["Widget::draw", "Widget::make"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
}
