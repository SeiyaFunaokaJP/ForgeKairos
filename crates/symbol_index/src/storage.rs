//! On-disk format.
//!
//! An index directory holds immutable segment files (`seg-<id>.bin`) and one small
//! `manifest.bin` naming the live segments, which files in each are superseded (a bitset) and
//! metadata-only updates. Segments are written to a temporary file and renamed into place
//! before the manifest that references them, so a crash leaves the previous manifest (and
//! therefore the previous index) intact. Segments are memory-mapped and read record by record;
//! every read is bounds-checked, so a damaged file yields missing results rather than a panic.

use std::{
    cmp::Ordering,
    fs::{self, File},
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    ops,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow, bail};
use memmap2::Mmap;
use rustc_hash::FxHashMap;

use crate::extract::{
    AssignmentKind, CallKind, DefinitionKind, ExtractedAssignment, ExtractedCall,
    ExtractedDefinition, ExtractedFile, Position, Range, SourceLanguage,
};

pub(crate) const SEGMENT_MAGIC: &[u8; 8] = b"ZSYMSEG\0";
pub(crate) const MANIFEST_MAGIC: &[u8; 8] = b"ZSYMMAN\0";
/// Bump whenever the layout of segments or the manifest, or the meaning of extracted data,
/// changes. Indexes with another version are discarded and rebuilt.
/// 2: lone member macros in struct bodies are blanked before parsing (`extract.rs`).
/// 3: C++ definitions are also recorded under their qualified names, `static` follows
/// anonymous namespaces and class members, and more is rewritten before parsing (member macros
/// ending in a call, `\` with trailing spaces and `/*` inside literals in `#define`s)
/// (`extract.rs`).
pub(crate) const FORMAT_VERSION: u32 = 3;

const NONE: u32 = u32::MAX;

pub(crate) const SECTION_COUNT: usize = 10;
pub(crate) const HEADER_SIZE: usize = 8 + 4 + 4 + 8 + 8 + 8 + SECTION_COUNT * 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum Section {
    Strings = 0,
    Files = 1,
    Definitions = 2,
    Calls = 3,
    Assignments = 4,
    Names = 5,
    DefinitionPostings = 6,
    CallPostings = 7,
    FieldPostings = 8,
    ValuePostings = 9,
}

impl Section {
    fn record_size(self) -> usize {
        match self {
            Section::Strings => 1,
            Section::Files => FILE_RECORD_SIZE,
            Section::Definitions => DEFINITION_RECORD_SIZE,
            Section::Calls => CALL_RECORD_SIZE,
            Section::Assignments => ASSIGNMENT_RECORD_SIZE,
            Section::Names => NAME_RECORD_SIZE,
            Section::DefinitionPostings
            | Section::CallPostings
            | Section::FieldPostings
            | Section::ValuePostings => 4,
        }
    }
}

pub(crate) const FILE_RECORD_SIZE: usize = 80;
pub(crate) const DEFINITION_RECORD_SIZE: usize = 68;
pub(crate) const CALL_RECORD_SIZE: usize = 28;
pub(crate) const ASSIGNMENT_RECORD_SIZE: usize = 32;
pub(crate) const NAME_RECORD_SIZE: usize = 40;

const FILE_FLAG_SYNTAX_ERRORS: u32 = 1;
const FILE_FLAG_PARSE_FAILED: u32 = 2;

const DEFINITION_FLAG_STATIC: u8 = 1;
const DEFINITION_FLAG_DECLARATION: u8 = 2;

const CALL_FLAG_MACRO_BODY: u8 = 1;

pub(crate) type ContentHash = [u8; 16];

/// What the writer needs to know about one file.
pub(crate) struct SegmentFileInput {
    pub path: Arc<str>,
    pub mtime: u64,
    pub size: u64,
    pub verified_at: u64,
    pub hash: ContentHash,
    pub language: SourceLanguage,
    pub extract: ExtractedFile,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FileRecord {
    pub path: (u32, u32),
    pub mtime: u64,
    pub size: u64,
    pub verified_at: u64,
    pub hash: ContentHash,
    pub definitions: (u32, u32),
    pub calls: (u32, u32),
    pub assignments: (u32, u32),
    pub flags: u32,
    pub language: SourceLanguage,
}

impl FileRecord {
    pub fn has_syntax_errors(&self) -> bool {
        self.flags & FILE_FLAG_SYNTAX_ERRORS != 0
    }

