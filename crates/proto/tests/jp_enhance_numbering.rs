//! zed-jp-enhance: the fork's messages and fields must keep out of the numbers
//! upstream hands out, so that merging an upstream release never gives one
//! number to two messages (or two fields) and never silently reinterprets what
//! an older peer sends.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use proto::{Envelope, KillTerminalSession, Message as _, SetEgressAllowlist, envelope};

/// Upstream numbers `Envelope` sequentially from 4 and was at 493 in v1.21.0,
/// growing by roughly a hundred a year. From 2048 up a tag takes three bytes
/// instead of two either way, and 19000..=19999 is reserved by protobuf.
const FORK_BAND: Range<u32> = 10_000..19_000;

/// Files that hold only the fork's messages.
const FORK_FILES: &[&str] = &["jp_enhance.proto", "file_transfer.proto"];

/// Fields the fork added to upstream's messages, as `(message, field)`.
const FORK_FIELDS_IN_UPSTREAM_MESSAGES: &[(&str, &str)] = &[
    ("GetAgentServerCommand", "project_roots"),
    ("GetAgentServerCommand", "approve_update"),
    ("AgentServerCommand", "sandbox"),
    ("AgentServerCommand", "sandbox_error"),
    ("Entry", "is_private"),
    ("AddWorktree", "code_runs_without_ide_decision"),
    ("UpdateRepository", "branch_list_unchanged"),
    ("UpdateRepository", "status_error"),
    ("UpdateRepository", "tags_fingerprint"),
    ("UpdateRepository", "loose_ref_count"),
    ("UpdateRepository", "loose_ref_count_is_lower_bound"),
    ("UpdateRepository", "opened_by_user"),
    ("Push", "force_without_lease"),
    ("RemoteMessageResponse", "forced_without_lease"),
    ("SearchCommits", "author"),
    ("SearchCommits", "more_authors"),
    ("SearchCommits", "only_commits"),
    ("SearchCommits", "branch"),
    ("SearchCommits", "path"),
    ("SearchCommits", "since"),
    ("SearchCommits", "until"),
    ("SearchCommits", "change"),
    ("SearchCommits", "merges"),
    ("SearchCommits", "sha_prefix"),
    ("SearchCommits", "search_id"),
    ("SearchCommitsResponse", "filters_version"),
    ("SearchCommitsResponse", "cancellable"),
    ("StatusUpdate", "egress_blocked_hosts"),
    ("LanguageServerShowDocumentRequest", "language_server_name"),
];

/// Values the fork added to upstream's enums, as `(enum, value)`.
const FORK_VALUES_IN_UPSTREAM_ENUMS: &[(&str, &str)] = &[
    ("GitReset.ResetMode", "HARD"),
    ("Push.PushOptions", "FORCE_WITHOUT_LEASE"),
];

/// Fork numbers that once meant something else and must never be given out
/// again, as `(message, number)`: a peer of the old revision would read the
/// new field as the old one.
const RETIRED_FORK_NUMBERS: &[(&str, u32)] = &[
    // `bool code_execution_follows_ide`, read as false by a host that did
    // not know it (wire revision 2 replaced it).
    ("AddWorktree", 10000),
];

#[derive(Debug)]
enum Item {
    Field {
        type_name: String,
        oneof: Option<String>,
    },
    EnumValue,
}

#[derive(Debug)]
struct Numbered {
    file: String,
    /// The enclosing message or enum, dotted when nested (`GitReset.ResetMode`).
    scope: String,
    name: String,
    number: u32,
    item: Item,
}

#[derive(Default)]
struct Protocol {
    numbered: Vec<Numbered>,
    /// Every message and enum, by its dotted name, and the file defining it.
    definitions: BTreeMap<String, String>,
    /// The numbers each message or enum declares `reserved`, by dotted name.
    reserved: BTreeMap<String, BTreeSet<u32>>,
}

enum Scope {
    Message(String),
    Enum(String),
    Oneof(String),
}

fn proto_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("proto")
}

fn parse_protocol() -> Protocol {
    parse_protocol_in(&proto_dir())
}