    pub fn parse_failed(&self) -> bool {
        self.flags & FILE_FLAG_PARSE_FAILED != 0
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DefinitionRecord {
    pub name: u32,
    pub file: u32,
    pub kind: DefinitionKind,
    pub is_static: bool,
    pub is_declaration: bool,
    pub name_range: Range,
    pub item_range: Range,
    pub calls: (u32, u32),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CallRecord {
    pub name: u32,
    pub file: u32,
    pub caller: Option<u32>,
    pub kind: CallKind,
    pub in_macro_body: bool,
    pub position: Position,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AssignmentRecord {
    pub field: u32,
    pub value: u32,
    pub container: Option<u32>,
    pub file: u32,
    pub kind: AssignmentKind,
    pub position: Position,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct NameRecord {
    pub text: (u32, u32),
    pub definitions: (u32, u32),
    pub calls: (u32, u32),
    pub fields: (u32, u32),
    pub values: (u32, u32),
}

/// Orders names case-insensitively (ASCII) first so that a case-insensitive prefix is a
/// contiguous range, then by bytes so that the order is total and exact lookups work.
pub(crate) fn compare_names(left: &[u8], right: &[u8]) -> Ordering {
    compare_folded(left, right).then_with(|| left.cmp(right))
}

pub(crate) fn compare_folded(left: &[u8], right: &[u8]) -> Ordering {
    let common = left.len().min(right.len());
    for index in 0..common {
        let (Some(a), Some(b)) = (left.get(index), right.get(index)) else {
            break;
        };
        let ordering = a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase());
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

pub(crate) fn checksum(bytes: &[u8]) -> u64 {
    let mut state = Checksum::new(bytes.len() as u64);
    let whole_words = bytes.len() / 8 * 8;
    let (words, remainder) = bytes.split_at(whole_words);
    state.update(words);
    state.finish(remainder)
}

/// A fast non-cryptographic checksum, computed incrementally so that a segment can be verified
/// by streaming reads instead of through the mapping (which would pull the whole file into the
/// process's working set).
struct Checksum {
    hash: u64,
}

impl Checksum {
    const MULTIPLIER: u64 = 0x517c_c1b7_2722_0a95;

    fn new(total_length: u64) -> Self {
        Self {
            hash: 0x9e37_79b9_7f4a_7c15 ^ total_length,
        }
    }

    /// `words` must be a whole number of 8-byte words.
    fn update(&mut self, words: &[u8]) {
        for chunk in words.chunks_exact(8) {
            let mut word = [0u8; 8];
            word.copy_from_slice(chunk);
            self.mix(u64::from_le_bytes(word));
        }
    }

    fn mix(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(Self::MULTIPLIER);
    }

    fn finish(mut self, remainder: &[u8]) -> u64 {
        if !remainder.is_empty() {
            let mut word = [0u8; 8];
            if let Some(prefix) = word.get_mut(..remainder.len().min(8)) {
                prefix.copy_from_slice(remainder.get(..prefix.len()).unwrap_or_default());
            }
            self.mix(u64::from_le_bytes(word));
        }
        self.hash
    }
}

fn checksum_file(file: &mut File, offset: u64, length: u64) -> std::io::Result<u64> {
    const CHUNK: usize = 1 << 20;
    file.seek(SeekFrom::Start(offset))?;
    let mut state = Checksum::new(length);
    let mut buffer = vec![0u8; CHUNK];
    let mut remaining = length;
    while remaining >= 8 {
        let want = usize::try_from(remaining.min(CHUNK as u64)).unwrap_or(CHUNK) / 8 * 8;
        let chunk = buffer.get_mut(..want).unwrap_or_default();
        file.read_exact(chunk)?;
        state.update(chunk);
        remaining -= want as u64;
    }
    let tail = buffer
        .get_mut(..usize::try_from(remaining).unwrap_or(0))
        .unwrap_or_default();
    file.read_exact(tail)?;
    Ok(state.finish(tail))
}

fn to_u32(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| anyhow!("segment too large: {value} entries"))
}

fn put_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(buffer: &mut Vec<u8>, value: u64) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn put_position(buffer: &mut Vec<u8>, position: Position) {
    put_u32(buffer, position.byte);
    put_u32(buffer, position.row);
    put_u32(buffer, position.column);
}

fn language_to_byte(language: SourceLanguage) -> u8 {
    match language {
        SourceLanguage::C => 0,
        SourceLanguage::Cpp => 1,
    }
}

fn language_from_byte(byte: u8) -> SourceLanguage {
    if byte == 1 {
        SourceLanguage::Cpp
    } else {
        SourceLanguage::C
    }
}

/// Whether `path` has the form the index stores: relative, `/`-separated, and without empty,
/// `.` or `..` components, so that joining it to the root cannot name a file outside the root.
pub(crate) fn is_valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                // On Windows these would start a new component or name a drive or stream.
                && !(cfg!(windows) && (component.contains('\\') || component.contains(':')))
        })
}

pub(crate) fn segment_file_name(id: u64) -> String {
    format!("seg-{id:016x}.bin")
}

pub(crate) fn parse_segment_file_name(name: &str) -> Option<u64> {
    let hex = name.strip_prefix("seg-")?.strip_suffix(".bin")?;
    u64::from_str_radix(hex, 16).ok()
}

/// Options for creating index files. The index holds the names in the user's sources, so on
/// Unix only the user may read or write its files.
pub(crate) fn private_file_options() -> fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut options = File::options();
        options.mode(0o600);
        options
    }
    #[cfg(not(unix))]
    File::options()
}

/// Creates `directory` (and its parents) so that on Unix only the user may enter it; another
/// user's program truncating a mapped segment would otherwise crash the editor.
pub(crate) fn create_private_directory(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .with_context(|| format!("creating index directory {}", directory.display()))?;
        // A directory made by an older version, or with a looser umask, is tightened.
        let mode = fs::metadata(directory)
            .with_context(|| format!("reading index directory {}", directory.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0
            && let Err(error) = fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        {
            log::warn!(
                "symbol_index: cannot restrict access to {}: {error}",
                directory.display()
            );
        }
    }
    #[cfg(not(unix))]
    fs::create_dir_all(directory)
        .with_context(|| format!("creating index directory {}", directory.display()))?;
    Ok(())
}

/// Writes `bytes` to `path` so that `path` either keeps its old contents or has all of the new
/// ones, even if the process or the machine dies midway.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid index file path {path:?}"))?;
    let temporary = path.with_file_name(format!("{file_name}.tmp"));
    {
        let mut file = private_file_options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing {}", temporary.display()))?;
    }
    fs::rename(&temporary, path)
        .with_context(|| format!("renaming {} into place", temporary.display()))?;
    // On Unix the rename itself is only durable once the directory is flushed: after a power
    // loss the manifest could otherwise name a segment whose rename was lost. The standard
    // library cannot open a directory for flushing on Windows.
    #[cfg(unix)]
    if let Some(directory) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        File::open(directory)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("flushing the directory {}", directory.display()))?;
    }
    Ok(())
}