fn parse_protocol_in(directory: &Path) -> Protocol {
    let mut protocol = Protocol::default();
    let mut files = std::fs::read_dir(directory)
        .expect("reading the proto directory")
        .map(|entry| entry.expect("reading a proto directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "proto")
        })
        .collect::<Vec<_>>();
    files.sort();
    assert!(!files.is_empty(), "no .proto files found");
    for path in files {
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a proto file name")
            .to_string();
        let text = std::fs::read_to_string(&path).expect("reading a proto file");
        parse_file(&file, &text, &mut protocol);
    }
    protocol
}

fn dotted(scopes: &[Scope]) -> String {
    scopes
        .iter()
        .filter_map(|scope| match scope {
            Scope::Message(name) | Scope::Enum(name) => Some(name.as_str()),
            Scope::Oneof(_) => None,
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn parse_file(file: &str, text: &str, protocol: &mut Protocol) {
    let mut scopes: Vec<Scope> = Vec::new();
    for (line_index, raw_line) in text.lines().enumerate() {
        let line = raw_line.split("//").next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let location = format!("{file}:{}", line_index + 1);
        let mut words = line.split_whitespace();
        let first = words.next().unwrap_or_default();
        if matches!(first, "message" | "enum" | "oneof") && line.contains('{') {
            let name = words
                .next()
                .map(|name| name.trim_end_matches('{').to_string())
                .unwrap_or_else(|| panic!("{location}: a declaration without a name"));
            scopes.push(match first {
                "message" => Scope::Message(name),
                "enum" => Scope::Enum(name),
                _ => Scope::Oneof(name),
            });
            if first != "oneof" {
                protocol
                    .definitions
                    .insert(dotted(&scopes), file.to_string());
            }
            if line.ends_with('}') {
                scopes.pop();
            }
            continue;
        }
        if line == "}" {
            scopes
                .pop()
                .unwrap_or_else(|| panic!("{location}: an unmatched closing brace"));
            continue;
        }
        if first == "reserved" {
            let numbers = protocol.reserved.entry(dotted(&scopes)).or_default();
            let statement = line
                .trim_start_matches("reserved")
                .trim_end_matches(';')
                .trim();
            for part in statement.split(',').map(str::trim) {
                // Reserved names are quoted; only numbers matter here.
                if part.starts_with('"') {
                    continue;
                }
                let parse = |number: &str| {
                    number.trim().parse::<u32>().unwrap_or_else(|error| {
                        panic!("{location}: bad reserved number in {raw_line:?}: {error}")
                    })
                };
                match part.split_once(" to ") {
                    Some((start, end)) => numbers.extend(parse(start)..=parse(end)),
                    None => {
                        numbers.insert(parse(part));
                    }
                }
            }
            continue;
        }
        if matches!(first, "syntax" | "package" | "import" | "option") {
            continue;
        }
        let Some(statement) = line.strip_suffix(';') else {
            panic!("{location}: cannot read {raw_line:?}");
        };
        let (left, right) = statement
            .split_once('=')
            .unwrap_or_else(|| panic!("{location}: cannot read {raw_line:?}"));
        let number = right
            .split('[')
            .next()
            .unwrap_or_default()
            .trim()
            .parse::<u32>()
            .unwrap_or_else(|error| panic!("{location}: bad number in {raw_line:?}: {error}"));
        let scope = dotted(&scopes);
        let left = left.trim();
        let item = match scopes.last() {
            Some(Scope::Enum(_)) => Item::EnumValue,
            Some(Scope::Message(_)) | Some(Scope::Oneof(_)) => {
                let oneof = match scopes.last() {
                    Some(Scope::Oneof(name)) => Some(name.clone()),
                    _ => None,
                };
                let (type_name, _) = left
                    .rsplit_once(char::is_whitespace)
                    .unwrap_or_else(|| panic!("{location}: a field without a type"));
                let type_name = type_name
                    .trim()
                    .trim_start_matches("repeated ")
                    .trim_start_matches("optional ")
                    .trim()
                    .to_string();
                Item::Field { type_name, oneof }
            }
            None => panic!("{location}: a numbered line outside any message or enum"),
        };
        let name = left
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or_default()
            .to_string();
        protocol.numbered.push(Numbered {
            file: file.to_string(),
            scope,
            name,
            number,
            item,
        });
    }
    assert!(scopes.is_empty(), "{file}: unclosed braces");
}

fn is_fork_file(file: &str) -> bool {
    FORK_FILES.contains(&file)
}

#[test]
fn envelope_numbers_fork_messages_in_the_fork_band_and_nothing_else() {
    let protocol = parse_protocol();
    let variants = protocol
        .numbered
        .iter()
        .filter(|numbered| numbered.scope == "Envelope")
        .filter_map(|numbered| match &numbered.item {
            Item::Field {
                type_name,
                oneof: Some(oneof),
            } if oneof == "payload" => Some((numbered, type_name)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        variants.len() > 400,
        "found only {} Envelope variants; is the parser still reading zed.proto?",
        variants.len()
    );

    let mut problems = Vec::new();
    let mut fork_variants = 0;
    for (variant, type_name) in variants {
        let defined_in = protocol
            .definitions
            .get(type_name.as_str())
            .unwrap_or_else(|| panic!("Envelope carries {type_name}, which no file defines"));
        let in_band = FORK_BAND.contains(&variant.number);
        if is_fork_file(defined_in) {
            fork_variants += 1;
            if !in_band {
                problems.push(format!(
                    "{type_name} (from {defined_in}) = {}: the fork's messages take numbers in {FORK_BAND:?}",
                    variant.number
                ));
            }
        } else if variant.number >= FORK_BAND.start {
            problems.push(format!(
                "{type_name} (from {defined_in}) = {}: an upstream file's message in the fork's numbers; \
                 a fork message belongs in {FORK_FILES:?}",
                variant.number
            ));
        }
    }
    assert!(
        fork_variants > 0,
        "no Envelope variant carries a fork message"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn fork_fields_and_enum_values_in_upstream_messages_are_in_the_fork_band() {
    let protocol = parse_protocol();
    let expected_fields = FORK_FIELDS_IN_UPSTREAM_MESSAGES
        .iter()
        .map(|(scope, name)| (scope.to_string(), name.to_string()))
        .collect::<BTreeSet<_>>();
    let expected_values = FORK_VALUES_IN_UPSTREAM_ENUMS
        .iter()
        .map(|(scope, name)| (scope.to_string(), name.to_string()))
        .collect::<BTreeSet<_>>();

    let mut problems = Vec::new();
    let mut found_fields = BTreeSet::new();
    let mut found_values = BTreeSet::new();
    for numbered in protocol
        .numbered
        .iter()
        .filter(|numbered| !is_fork_file(&numbered.file) && numbered.scope != "Envelope")
    {
        let key = (numbered.scope.clone(), numbered.name.clone());
        let (expected, found) = match numbered.item {
            Item::Field { .. } => (&expected_fields, &mut found_fields),
            Item::EnumValue => (&expected_values, &mut found_values),
        };
        let listed = expected.contains(&key);
        if listed {
            found.insert(key);
        }
        let in_band = FORK_BAND.contains(&numbered.number);
        match (listed, in_band) {
            (true, false) => problems.push(format!(
                "{}.{} = {} ({}): the fork added it, so it takes a number in {FORK_BAND:?}",
                numbered.scope, numbered.name, numbered.number, numbered.file
            )),
            (false, _) if numbered.number >= FORK_BAND.start => problems.push(format!(
                "{}.{} = {} ({}): not listed as the fork's addition; list it in \
                 FORK_FIELDS_IN_UPSTREAM_MESSAGES / FORK_VALUES_IN_UPSTREAM_ENUMS, or give it \
                 upstream's next number if upstream added it",
                numbered.scope, numbered.name, numbered.number, numbered.file
            )),
            _ => {}
        }
    }
    for missing in expected_fields.difference(&found_fields) {
        problems.push(format!(
            "listed field {}.{} not found",
            missing.0, missing.1
        ));
    }
    for missing in expected_values.difference(&found_values) {
        problems.push(format!(
            "listed enum value {}.{} not found",
            missing.0, missing.1
        ));
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Every numbered field and enum value of upstream's `.proto` files at the
/// upstream release last merged. The two tests above can only check what is
/// listed or already in the band; this is what catches a fork addition to an
/// upstream message (or a fork message added to an upstream file) that was
/// given a small number and never listed, and an upstream number the fork
/// changed.
const UPSTREAM_NUMBERS_FILE: &str = "tests/jp_enhance_upstream_numbers.txt";

fn upstream_numbers_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(UPSTREAM_NUMBERS_FILE)
}

fn numbered_key(numbered: &Numbered) -> (String, String, u32) {
    (
        numbered.scope.clone(),
        numbered.name.clone(),
        numbered.number,
    )
}

#[test]
fn numbers_below_the_band_in_upstream_files_are_upstreams_own() {
    let recorded = std::fs::read_to_string(upstream_numbers_path())
        .expect("reading the upstream numbers file")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let mut words = line.split_whitespace();
            let scope = words.next().unwrap_or_default().to_string();
            let name = words.next().unwrap_or_default().to_string();
            let number = words
                .next()
                .unwrap_or_default()
                .parse::<u32>()
                .unwrap_or_else(|error| panic!("{UPSTREAM_NUMBERS_FILE}: {line:?}: {error}"));
            (scope, name, number)
        })
        .collect::<BTreeSet<_>>();
    assert!(
        recorded.len() > 2000,
        "{UPSTREAM_NUMBERS_FILE} holds only {} entries",
        recorded.len()
    );

    let protocol = parse_protocol();
    let current = protocol
        .numbered
        .iter()
        .filter(|numbered| !is_fork_file(&numbered.file))
        .filter(|numbered| numbered.number < FORK_BAND.start)
        .map(|numbered| (numbered_key(numbered), numbered.file.as_str()))
        .collect::<BTreeMap<_, _>>();

    let regenerate = "if upstream changed it, regenerate the file after the merge \
                      (see `regenerate_upstream_numbers`)";
    let mut problems = Vec::new();
    for (key, file) in &current {
        if !recorded.contains(key) {
            let (scope, name, number) = key;
            problems.push(format!(
                "{scope}.{name} = {number} ({file}) is not upstream's: a field or value the fork \
                 adds to an upstream message takes a number in {FORK_BAND:?} (and a line in \
                 FORK_FIELDS_IN_UPSTREAM_MESSAGES / FORK_VALUES_IN_UPSTREAM_ENUMS), a message the \
                 fork adds belongs in {FORK_FILES:?}; {regenerate}"
            ));
        }
    }
    for key in &recorded {
        if !current.contains_key(key) {
            let (scope, name, number) = key;
            problems.push(format!(
                "upstream's {scope}.{name} = {number} is missing or renumbered; {regenerate}"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Rewrites [`UPSTREAM_NUMBERS_FILE`] from an upstream release's own
/// `crates/proto/proto`, never from this tree, e.g.:
///
/// ```text
/// mkdir upstream-proto
/// git archive refs/upstream/tags/v1.21.0 crates/proto/proto | tar -x -C upstream-proto
/// JP_ENHANCE_UPSTREAM_PROTO_DIR=upstream-proto/crates/proto/proto \
/// JP_ENHANCE_UPSTREAM_LABEL="v1.21.0 (33c9585)" \
///   cargo test -p proto --test jp_enhance_numbering -- --ignored regenerate_upstream_numbers
/// ```
#[test]
#[ignore = "rewrites the upstream numbers file from JP_ENHANCE_UPSTREAM_PROTO_DIR"]
fn regenerate_upstream_numbers() {
    let directory = PathBuf::from(
        std::env::var("JP_ENHANCE_UPSTREAM_PROTO_DIR")
            .expect("JP_ENHANCE_UPSTREAM_PROTO_DIR names upstream's crates/proto/proto"),
    );
    let label = std::env::var("JP_ENHANCE_UPSTREAM_LABEL")
        .expect("JP_ENHANCE_UPSTREAM_LABEL names the upstream release, e.g. `v1.21.0 (33c9585)`");
    for fork_file in FORK_FILES {
        assert!(
            !directory.join(fork_file).exists(),
            "{} holds {fork_file}: that is this fork's tree, not upstream's",
            directory.display()
        );
    }
    let protocol = parse_protocol_in(&directory);
    let mut keys = protocol
        .numbered
        .iter()
        .map(|numbered| {
            assert!(
                numbered.number < FORK_BAND.start,
                "{}.{} = {} in {}: upstream never numbers this high; is this the fork's tree?",
                numbered.scope,
                numbered.name,
                numbered.number,
                numbered.file
            );
            numbered_key(numbered)
        })
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();
    let mut text = format!(
        "# Every numbered field and enum value in upstream's crates/proto/proto at\n\
         # {label}, as `scope name number`. Written by `regenerate_upstream_numbers`\n\
         # in jp_enhance_numbering.rs; do not edit by hand.\n"
    );
    for (scope, name, number) in keys {
        text.push_str(&format!("{scope} {name} {number}\n"));
    }
    std::fs::write(upstream_numbers_path(), text).expect("writing the upstream numbers file");
}

#[test]
fn no_number_falls_in_the_range_protobuf_reserves() {
    let protocol = parse_protocol();
    let problems = protocol
        .numbered
        .iter()
        .filter(|numbered| matches!(numbered.item, Item::Field { .. }))
        .filter(|numbered| (19_000..=19_999).contains(&numbered.number) || numbered.number == 0)
        .map(|numbered| format!("{}.{} = {}", numbered.scope, numbered.name, numbered.number))
        .collect::<Vec<_>>();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

fn first_field_number(bytes: &[u8]) -> u64 {
    let mut tag = 0u64;
    for (index, byte) in bytes.iter().enumerate() {
        tag |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return tag >> 3;
        }
    }
    panic!("no complete tag in {bytes:?}");
}

fn envelope_number_in_proto(field_name: &str) -> u64 {
    let protocol = parse_protocol();
    protocol
        .numbered
        .iter()
        .find(|numbered| numbered.scope == "Envelope" && numbered.name == field_name)
        .map(|numbered| u64::from(numbered.number))
        .unwrap_or_else(|| panic!("Envelope has no field {field_name}"))
}

#[test]
fn generated_code_puts_fork_messages_on_the_wire_under_their_fork_numbers() {
    // Proves the parser reads what the build compiled: an Envelope with only a
    // payload set encodes that payload's tag first.
    let cases = [
        (
            "set_egress_allowlist",
            envelope::Payload::SetEgressAllowlist(SetEgressAllowlist::default()),
        ),
        (
            "kill_terminal_session",
            envelope::Payload::KillTerminalSession(KillTerminalSession::default()),
        ),
        (
            "get_clangd_memory_usage",
            envelope::Payload::GetClangdMemoryUsage(proto::GetClangdMemoryUsage::default()),
        ),
        (
            "clangd_memory_limit_exceeded",
            envelope::Payload::ClangdMemoryLimitExceeded(
                proto::ClangdMemoryLimitExceeded::default(),
            ),
        ),
        (
            "get_symbol_index_status",
            envelope::Payload::GetSymbolIndexStatus(proto::GetSymbolIndexStatus::default()),
        ),
        (
            "rebuild_symbol_index",
            envelope::Payload::RebuildSymbolIndex(proto::RebuildSymbolIndex::default()),
        ),
        (
            "symbol_index_describe_type",
            envelope::Payload::SymbolIndexDescribeType(proto::SymbolIndexDescribeType::default()),
        ),
        (
            "symbol_index_type_tree_response",
            envelope::Payload::SymbolIndexTypeTreeResponse(
                proto::SymbolIndexTypeTreeResponse::default(),
            ),
        ),
    ];
    for (field_name, payload) in cases {
        let bytes = Envelope {
            payload: Some(payload),
            ..Default::default()
        }
        .encode_to_vec();
        let on_the_wire = first_field_number(&bytes);
        assert_eq!(on_the_wire, envelope_number_in_proto(field_name));
        assert!(
            FORK_BAND.contains(&(on_the_wire as u32)),
            "{field_name} = {on_the_wire}"
        );
    }
}

#[test]
fn retired_fork_numbers_stay_reserved_and_unused() {
    let protocol = parse_protocol();
    let mut problems = Vec::new();
    for (scope, number) in RETIRED_FORK_NUMBERS {
        if !protocol
            .reserved
            .get(*scope)
            .is_some_and(|numbers| numbers.contains(number))
        {
            problems.push(format!(
                "{scope} does not declare `reserved {number};`, so nothing stops it being reused"
            ));
        }
        for numbered in protocol
            .numbered
            .iter()
            .filter(|numbered| numbered.scope == *scope && numbered.number == *number)
        {
            problems.push(format!(
                "{scope}.{} = {number} ({}): a retired fork number, which a peer of the old \
                 revision reads as the field it used to be",
                numbered.name, numbered.file
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn a_wire_revision_check_is_on_the_wire_under_its_fork_number() {
    let bytes = Envelope {
        payload: Some(envelope::Payload::CheckWireRevision(
            proto::CheckWireRevision {
                revision: 1,
                display_language: None,
            },
        )),
        ..Default::default()
    }
    .encode_to_vec();
    let on_the_wire = first_field_number(&bytes);
    assert_eq!(on_the_wire, envelope_number_in_proto("check_wire_revision"));
    assert!(FORK_BAND.contains(&(on_the_wire as u32)));
}

/// Every revision to come asks and answers under these numbers, or a server
/// of one revision cannot tell a client of another what it speaks: renumber
/// anything else, never these.
#[test]
fn the_wire_revision_check_keeps_its_numbers_across_revisions() {
    assert_eq!(envelope_number_in_proto("check_wire_revision"), 10033);
    assert_eq!(
        envelope_number_in_proto("check_wire_revision_response"),
        10034
    );
    for (message, bytes) in [
        (
            "CheckWireRevision",
            proto::CheckWireRevision {
                revision: 7,
                display_language: None,
            }
            .encode_to_vec(),
        ),
        (
            "CheckWireRevisionResponse",
            proto::CheckWireRevisionResponse { revision: 7 }.encode_to_vec(),
        ),
    ] {
        assert_eq!(bytes, [0x08, 7], "{message}.revision must stay field 1");
    }
}

/// The display language rides on the check without a new revision, so both
/// directions have to keep working: a server from before the field reads a
/// check that carries it, and a server with the field reads a check from a
/// client without it as carrying none.
#[test]
fn the_display_language_is_optional_on_the_wire() {
    let with_language = proto::CheckWireRevision {
        revision: 7,
        display_language: Some("ja".to_string()),
    }
    .encode_to_vec();
    assert_eq!(&with_language[..2], [0x08, 7]);
    // `CheckWireRevisionResponse` has the same field 1 and nothing else,
    // which is what the check was to a server from before the field.
    let read_by_an_older_server =
        proto::CheckWireRevisionResponse::decode(with_language.as_slice())
            .expect("an unknown field is skipped");
    assert_eq!(read_by_an_older_server.revision, 7);

    let from_an_older_client = proto::CheckWireRevision::decode([0x08, 7].as_slice())
        .expect("a check without the field decodes");
    assert_eq!(from_an_older_client.revision, 7);
    assert_eq!(from_an_older_client.display_language, None);

    let english = proto::CheckWireRevision {
        revision: 7,
        display_language: Some("en".to_string()),
    };
    let decoded = proto::CheckWireRevision::decode(english.encode_to_vec().as_slice())
        .expect("the check round-trips");
    assert_eq!(
        decoded.display_language.as_deref(),
        Some("en"),
        "an explicit English is not the same as no language"
    );
}