/// Serializes the extraction results of `files` into a new segment at `path`.
pub(crate) fn write_segment(path: &Path, created: u64, files: &[SegmentFileInput]) -> Result<()> {
    let bytes = encode_segment(created, files)?;
    write_atomically(path, &bytes)
}

fn encode_segment(created: u64, files: &[SegmentFileInput]) -> Result<Vec<u8>> {
    // Intern every name so that records store 4-byte ids.
    let mut interned: FxHashMap<&str, u32> = FxHashMap::default();
    let mut names: Vec<&str> = Vec::new();
    for file in files {
        let extract = &file.extract;
        let strings = extract
            .definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .chain(extract.calls.iter().map(|call| call.callee.as_str()))
            .chain(extract.assignments.iter().flat_map(|assignment| {
                [
                    Some(assignment.field.as_str()),
                    Some(assignment.value.as_str()),
                    assignment.container.as_deref(),
                ]
                .into_iter()
                .flatten()
            }));
        for name in strings {
            if !interned.contains_key(name) {
                interned.insert(name, to_u32(names.len())?);
                names.push(name);
            }
        }
    }

    let mut order: Vec<u32> = (0..to_u32(names.len())?).collect();
    order.sort_unstable_by(|&left, &right| {
        let left = names.get(left as usize).copied().unwrap_or_default();
        let right = names.get(right as usize).copied().unwrap_or_default();
        compare_names(left.as_bytes(), right.as_bytes())
    });
    let mut final_id = vec![0u32; names.len()];
    for (position, &temporary_id) in order.iter().enumerate() {
        if let Some(slot) = final_id.get_mut(temporary_id as usize) {
            *slot = to_u32(position)?;
        }
    }
    let name_id = |name: &str| -> u32 {
        interned
            .get(name)
            .and_then(|&temporary| final_id.get(temporary as usize).copied())
            .unwrap_or(NONE)
    };

    let mut strings: Vec<u8> = Vec::new();
    let mut name_text: Vec<(u32, u32)> = Vec::with_capacity(names.len());
    for &temporary_id in &order {
        let name = names
            .get(temporary_id as usize)
            .copied()
            .unwrap_or_default();
        name_text.push((to_u32(strings.len())?, to_u32(name.len())?));
        strings.extend_from_slice(name.as_bytes());
    }

    let mut file_section = Vec::with_capacity(files.len() * FILE_RECORD_SIZE);
    let mut definition_section = Vec::new();
    let mut call_section = Vec::new();
    let mut assignment_section = Vec::new();
    let mut definition_names: Vec<u32> = Vec::new();
    let mut call_names: Vec<u32> = Vec::new();
    let mut field_names: Vec<u32> = Vec::new();
    let mut value_names: Vec<u32> = Vec::new();

    for (file_index, file) in files.iter().enumerate() {
        let file_index = to_u32(file_index)?;
        let path_offset = to_u32(strings.len())?;
        strings.extend_from_slice(file.path.as_bytes());
        let extract = &file.extract;
        let definition_start = to_u32(definition_names.len())?;
        let call_start = to_u32(call_names.len())?;
        let assignment_start = to_u32(field_names.len())?;

        // Calls are grouped by the function containing them so that each definition can
        // point at one contiguous run of its callees.
        let mut calls: Vec<&ExtractedCall> = extract.calls.iter().collect();
        calls.sort_by_key(|call| (call.caller.unwrap_or(NONE), call.position.byte));
        let mut call_runs: FxHashMap<u32, (u32, u32)> = FxHashMap::default();
        for (offset, call) in calls.iter().enumerate() {
            if let Some(caller) = call.caller {
                let index = call_start + to_u32(offset)?;
                let run = call_runs.entry(caller).or_insert((index, 0));
                run.1 += 1;
            }
        }

        for (local_index, definition) in extract.definitions.iter().enumerate() {
            let local_index = to_u32(local_index)?;
            let name = name_id(&definition.name);
            definition_names.push(name);
            put_u32(&mut definition_section, name);
            put_u32(&mut definition_section, file_index);
            let mut flags = 0u8;
            if definition.is_static {
                flags |= DEFINITION_FLAG_STATIC;
            }
            if definition.is_declaration {
                flags |= DEFINITION_FLAG_DECLARATION;
            }
            definition_section.extend_from_slice(&[definition.kind.to_byte(), flags, 0, 0]);
            put_position(&mut definition_section, definition.name_range.start);
            put_position(&mut definition_section, definition.name_range.end);
            put_position(&mut definition_section, definition.item_range.start);
            put_position(&mut definition_section, definition.item_range.end);
            let (run_start, run_count) = call_runs
                .get(&local_index)
                .copied()
                .unwrap_or((call_start, 0));
            put_u32(&mut definition_section, run_start);
            put_u32(&mut definition_section, run_count);
        }

        for call in &calls {
            let name = name_id(&call.callee);
            call_names.push(name);
            put_u32(&mut call_section, name);
            put_u32(&mut call_section, file_index);
            let caller = match call.caller {
                Some(local) => definition_start
                    .checked_add(local)
                    .filter(|&global| {
                        local < to_u32(extract.definitions.len()).unwrap_or(0) && global != NONE
                    })
                    .unwrap_or(NONE),
                None => NONE,
            };
            put_u32(&mut call_section, caller);
            let kind = match call.kind {
                CallKind::Direct => 0u8,
                CallKind::Member => 1u8,
            };
            let flags = if call.in_macro_body {
                CALL_FLAG_MACRO_BODY
            } else {
                0
            };
            call_section.extend_from_slice(&[kind, flags, 0, 0]);
            put_position(&mut call_section, call.position);
        }

        for assignment in &extract.assignments {
            let field = name_id(&assignment.field);
            let value = name_id(&assignment.value);
            field_names.push(field);
            value_names.push(value);
            put_u32(&mut assignment_section, field);
            put_u32(&mut assignment_section, value);
            put_u32(
                &mut assignment_section,
                assignment.container.as_deref().map_or(NONE, name_id),
            );
            put_u32(&mut assignment_section, file_index);
            let kind = match assignment.kind {
                AssignmentKind::DesignatedInitializer => 0u8,
                AssignmentKind::Assignment => 1u8,
            };
            assignment_section.extend_from_slice(&[kind, 0, 0, 0]);
            put_position(&mut assignment_section, assignment.position);
        }

        let mut flags = 0u32;
        if extract.has_syntax_errors {
            flags |= FILE_FLAG_SYNTAX_ERRORS;
        }
        if extract.parse_failed {
            flags |= FILE_FLAG_PARSE_FAILED;
        }
        put_u32(&mut file_section, path_offset);
        put_u32(&mut file_section, to_u32(file.path.len())?);
        put_u64(&mut file_section, file.mtime);
        put_u64(&mut file_section, file.size);
        put_u64(&mut file_section, file.verified_at);
        file_section.extend_from_slice(&file.hash);
        put_u32(&mut file_section, definition_start);
        put_u32(&mut file_section, to_u32(extract.definitions.len())?);
        put_u32(&mut file_section, call_start);
        put_u32(&mut file_section, to_u32(calls.len())?);
        put_u32(&mut file_section, assignment_start);
        put_u32(&mut file_section, to_u32(extract.assignments.len())?);
        put_u32(&mut file_section, flags);
        file_section.extend_from_slice(&[language_to_byte(file.language), 0, 0, 0]);
    }

    let name_count = names.len();
    let (definition_postings, definition_runs) = build_postings(&definition_names, name_count)?;
    let (call_postings, call_runs) = build_postings(&call_names, name_count)?;
    let (field_postings, field_runs) = build_postings(&field_names, name_count)?;
    let (value_postings, value_runs) = build_postings(&value_names, name_count)?;

    let mut name_section = Vec::with_capacity(name_count * NAME_RECORD_SIZE);
    for index in 0..name_count {
        let (offset, length) = name_text.get(index).copied().unwrap_or((0, 0));
        put_u32(&mut name_section, offset);
        put_u32(&mut name_section, length);
        for runs in [&definition_runs, &call_runs, &field_runs, &value_runs] {
            let (start, count) = runs.get(index).copied().unwrap_or((0, 0));
            put_u32(&mut name_section, start);
            put_u32(&mut name_section, count);
        }
    }

    let sections: [(Section, Vec<u8>); SECTION_COUNT] = [
        (Section::Strings, strings),
        (Section::Files, file_section),
        (Section::Definitions, definition_section),
        (Section::Calls, call_section),
        (Section::Assignments, assignment_section),
        (Section::Names, name_section),
        (
            Section::DefinitionPostings,
            postings_bytes(&definition_postings),
        ),
        (Section::CallPostings, postings_bytes(&call_postings)),
        (Section::FieldPostings, postings_bytes(&field_postings)),
        (Section::ValuePostings, postings_bytes(&value_postings)),
    ];

    let body_length: usize = sections.iter().map(|(_, bytes)| bytes.len()).sum();
    let mut output = Vec::with_capacity(HEADER_SIZE + body_length);
    output.extend_from_slice(SEGMENT_MAGIC);
    put_u32(&mut output, FORMAT_VERSION);
    put_u32(&mut output, 0);
    put_u64(&mut output, (HEADER_SIZE + body_length) as u64);
    put_u64(&mut output, created);
    let checksum_offset = output.len();
    put_u64(&mut output, 0);
    let mut offset = HEADER_SIZE as u64;
    for (section, bytes) in &sections {
        put_u64(&mut output, offset);
        put_u64(&mut output, (bytes.len() / section.record_size()) as u64);
        offset += bytes.len() as u64;
    }
    debug_assert_eq!(output.len(), HEADER_SIZE);
    for (_, bytes) in &sections {
        output.extend_from_slice(bytes);
    }
    let body_checksum = checksum(output.get(HEADER_SIZE..).unwrap_or_default());
    if let Some(slot) = output.get_mut(checksum_offset..checksum_offset + 8) {
        slot.copy_from_slice(&body_checksum.to_le_bytes());
    }
    Ok(output)
}

fn build_postings(owners: &[u32], name_count: usize) -> Result<(Vec<u32>, Vec<(u32, u32)>)> {
    let mut counts = vec![0u32; name_count];
    for &owner in owners {
        if let Some(count) = counts.get_mut(owner as usize) {
            *count += 1;
        }
    }
    let mut runs = Vec::with_capacity(name_count);
    let mut next = 0u32;
    for &count in &counts {
        runs.push((next, count));
        next += count;
    }
    let mut fill: Vec<u32> = runs.iter().map(|&(start, _)| start).collect();
    let mut postings = vec![0u32; next as usize];
    for (record, &owner) in owners.iter().enumerate() {
        if let Some(cursor) = fill.get_mut(owner as usize)
            && let Some(slot) = postings.get_mut(*cursor as usize)
        {
            *slot = to_u32(record)?;
            *cursor += 1;
        }
    }
    Ok((postings, runs))
}

fn postings_bytes(postings: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(postings.len() * 4);
    for &posting in postings {
        put_u32(&mut bytes, posting);
    }
    bytes
}

#[derive(Clone, Copy, Debug, Default)]
struct SectionBounds {
    offset: usize,
    count: usize,
}

pub(crate) struct Segment {
    pub id: u64,
    map: Mmap,
    sections: [SectionBounds; SECTION_COUNT],
}

impl Segment {
    pub fn open(directory: &Path, id: u64, verify_checksum: bool) -> Result<Self> {
        let path = directory.join(segment_file_name(id));
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: segment files are never modified after they are renamed into place, and the
        // lock file keeps other instances out of the directory. Deleting a mapped file (Unix)
        // keeps the mapping valid, and Windows refuses to delete or truncate one. Every access
        // below is bounds-checked against the mapped length. What remains is another program
        // truncating the file, which would fault the process on Unix (SIGBUS); the index
        // directory is private to the user there, so only the user's own programs could, and
        // that is not guarded against.
        let map =
            unsafe { Mmap::map(&file) }.with_context(|| format!("mapping {}", path.display()))?;
        let bytes: &[u8] = &map;
        let header = bytes
            .get(..HEADER_SIZE)
            .ok_or_else(|| anyhow!("{} is truncated", path.display()))?;
        if header.get(..8) != Some(SEGMENT_MAGIC.as_slice()) {
            bail!("{} is not a segment", path.display());
        }
        let version = read_u32(header, 8).unwrap_or(0);
        if version != FORMAT_VERSION {
            bail!(
                "{} has format version {version}, expected {FORMAT_VERSION}",
                path.display()
            );
        }
        let total_length = read_u64(header, 16).unwrap_or(0);
        if total_length != bytes.len() as u64 {
            bail!(
                "{} has length {}, header says {total_length}",
                path.display(),
                bytes.len()
            );
        }
        let expected_checksum = read_u64(header, 32).unwrap_or(0);
        let mut sections = [SectionBounds::default(); SECTION_COUNT];
        let all_sections = [
            Section::Strings,
            Section::Files,
            Section::Definitions,
            Section::Calls,
            Section::Assignments,
            Section::Names,
            Section::DefinitionPostings,
            Section::CallPostings,
            Section::FieldPostings,
            Section::ValuePostings,
        ];
        for section in all_sections {
            let entry = 40 + section as usize * 16;
            let offset = read_u64(header, entry).unwrap_or(u64::MAX);
            let count = read_u64(header, entry + 8).unwrap_or(u64::MAX);
            let end = count
                .checked_mul(section.record_size() as u64)
                .and_then(|length| length.checked_add(offset));
            match end {
                Some(end) if offset >= HEADER_SIZE as u64 && end <= total_length => {}
                _ => bail!("{} has a section out of bounds", path.display()),
            }
            let (Ok(offset), Ok(count)) = (usize::try_from(offset), usize::try_from(count)) else {
                bail!("{} has a section out of bounds", path.display());
            };
            if let Some(slot) = sections.get_mut(section as usize) {
                *slot = SectionBounds { offset, count };
            }
        }
        if verify_checksum {
            let mut file = file;
            let actual = checksum_file(
                &mut file,
                HEADER_SIZE as u64,
                total_length - HEADER_SIZE as u64,
            )
            .with_context(|| format!("reading {}", path.display()))?;
            if actual != expected_checksum {
                bail!("{} failed its checksum", path.display());
            }
        }
        Ok(Self { id, map, sections })
    }

    pub fn path(directory: &Path, id: u64) -> PathBuf {
        directory.join(segment_file_name(id))
    }

    pub fn byte_length(&self) -> u64 {
        self.map.len() as u64
    }

    fn count(&self, section: Section) -> usize {
        self.sections
            .get(section as usize)
            .map_or(0, |bounds| bounds.count)
    }

    fn record(&self, section: Section, index: u32) -> Option<&[u8]> {
        let bounds = self.sections.get(section as usize)?;
        let index = index as usize;
        if index >= bounds.count {
            return None;
        }
        let size = section.record_size();
        let start = bounds.offset.checked_add(index.checked_mul(size)?)?;
        self.map.get(start..start.checked_add(size)?)
    }

    pub fn file_count(&self) -> u32 {
        u32::try_from(self.count(Section::Files)).unwrap_or(u32::MAX)
    }

    pub fn name_count(&self) -> u32 {
        u32::try_from(self.count(Section::Names)).unwrap_or(u32::MAX)
    }

    pub fn string(&self, text: (u32, u32)) -> Option<&str> {
        std::str::from_utf8(self.string_bytes(text)?).ok()
    }

    pub fn string_bytes(&self, (offset, length): (u32, u32)) -> Option<&[u8]> {
        let bounds = self.sections.get(Section::Strings as usize)?;
        let start = (offset as usize).checked_add(bounds.offset)?;
        let end = start.checked_add(length as usize)?;
        if (offset as usize).checked_add(length as usize)? > bounds.count {
            return None;
        }
        self.map.get(start..end)
    }

    pub fn file(&self, index: u32) -> Option<FileRecord> {
        let record = self.record(Section::Files, index)?;
        let mut hash = [0u8; 16];
        hash.copy_from_slice(record.get(32..48)?);
        Some(FileRecord {
            path: (read_u32(record, 0)?, read_u32(record, 4)?),
            mtime: read_u64(record, 8)?,
            size: read_u64(record, 16)?,
            verified_at: read_u64(record, 24)?,
            hash,
            definitions: (read_u32(record, 48)?, read_u32(record, 52)?),
            calls: (read_u32(record, 56)?, read_u32(record, 60)?),
            assignments: (read_u32(record, 64)?, read_u32(record, 68)?),
            flags: read_u32(record, 72)?,
            language: language_from_byte(*record.get(76)?),
        })
    }

    pub fn definition(&self, index: u32) -> Option<DefinitionRecord> {
        let record = self.record(Section::Definitions, index)?;
        let flags = *record.get(9)?;
        Some(DefinitionRecord {
            name: read_u32(record, 0)?,
            file: read_u32(record, 4)?,
            kind: DefinitionKind::from_byte(*record.get(8)?)?,
            is_static: flags & DEFINITION_FLAG_STATIC != 0,
            is_declaration: flags & DEFINITION_FLAG_DECLARATION != 0,
            name_range: Range {
                start: read_position(record, 12)?,
                end: read_position(record, 24)?,
            },
            item_range: Range {
                start: read_position(record, 36)?,
                end: read_position(record, 48)?,
            },
            calls: (read_u32(record, 60)?, read_u32(record, 64)?),
        })
    }

    pub fn call(&self, index: u32) -> Option<CallRecord> {
        let record = self.record(Section::Calls, index)?;
        let caller = read_u32(record, 8)?;
        Some(CallRecord {
            name: read_u32(record, 0)?,
            file: read_u32(record, 4)?,
            caller: (caller != NONE).then_some(caller),
            kind: if *record.get(12)? == 1 {
                CallKind::Member
            } else {
                CallKind::Direct
            },
            in_macro_body: *record.get(13)? & CALL_FLAG_MACRO_BODY != 0,
            position: read_position(record, 16)?,
        })
    }

    pub fn assignment(&self, index: u32) -> Option<AssignmentRecord> {
        let record = self.record(Section::Assignments, index)?;
        let container = read_u32(record, 8)?;
        Some(AssignmentRecord {
            field: read_u32(record, 0)?,
            value: read_u32(record, 4)?,
            container: (container != NONE).then_some(container),
            file: read_u32(record, 12)?,
            kind: if *record.get(16)? == 1 {
                AssignmentKind::Assignment
            } else {
                AssignmentKind::DesignatedInitializer
            },
            position: read_position(record, 20)?,
        })
    }

    pub fn name(&self, index: u32) -> Option<NameRecord> {
        let record = self.record(Section::Names, index)?;
        Some(NameRecord {
            text: (read_u32(record, 0)?, read_u32(record, 4)?),
            definitions: (read_u32(record, 8)?, read_u32(record, 12)?),
            calls: (read_u32(record, 16)?, read_u32(record, 20)?),
            fields: (read_u32(record, 24)?, read_u32(record, 28)?),
            values: (read_u32(record, 32)?, read_u32(record, 36)?),
        })
    }

    pub fn name_text(&self, index: u32) -> Option<&str> {
        self.string(self.name(index)?.text)
    }

    fn name_bytes(&self, index: u32) -> &[u8] {
        self.name(index)
            .and_then(|record| self.string_bytes(record.text))
            .unwrap_or_default()
    }

    pub fn find_name(&self, name: &str) -> Option<u32> {
        let target = name.as_bytes();
        let (mut low, mut high) = (0u32, self.name_count());
        while low < high {
            let middle = low + (high - low) / 2;
            match compare_names(self.name_bytes(middle), target) {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => return Some(middle),
            }
        }
        None
    }

    /// The text of every name back to back, in name order: the writer puts the names at the
    /// start of the string section, in the order of the name records. Scanning this instead of
    /// the name records reads a fraction of the bytes.
    pub fn names_text(&self) -> &[u8] {
        let Some(strings) = self.sections.get(Section::Strings as usize) else {
            return &[];
        };
        let end = self
            .name_count()
            .checked_sub(1)
            .and_then(|last| self.name(last))
            .map_or(0, |record| {
                (record.text.0 as usize).saturating_add(record.text.1 as usize)
            })
            .min(strings.count);
        strings
            .offset
            .checked_add(end)
            .and_then(|end| self.map.get(strings.offset..end))
            .unwrap_or_default()
    }

    /// The first name at or after `from` whose text ends after `byte` of [`Self::names_text`],
    /// which is the name containing that byte when there is one. Names are visited in
    /// increasing order, so this gallops forward from the previous answer rather than
    /// searching all names. A damaged segment yields some index, never a panic.
    pub fn name_at_text_offset(&self, from: u32, byte: usize) -> u32 {
        let count = self.name_count();
        let ends_by = |index: u32| {
            self.name(index).is_some_and(|record| {
                (record.text.0 as usize).saturating_add(record.text.1 as usize) <= byte
            })
        };
        if from >= count || !ends_by(from) {
            return from.min(count);
        }
        let mut low = from;
        let mut step = 1u32;
        let high = loop {
            let probe = low.saturating_add(step);
            if probe >= count {
                break count;
            }
            if !ends_by(probe) {
                break probe;
            }
            low = probe;
            step = step.saturating_mul(2);
        };
        let (mut low, mut high) = (low + 1, high);
        while low < high {
            let middle = low + (high - low) / 2;
            if ends_by(middle) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    /// First name whose case-folded form is not less than `prefix`.
    pub fn folded_lower_bound(&self, prefix: &[u8]) -> u32 {
        let (mut low, mut high) = (0u32, self.name_count());
        while low < high {
            let middle = low + (high - low) / 2;
            if compare_folded(self.name_bytes(middle), prefix) == Ordering::Less {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    /// The record indices of a `(start, count)` run read from another record, limited to the
    /// records that exist. A damaged count must not make a query step through billions of
    /// missing records.
    fn clamped_run(&self, section: Section, (start, count): (u32, u32)) -> ops::Range<u32> {
        let available = u32::try_from(self.count(section)).unwrap_or(u32::MAX);
        let start = start.min(available);
        start..start.saturating_add(count).min(available)
    }

    pub fn definition_run(&self, run: (u32, u32)) -> ops::Range<u32> {
        self.clamped_run(Section::Definitions, run)
    }

    pub fn call_run(&self, run: (u32, u32)) -> ops::Range<u32> {
        self.clamped_run(Section::Calls, run)
    }

    pub fn assignment_run(&self, run: (u32, u32)) -> ops::Range<u32> {
        self.clamped_run(Section::Assignments, run)
    }

    fn posting(&self, section: Section, index: u32) -> Option<u32> {
        read_u32(self.record(section, index)?, 0)
    }

    fn postings(&self, section: Section, run: (u32, u32)) -> impl Iterator<Item = u32> {
        self.clamped_run(section, run)
            .filter_map(move |index| self.posting(section, index))
    }

    pub fn definition_postings(&self, run: (u32, u32)) -> impl Iterator<Item = u32> {
        self.postings(Section::DefinitionPostings, run)
    }

    pub fn call_postings(&self, run: (u32, u32)) -> impl Iterator<Item = u32> {
        self.postings(Section::CallPostings, run)
    }

    pub fn field_postings(&self, run: (u32, u32)) -> impl Iterator<Item = u32> {
        self.postings(Section::FieldPostings, run)
    }

    pub fn value_postings(&self, run: (u32, u32)) -> impl Iterator<Item = u32> {
        self.postings(Section::ValuePostings, run)
    }

    /// `None` for a path that could leave the index root, which only a damaged segment holds:
    /// callers join these paths to the root.
    pub fn file_path(&self, file: &FileRecord) -> Option<&str> {
        self.string(file.path)
            .filter(|path| is_valid_relative_path(path))
    }

    /// Rebuilds the extraction result of one file, for compaction.
    pub fn materialize(&self, file_index: u32) -> Option<(FileRecord, ExtractedFile)> {
        let file = self.file(file_index)?;
        let (definition_start, definition_count) = file.definitions;
        let mut extract = ExtractedFile {
            has_syntax_errors: file.has_syntax_errors(),
            parse_failed: file.parse_failed(),
            ..ExtractedFile::default()
        };
        // Unlike queries, a run that points past the records makes the whole file unreadable
        // here, so that compaction does not carry a damaged file forward.
        for index in definition_start..definition_start.saturating_add(definition_count) {
            let record = self.definition(index)?;
            extract.definitions.push(ExtractedDefinition {
                name: self.name_text(record.name)?.to_owned(),
                kind: record.kind,
                is_static: record.is_static,
                is_declaration: record.is_declaration,
                name_range: record.name_range,
                item_range: record.item_range,
            });
        }
        let (call_start, call_count) = file.calls;
        for index in call_start..call_start.saturating_add(call_count) {
            let record = self.call(index)?;
            extract.calls.push(ExtractedCall {
                callee: self.name_text(record.name)?.to_owned(),
                caller: record
                    .caller
                    .and_then(|caller| caller.checked_sub(definition_start)),
                kind: record.kind,
                in_macro_body: record.in_macro_body,
                position: record.position,
            });
        }
        let (assignment_start, assignment_count) = file.assignments;
        for index in assignment_start..assignment_start.saturating_add(assignment_count) {
            let record = self.assignment(index)?;
            extract.assignments.push(ExtractedAssignment {
                field: self.name_text(record.field)?.to_owned(),
                value: self.name_text(record.value)?.to_owned(),
                container: match record.container {
                    Some(container) => Some(self.name_text(container)?.to_owned()),
                    None => None,
                },
                kind: record.kind,
                position: record.position,
            });
        }
        Some((file, extract))
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(slice.try_into().ok()?))
}

fn read_position(bytes: &[u8], offset: usize) -> Option<Position> {
    Some(Position {
        byte: read_u32(bytes, offset)?,
        row: read_u32(bytes, offset + 4)?,
        column: read_u32(bytes, offset + 8)?,
    })
}

/// A metadata-only update: the file's contents hash matched, so only its timestamps changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MetadataOverride {
    pub segment: u64,
    pub file_index: u32,
    pub mtime: u64,
    pub verified_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestSegment {
    pub id: u64,
    pub file_count: u32,
    pub dead: Vec<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub next_segment_id: u64,
    pub skipped_too_large: u64,
    pub skipped_unsupported: u64,
    pub segments: Vec<ManifestSegment>,
    pub overrides: Vec<MetadataOverride>,
}

pub(crate) const MANIFEST_FILE_NAME: &str = "manifest.bin";

impl Manifest {
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(MANIFEST_MAGIC);
        put_u32(&mut output, FORMAT_VERSION);
        put_u32(&mut output, 0);
        put_u64(&mut output, self.next_segment_id);
        put_u64(&mut output, self.skipped_too_large);
        put_u64(&mut output, self.skipped_unsupported);
        put_u64(&mut output, self.segments.len() as u64);
        put_u64(&mut output, self.overrides.len() as u64);
        for segment in &self.segments {
            put_u64(&mut output, segment.id);
            put_u32(&mut output, segment.file_count);
            put_u32(&mut output, segment.dead.len() as u32);
            for &word in &segment.dead {
                put_u64(&mut output, word);
            }
        }
        for metadata in &self.overrides {
            put_u64(&mut output, metadata.segment);
            put_u32(&mut output, metadata.file_index);
            put_u32(&mut output, 0);
            put_u64(&mut output, metadata.mtime);
            put_u64(&mut output, metadata.verified_at);
        }
        let sum = checksum(&output);
        put_u64(&mut output, sum);
        output
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let body_length = bytes
            .len()
            .checked_sub(8)
            .ok_or_else(|| anyhow!("manifest is truncated"))?;
        let (body, trailer) = bytes.split_at(body_length);
        if read_u64(trailer, 0) != Some(checksum(body)) {
            bail!("manifest failed its checksum");
        }
        if body.get(..8) != Some(MANIFEST_MAGIC.as_slice()) {
            bail!("not a manifest");
        }
        let version = read_u32(body, 8).unwrap_or(0);
        if version != FORMAT_VERSION {
            bail!("manifest has format version {version}, expected {FORMAT_VERSION}");
        }
        let truncated = || anyhow!("manifest is truncated");
        let mut offset = 16;
        let next_u64 = |offset: &mut usize| -> Result<u64> {
            let value = read_u64(body, *offset).ok_or_else(truncated)?;
            *offset += 8;
            Ok(value)
        };
        let next_segment_id = next_u64(&mut offset)?;
        let skipped_too_large = next_u64(&mut offset)?;
        let skipped_unsupported = next_u64(&mut offset)?;
        let segment_count = next_u64(&mut offset)?;
        let override_count = next_u64(&mut offset)?;
        let mut segments = Vec::new();
        for _ in 0..segment_count {
            let id = next_u64(&mut offset)?;
            let file_count = read_u32(body, offset).ok_or_else(truncated)?;
            let word_count = read_u32(body, offset + 4).ok_or_else(truncated)?;
            offset += 8;
            if word_count as usize != (file_count as usize).div_ceil(64) {
                bail!("manifest has a malformed tombstone set");
            }
            // Checked before allocating so that a damaged count cannot request gigabytes.
            if body.len().saturating_sub(offset) / 8 < word_count as usize {
                return Err(truncated());
            }
            let mut dead = Vec::with_capacity(word_count as usize);
            for _ in 0..word_count {
                dead.push(next_u64(&mut offset)?);
            }
            segments.push(ManifestSegment {
                id,
                file_count,
                dead,
            });
        }
        let mut overrides = Vec::new();
        for _ in 0..override_count {
            let segment = next_u64(&mut offset)?;
            let file_index = read_u32(body, offset).ok_or_else(truncated)?;
            offset += 8;
            let mtime = next_u64(&mut offset)?;
            let verified_at = next_u64(&mut offset)?;
            overrides.push(MetadataOverride {
                segment,
                file_index,
                mtime,
                verified_at,
            });
        }
        if offset != body.len() {
            bail!("manifest has trailing data");
        }
        Ok(Self {
            next_segment_id,
            skipped_too_large,
            skipped_unsupported,
            segments,
            overrides,
        })
    }
}
