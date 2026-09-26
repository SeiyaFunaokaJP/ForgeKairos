//! A round-trippable editing model for Mermaid sequence diagrams.
//!
//! The shape mirrors [`crate::flowchart_model`]: [`parse_sequence`] borrows
//! `merman`'s parser and copies the parts of its sequence model that map onto
//! source constructs, and [`emit_sequence`] writes them back.
//!
//! Two properties matter more than fidelity to the original spelling:
//!
//! - [`emit_sequence`] is idempotent from its second application. Parsing its
//!   own output and emitting again produces the identical string, so opening
//!   and confirming the editor twice does not keep rewriting the fence.
//! - Everything `merman`'s model does not carry — comments, `%%{init}%%`
//!   directives, `accTitle` / `accDescr`, `links` / `link` / `properties` /
//!   `details` — survives verbatim in [`SequenceDocument::preserved_lines`].
//!
//! Deliberate re-spellings, none of which change what Mermaid draws:
//!
//! - Every participant gets an explicit `participant` / `actor` declaration at
//!   the top, in the order the diagram used them. The column order of a
//!   sequence diagram is the order participants are first mentioned, and
//!   `merman` reports that order but not which mentions were declarations, so
//!   declaring all of them is the only way to keep the order stable.
//! - `A ->> B: x` followed by `deactivate A` is written back as `A ->>- B: x`.
//!   Mermaid turns both into the same pair of model entries.
//!
//! - A `create`d participant keeps its declaration where the `create`
//!   statement stood, because that position is what decides when its column
//!   starts. It is the one participant that is not re-declared at the top.
//!
//! - A participant config object (`participant A@{ "type": "boundary" }`) is
//!   carried as the text between its braces, because `merman` keeps only the
//!   `type` it reads out of it.
//! - An id spelled with the `#…;` shorthand keeps that spelling. It is written
//!   back verbatim, so it is never decoded the way a text is.
//!
//! Sources the parser refuses rather than rewrite, because writing them back
//! would drop something: config objects that span several lines, the
//! central-connection spellings (`A ->>() B`) and end-of-line comments.

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use merman::diagrams::sequence::{SequenceDiagramRenderModel, SequenceMessagePayload};
use merman::{Engine, ParseOptions};

use crate::flowchart_model::{PreservedLine, PreservedPosition};

// Mermaid's `SequenceDB.LINETYPE` numbering. `merman` stores it in
// `SequenceMessage::message_type` but keeps the constants private, so they are
// repeated here.
const LINETYPE_SOLID: i32 = 0;
const LINETYPE_DOTTED: i32 = 1;
const LINETYPE_NOTE: i32 = 2;
const LINETYPE_SOLID_CROSS: i32 = 3;
const LINETYPE_DOTTED_CROSS: i32 = 4;
const LINETYPE_SOLID_OPEN: i32 = 5;
const LINETYPE_DOTTED_OPEN: i32 = 6;
const LINETYPE_LOOP_START: i32 = 10;
const LINETYPE_LOOP_END: i32 = 11;
const LINETYPE_ALT_START: i32 = 12;
const LINETYPE_ALT_ELSE: i32 = 13;
const LINETYPE_ALT_END: i32 = 14;
const LINETYPE_OPT_START: i32 = 15;
const LINETYPE_OPT_END: i32 = 16;
const LINETYPE_ACTIVE_START: i32 = 17;
const LINETYPE_ACTIVE_END: i32 = 18;
const LINETYPE_PAR_START: i32 = 19;
const LINETYPE_PAR_AND: i32 = 20;
const LINETYPE_PAR_END: i32 = 21;
const LINETYPE_RECT_START: i32 = 22;
const LINETYPE_RECT_END: i32 = 23;
const LINETYPE_SOLID_POINT: i32 = 24;
const LINETYPE_DOTTED_POINT: i32 = 25;
const LINETYPE_AUTONUMBER: i32 = 26;
const LINETYPE_CRITICAL_START: i32 = 27;
const LINETYPE_CRITICAL_OPTION: i32 = 28;
const LINETYPE_CRITICAL_END: i32 = 29;
const LINETYPE_BREAK_START: i32 = 30;
const LINETYPE_BREAK_END: i32 = 31;
const LINETYPE_PAR_OVER_START: i32 = 32;
const LINETYPE_BIDIRECTIONAL_SOLID: i32 = 33;
const LINETYPE_BIDIRECTIONAL_DOTTED: i32 = 34;

const PLACEMENT_LEFT_OF: i32 = 0;
const PLACEMENT_RIGHT_OF: i32 = 1;
const PLACEMENT_OVER: i32 = 2;

const HEADER: &str = "sequenceDiagram";
const INDENT: &str = "    ";

/// The arrow of a message, i.e. the operator between the two participants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceArrow {
    /// `->>`
    Solid,
    /// `-->>`
    Dotted,
    /// `->`
    SolidOpen,
    /// `-->`
    DottedOpen,
    /// `-x`
    SolidCross,
    /// `--x`
    DottedCross,
    /// `-)`
    SolidPoint,
    /// `--)`
    DottedPoint,
    /// `<<->>`
    BidirectionalSolid,
    /// `<<-->>`
    BidirectionalDotted,
}

impl SequenceArrow {
    /// Every arrow, in the order the editor's arrow menu lists them.
    pub const ALL: [SequenceArrow; 10] = [
        SequenceArrow::Solid,
        SequenceArrow::Dotted,
        SequenceArrow::SolidOpen,
        SequenceArrow::DottedOpen,
        SequenceArrow::SolidCross,
        SequenceArrow::DottedCross,
        SequenceArrow::SolidPoint,
        SequenceArrow::DottedPoint,
        SequenceArrow::BidirectionalSolid,
        SequenceArrow::BidirectionalDotted,
    ];

    pub fn operator(self) -> &'static str {
        match self {
            Self::Solid => "->>",
            Self::Dotted => "-->>",
            Self::SolidOpen => "->",
            Self::DottedOpen => "-->",
            Self::SolidCross => "-x",
            Self::DottedCross => "--x",
            Self::SolidPoint => "-)",
            Self::DottedPoint => "--)",
            Self::BidirectionalSolid => "<<->>",
            Self::BidirectionalDotted => "<<-->>",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Solid => "実線の矢印 (呼び出し)",
            Self::Dotted => "点線の矢印 (応答)",
            Self::SolidOpen => "実線の線",
            Self::DottedOpen => "点線の線",
            Self::SolidCross => "実線の×印 (失敗)",
            Self::DottedCross => "点線の×印",
            Self::SolidPoint => "実線の非同期",
            Self::DottedPoint => "点線の非同期",
            Self::BidirectionalSolid => "実線の両向き",
            Self::BidirectionalDotted => "点線の両向き",
        }
    }

    fn from_model(message_type: i32) -> Option<Self> {
        match message_type {
            LINETYPE_SOLID => Some(Self::Solid),
            LINETYPE_DOTTED => Some(Self::Dotted),
            LINETYPE_SOLID_OPEN => Some(Self::SolidOpen),
            LINETYPE_DOTTED_OPEN => Some(Self::DottedOpen),
            LINETYPE_SOLID_CROSS => Some(Self::SolidCross),
            LINETYPE_DOTTED_CROSS => Some(Self::DottedCross),
            LINETYPE_SOLID_POINT => Some(Self::SolidPoint),
            LINETYPE_DOTTED_POINT => Some(Self::DottedPoint),
            LINETYPE_BIDIRECTIONAL_SOLID => Some(Self::BidirectionalSolid),
            LINETYPE_BIDIRECTIONAL_DOTTED => Some(Self::BidirectionalDotted),
            _ => None,
        }
    }
}

/// Which of the two declaration keywords a participant was written with.
/// `actor` draws a stick figure instead of a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceParticipantKind {
    Participant,
    Actor,
}

impl SequenceParticipantKind {
    pub const ALL: [SequenceParticipantKind; 2] = [
        SequenceParticipantKind::Participant,
        SequenceParticipantKind::Actor,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Participant => "participant",
            Self::Actor => "actor",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Participant => "箱 (participant)",
            Self::Actor => "人型 (actor)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceParticipant {
    pub id: String,
    /// The `as` description, when it differs from the id.
    pub alias: Option<String>,
    pub kind: SequenceParticipantKind,
    /// Whether the description carried Mermaid's `wrap:` prefix. The editor
    /// does not offer this, but dropping it would change how the label breaks.
    pub wrap: bool,
    /// Index into [`SequenceDocument::boxes`] of the `box` this participant is
    /// declared in.
    pub box_index: Option<usize>,
    /// The text between the braces of a `participant A@{ … }` config object,
    /// as the source spelled it. Mermaid only accepts the object on the
    /// `participant` keyword and without an `as` description.
    pub preserved_settings: Option<String>,
}

/// A `box` that groups neighbouring participants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceGroupBox {
    pub name: Option<String>,
    /// The CSS color written before the name; `None` is Mermaid's
    /// `transparent`.
    pub fill: Option<String>,
    pub wrap: bool,
}

/// The fills the editor offers for a `box`, as `(value, label)`.
///
/// Mermaid reads the first word of a `box` header as the group's color when it
/// is a CSS color keyword, so a free-text fill and a free-text name share one
/// ambiguous slot. Offering a fixed list of keywords keeps the two apart: the
/// fill is always a word [`header_starts_with_color`] knows, so a name that
/// happens to start with a color word still round-trips through the
/// `transparent` [`box_header`] writes in front of it.
pub const BOX_FILL_PRESETS: [(Option<&str>, &str); 7] = [
    (None, "なし (透明)"),
    (Some("lightblue"), "薄い青"),
    (Some("lightgreen"), "薄い緑"),
    (Some("lightyellow"), "薄い黄"),
    (Some("lightpink"), "薄い桃"),
    (Some("lavender"), "藤色"),
    (Some("gainsboro"), "薄い灰"),
];

/// What the fill menu shows for `fill`, which is the preset's label for a
/// color the editor offers and the color itself for one written by hand.
pub fn box_fill_label(fill: Option<&str>) -> String {
    BOX_FILL_PRESETS
        .iter()
        .find(|(value, _)| *value == fill)
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| fill.unwrap_or_default().to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceNotePlacement {
    LeftOf,
    RightOf,
    Over,
}

impl SequenceNotePlacement {
    pub const ALL: [SequenceNotePlacement; 3] = [
        SequenceNotePlacement::LeftOf,
        SequenceNotePlacement::RightOf,
        SequenceNotePlacement::Over,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::LeftOf => "左側",
            Self::RightOf => "右側",
            Self::Over => "重ねる",
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            Self::LeftOf => "left of",
            Self::RightOf => "right of",
            Self::Over => "over",
        }
    }

    fn from_model(placement: i32) -> Option<Self> {
        match placement {
            PLACEMENT_LEFT_OF => Some(Self::LeftOf),
            PLACEMENT_RIGHT_OF => Some(Self::RightOf),
            PLACEMENT_OVER => Some(Self::Over),
            _ => None,
        }
    }
}

/// A block that wraps a run of items and is closed with `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceBlockKind {
    Loop,
    Alt,
    Opt,
    Par,
    ParOver,
    Critical,
    Break,
    Rect,
}

impl SequenceBlockKind {
    pub const ALL: [SequenceBlockKind; 8] = [
        SequenceBlockKind::Loop,
        SequenceBlockKind::Alt,
        SequenceBlockKind::Opt,
        SequenceBlockKind::Par,
        SequenceBlockKind::ParOver,
        SequenceBlockKind::Critical,
        SequenceBlockKind::Break,
        SequenceBlockKind::Rect,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Loop => "loop",
            Self::Alt => "alt",
            Self::Opt => "opt",
            Self::Par => "par",
            Self::ParOver => "par_over",
            Self::Critical => "critical",
            Self::Break => "break",
            Self::Rect => "rect",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Loop => "繰り返し (loop)",
            Self::Alt => "分岐 (alt)",
            Self::Opt => "任意 (opt)",
            Self::Par => "並行 (par)",
            Self::ParOver => "重ねる並行 (par_over)",
            Self::Critical => "critical",
            Self::Break => "中断 (break)",
            Self::Rect => "背景 (rect)",
        }
    }

    /// The branch this block accepts, if any.
    pub fn branch(self) -> Option<SequenceBranchKind> {
        match self {
            Self::Alt => Some(SequenceBranchKind::Else),
            Self::Par => Some(SequenceBranchKind::And),
            Self::Critical => Some(SequenceBranchKind::Option),
            _ => None,
        }
    }

    fn start_from_model(message_type: i32) -> Option<Self> {
        match message_type {
            LINETYPE_LOOP_START => Some(Self::Loop),
            LINETYPE_ALT_START => Some(Self::Alt),
            LINETYPE_OPT_START => Some(Self::Opt),
            LINETYPE_PAR_START => Some(Self::Par),
            LINETYPE_PAR_OVER_START => Some(Self::ParOver),
            LINETYPE_CRITICAL_START => Some(Self::Critical),
            LINETYPE_BREAK_START => Some(Self::Break),
            LINETYPE_RECT_START => Some(Self::Rect),
            _ => None,
        }
    }

    fn is_end(message_type: i32) -> bool {
        matches!(
            message_type,
            LINETYPE_LOOP_END
                | LINETYPE_ALT_END
                | LINETYPE_OPT_END
                | LINETYPE_PAR_END
                | LINETYPE_CRITICAL_END
                | LINETYPE_BREAK_END
                | LINETYPE_RECT_END
        )
    }
}

/// The middle keyword of a block that has several branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceBranchKind {
    /// `else`, inside `alt`.
    Else,
    /// `and`, inside `par`.
    And,
    /// `option`, inside `critical`.
    Option,
}

impl SequenceBranchKind {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Else => "else",
            Self::And => "and",
            Self::Option => "option",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Else => "それ以外 (else)",
            Self::And => "同時に (and)",
            Self::Option => "選択肢 (option)",
        }
    }

    fn from_model(message_type: i32) -> Option<Self> {
        match message_type {
            LINETYPE_ALT_ELSE => Some(Self::Else),
            LINETYPE_PAR_AND => Some(Self::And),
            LINETYPE_CRITICAL_OPTION => Some(Self::Option),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceMessage {
    pub from: String,
    pub to: String,
    pub arrow: SequenceArrow,
    /// The `+` suffix: activates `to`.
    pub activate: bool,
    /// The `-` suffix: deactivates `from`.
    pub deactivate: bool,
    pub text: String,
    pub wrap: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceNote {
    pub placement: SequenceNotePlacement,
    /// The participant the note is placed at, or the left one of the pair a
    /// `Note over A,B` spans.
    pub from: String,
    /// The right participant of a spanning note; equal to `from` otherwise.
    pub to: String,
    pub text: String,
    pub wrap: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceBlock {
    pub kind: SequenceBlockKind,
    pub label: String,
    pub wrap: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceBranch {
    pub kind: SequenceBranchKind,
    pub label: String,
    pub wrap: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SequenceAutonumber {
    pub start: Option<f64>,
    pub step: Option<f64>,
    pub visible: bool,
}

/// One row of the editor's item list. The order of the list is the order of
/// the diagram, which is why blocks and notes live in it rather than beside it.
#[derive(Debug, Clone, PartialEq)]
pub enum SequenceItem {
    Message(SequenceMessage),
    Note(SequenceNote),
    BlockStart(SequenceBlock),
    BlockBranch(SequenceBranch),
    BlockEnd,
    /// `activate <participant>`
    Activate(String),
    /// `deactivate <participant>`
    Deactivate(String),
    /// `create participant <participant>`. The declaration itself is written
    /// here rather than at the top of the diagram, because where the statement
    /// sits is what decides when the column appears.
    Create(String),
    /// `destroy <participant>`
    Destroy(String),
    Autonumber(SequenceAutonumber),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SequenceDocument {
    pub title: Option<String>,
    /// Declaration order, which is also the left-to-right column order.
    pub participants: Vec<SequenceParticipant>,
    pub boxes: Vec<SequenceGroupBox>,
    pub items: Vec<SequenceItem>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl SequenceDocument {
    /// A blank diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            title: None,
            participants: Vec::new(),
            boxes: Vec::new(),
            items: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    pub fn participant_index(&self, id: &str) -> Option<usize> {
        self.participants
            .iter()
            .position(|participant| participant.id == id)
    }

    /// Whether `id` is declared by a `create` statement in the item list
    /// rather than with the other participants at the top.
    pub fn is_created(&self, id: &str) -> bool {
        self.created_ids().contains(&id)
    }

    /// The participants a `create` statement declares, in the order the
    /// statements appear.
    fn created_ids(&self) -> Vec<&str> {
        self.items
            .iter()
            .filter_map(|item| match item {
                SequenceItem::Create(id) => Some(id.as_str()),
                _ => None,
            })
            .collect()
    }

    /// An id no participant is using, based on `prefix`.
    pub fn unused_participant_id(&self, prefix: &str) -> String {
        if self.participant_index(prefix).is_none() {
            return prefix.to_string();
        }
        for suffix in 1..usize::MAX {
            let candidate = format!("{prefix}{suffix}");
            if self.participant_index(&candidate).is_none() {
                return candidate;
            }
        }
        prefix.to_string()
    }

    /// Why the document could not be written back as a diagram Mermaid accepts,
    /// if there is a reason. The editor calls this before confirming an edit;
    /// [`parse_sequence`] never produces a document that fails it.
    pub fn structure_error(&self) -> Option<String> {
        let mut blocks: Vec<SequenceBlockKind> = Vec::new();
        let mut activations: HashMap<&str, i64> = HashMap::new();

        let created = self.created_ids();
        for (index, id) in created.iter().enumerate() {
            if created.get(..index).unwrap_or_default().contains(id) {
                return Some(format!("参加者 `{id}` が二重に `create` されています。"));
            }
        }
        // `merman` keeps one position per destroyed column, so the second
        // statement would be all that is left once the fence is reopened.
        let mut destroyed: Vec<&str> = Vec::new();
        for item in &self.items {
            if let SequenceItem::Destroy(id) = item {
                if destroyed.contains(&id.as_str()) {
                    return Some(format!("参加者 `{id}` が二重に `destroy` されています。"));
                }
                destroyed.push(id.as_str());
            }
        }

        for (index, participant) in self.participants.iter().enumerate() {
            if !is_writable_participant_id(&participant.id) {
                return Some(format!(
                    "`{}` は参加者の id として使えません。",
                    participant.id
                ));
            }
            if self
                .participants
                .get(..index)
                .unwrap_or_default()
                .iter()
                .any(|earlier| earlier.id == participant.id)
            {
                return Some(format!(
                    "参加者 `{}` が二重に宣言されています。",
                    participant.id
                ));
            }
            if let Some(settings) = &participant.preserved_settings {
                if settings.contains(['}', '\n', '\r']) {
                    return Some(format!(
                        "参加者 `{}` の設定オブジェクトは 1 行で、`}}` を含まない形でなければなりません。",
                        participant.id
                    ));
                }
                if participant.kind != SequenceParticipantKind::Participant {
                    return Some(format!(
                        "設定オブジェクト (`@{{ … }}`) を持つ参加者 `{}` は箱 (participant) でなければなりません。",
                        participant.id
                    ));
                }
                if participant.alias.is_some() {
                    return Some(format!(
                        "設定オブジェクト (`@{{ … }}`) を持つ参加者 `{}` には別名 (as) を付けられません。",
                        participant.id
                    ));
                }
            }
        }
        // A `create`d participant is declared where its statement stands, so
        // the top of the diagram can only reproduce the column order when
        // every other participant is already declared by then.
        let mut seen_created = false;
        for participant in &self.participants {
            if created.contains(&participant.id.as_str()) {
                seen_created = true;
                if participant.box_index.is_some() {
                    return Some(format!(
                        "`create` で作られる参加者 `{}` は box に入れられません。",
                        participant.id
                    ));
                }
            } else if seen_created {
                return Some(format!(
                    "`create` で作られる参加者より後ろに `{}` を並べることはできません。",
                    participant.id
                ));
            }
        }
        let declared_created = self
            .participants
            .iter()
            .map(|participant| participant.id.as_str())
            .filter(|id| created.contains(id))
            .collect::<Vec<_>>();
        if declared_created != created {
            return Some(
                "`create` の順番と参加者の並び順が食い違っています。`create` した順に並べてください。"
                    .to_string(),
            );
        }

        // `box … end` wraps a run of declarations, so a box whose participants
        // are not neighbours cannot be written back without moving a column.
        let mut closed_boxes: Vec<usize> = Vec::new();
        let mut previous_box: Option<usize> = None;
        for participant in &self.participants {
            if participant.box_index == previous_box {
                continue;
            }
            if let Some(box_index) = previous_box {
                closed_boxes.push(box_index);
            }
            if let Some(box_index) = participant.box_index {
                if closed_boxes.contains(&box_index) {
                    return Some(
                        "同じ box の参加者は続けて並んでいなければなりません。".to_string(),
                    );
                }
            }
            previous_box = participant.box_index;
        }

        // Mermaid refuses a `create` for a column that already exists, so a
        // statement that names the participant first would make the diagram
        // unopenable rather than merely misordered.
        let mut used: Vec<&str> = Vec::new();
        // Mermaid checks that the message right after a `create` / `destroy` is
        // the one that draws the column's start or its cross, and refuses the
        // whole diagram when it is not. The editor has to say so before it
        // writes a fence its own preview could only report an error about.
        let mut last_created: Option<&str> = None;
        let mut last_destroyed: Option<&str> = None;
        for item in &self.items {
            match item {
                SequenceItem::Message(message) => {
                    if let Some(created) = last_created.take() {
                        if message.to != created {
                            return Some(format!(
                                "`create {created}` の次のメッセージは `{created}` 宛てでなければなりません。"
                            ));
                        }
                    } else if let Some(destroyed) = last_destroyed.take() {
                        if message.from != destroyed && message.to != destroyed {
                            return Some(format!(
                                "`destroy {destroyed}` の次のメッセージは `{destroyed}` が関わるものでなければなりません。"
                            ));
                        }
                    }
                    for id in [&message.from, &message.to] {
                        if self.participant_index(id).is_none() {
                            return Some(format!("参加者 `{id}` が宣言されていません。"));
                        }
                        used.push(id.as_str());
                    }
                    if message.activate {
                        *activations.entry(message.to.as_str()).or_default() += 1;
                    }
                    if message.deactivate {
                        let count = activations.entry(message.from.as_str()).or_default();
                        if *count < 1 {
                            return Some(format!(
                                "`{}` は活性化していないので終了できません。",
                                message.from
                            ));
                        }
                        *count -= 1;
                    }
                }
                SequenceItem::Note(note) => {
                    for id in [&note.from, &note.to] {
                        if self.participant_index(id).is_none() {
                            return Some(format!("参加者 `{id}` が宣言されていません。"));
                        }
                        used.push(id.as_str());
                    }
                }
                SequenceItem::Create(id) => {
                    if self.participant_index(id).is_none() {
                        return Some(format!("参加者 `{id}` が宣言されていません。"));
                    }
                    if used.contains(&id.as_str()) {
                        return Some(format!(
                            "`{id}` は `create` より前に登場しています。`create` を先に置いてください。"
                        ));
                    }
                    used.push(id.as_str());
                    last_created = Some(id.as_str());
                }
                SequenceItem::Destroy(id) => {
                    if self.participant_index(id).is_none() {
                        return Some(format!("参加者 `{id}` が宣言されていません。"));
                    }
                    used.push(id.as_str());
                    last_destroyed = Some(id.as_str());
                }
                SequenceItem::Activate(id) => {
                    if self.participant_index(id).is_none() {
                        return Some(format!("参加者 `{id}` が宣言されていません。"));
                    }
                    used.push(id.as_str());
                    *activations.entry(id.as_str()).or_default() += 1;
                }
                SequenceItem::Deactivate(id) => {
                    if self.participant_index(id).is_none() {
                        return Some(format!("参加者 `{id}` が宣言されていません。"));
                    }
                    used.push(id.as_str());
                    let count = activations.entry(id.as_str()).or_default();
                    if *count < 1 {
                        return Some(format!("`{id}` は活性化していないので終了できません。"));
                    }
                    *count -= 1;
                }
                SequenceItem::BlockStart(block) => blocks.push(block.kind),
                SequenceItem::BlockBranch(branch) => {
                    let Some(kind) = blocks.last().copied() else {
                        return Some(format!(
                            "`{}` を囲むブロックがありません。",
                            branch.kind.keyword()
                        ));
                    };
                    if kind.branch() != Some(branch.kind) {
                        return Some(format!(
                            "`{}` は `{}` の中では使えません。",
                            branch.kind.keyword(),
                            kind.keyword()
                        ));
                    }
                }
                SequenceItem::BlockEnd => {
                    if blocks.pop().is_none() {
                        return Some("`end` が対応するブロックより多くあります。".to_string());
                    }
                }
                SequenceItem::Autonumber(_) => {}
            }
        }

        if let Some(kind) = blocks.last() {
            return Some(format!(
                "`{}` ブロックが `end` で閉じられていません。",
                kind.keyword()
            ));
        }
        for participant in &self.participants {
            if let Some(box_index) = participant.box_index {
                if self.boxes.get(box_index).is_none() {
                    return Some(format!(
                        "参加者 `{}` が存在しない box を指しています。",
                        participant.id
                    ));
                }
            }
        }
        None
    }

    /// The column order after moving the participant at `index` one step, as
    /// the old index of each new position, or `None` when that step would
    /// leave a diagram [`Self::structure_error`] refuses.
    ///
    /// A member of a `box` only moves among the other members, because the
    /// box is one run of declarations and leaving it is what the group menu
    /// is for. A column in no box steps over a whole box at once rather than
    /// into the middle of it. A `create`d column is placed by its statement,
    /// so it does not move and nothing moves past it.
    pub fn participant_move_order(&self, index: usize, forward: bool) -> Option<Vec<usize>> {
        let participant = self.participants.get(index)?;
        let other = if forward {
            index.checked_add(1)?
        } else {
            index.checked_sub(1)?
        };
        let neighbour = self.participants.get(other)?;
        if self.is_created(&participant.id) || self.is_created(&neighbour.id) {
            return None;
        }

        let mut order = (0..self.participants.len()).collect::<Vec<_>>();
        match (participant.box_index, neighbour.box_index) {
            (Some(own), Some(theirs)) if own == theirs => {
                order.swap(index, other);
            }
            (Some(_), _) => return None,
            (None, None) => order.swap(index, other),
            (None, Some(group)) => {
                let in_group = |position: usize| {
                    self.participants.get(position).is_some_and(|participant| {
                        participant.box_index == Some(group) && !self.is_created(&participant.id)
                    })
                };
                let mut far = other;
                if forward {
                    while in_group(far + 1) {
                        far += 1;
                    }
                } else {
                    while far > 0 && in_group(far - 1) {
                        far -= 1;
                    }
                }
                let moved = order.remove(index);
                // Taking the row out first shifts every later index down by
                // one, which is why a row moving forward lands on `far` itself.
                order.insert(far, moved);
            }
        }
        Some(order)
    }

    /// The column order and the `box` order after moving the box at
    /// `box_index` one step through [`Self::boxes`], or `None` when there is
    /// nothing it can trade places with.
    ///
    /// Two boxes that hold columns trade their whole runs of declarations,
    /// so the list of boxes stays in the order the boxes are drawn. The
    /// columns in no box stay where they are. A box with no member is drawn
    /// after every other box, so it only trades places with another empty one.
    pub fn box_move_order(
        &self,
        box_index: usize,
        forward: bool,
    ) -> Option<(Vec<usize>, Vec<usize>)> {
        self.boxes.get(box_index)?;
        let other = if forward {
            box_index.checked_add(1)?
        } else {
            box_index.checked_sub(1)?
        };
        self.boxes.get(other)?;

        let mut box_order = (0..self.boxes.len()).collect::<Vec<_>>();
        box_order.swap(box_index, other);

        let runs = self.participant_runs();
        let own = runs.iter().position(|(group, _)| *group == Some(box_index));
        let theirs = runs.iter().position(|(group, _)| *group == Some(other));
        let is_empty = |group: usize| {
            !self
                .participants
                .iter()
                .any(|participant| participant.box_index == Some(group))
        };
        let (own, theirs) = match (own, theirs) {
            (Some(own), Some(theirs)) => (own, theirs),
            (None, None) if is_empty(box_index) && is_empty(other) => {
                return Some(((0..self.participants.len()).collect(), box_order));
            }
            _ => return None,
        };

        let mut runs = runs;
        runs.swap(own, theirs);
        let order = runs
            .into_iter()
            .flat_map(|(_, members)| members)
            .collect::<Vec<_>>();
        if order.len() != self.participants.len() {
            return None;
        }
        Some((order, box_order))
    }

    /// The participants cut into runs: each `box`'s members together, and
    /// every other column on its own. A box that is not one run — which
    /// [`Self::structure_error`] refuses — shows up as several runs.
    fn participant_runs(&self) -> Vec<(Option<usize>, Vec<usize>)> {
        let mut runs: Vec<(Option<usize>, Vec<usize>)> = Vec::new();
        for (index, participant) in self.participants.iter().enumerate() {
            match (participant.box_index, runs.last_mut()) {
                (Some(group), Some((Some(current), members))) if *current == group => {
                    members.push(index);
                }
                (group, _) => runs.push((group, vec![index])),
            }
        }
        runs
    }

    /// The order [`emit_sequence`] writes the boxes in, as the old index of
    /// each new position: the boxes that hold columns in column order, then
    /// the empty ones. Keeping [`Self::boxes`] in this order is what makes the
    /// editor's list of boxes the order the reopened fence will have.
    pub fn canonical_box_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = Vec::with_capacity(self.boxes.len());
        for participant in &self.participants {
            if let Some(group) = participant.box_index
                && group < self.boxes.len()
                && !order.contains(&group)
            {
                order.push(group);
            }
        }
        for group in 0..self.boxes.len() {
            if !order.contains(&group) {
                order.push(group);
            }
        }
        order
    }

    /// The column order that puts every `create`d column after the declared
    /// ones, in the order of their statements, and keeps everybody else where
    /// they were.
    pub fn created_last_order(&self) -> Vec<usize> {
        let created = self.created_ids();
        let mut order = self
            .participants
            .iter()
            .enumerate()
            .filter(|(_, participant)| !created.contains(&participant.id.as_str()))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        for id in created {
            if let Some(index) = self.participant_index(id)
                && !order.contains(&index)
            {
                order.push(index);
            }
        }
        order
    }

    /// Puts the participants in `order` (the old index of each new position).
    /// Returns whether `order` was a permutation and so was applied.
    pub fn reorder_participants(&mut self, order: &[usize]) -> bool {
        apply_order(&mut self.participants, order)
    }

    /// Puts the boxes in `order` and points every participant at its box's
    /// new position. Returns whether `order` was a permutation and so was
    /// applied.
    pub fn reorder_boxes(&mut self, order: &[usize]) -> bool {
        if !apply_order(&mut self.boxes, order) {
            return false;
        }
        for participant in &mut self.participants {
            participant.box_index = participant
                .box_index
                .and_then(|old| order.iter().position(|index| *index == old));
        }
        true
    }
}

/// Rearranges `items` so that position `n` holds what was at `order[n]`, when
/// `order` is a permutation of the positions; leaves `items` alone otherwise.
/// The editor keeps a row of fields beside each model entry, and moving both
/// with the same `order` is what keeps them paired.
pub fn apply_order<T>(items: &mut Vec<T>, order: &[usize]) -> bool {
    if order.len() != items.len() {
        return false;
    }
    let mut seen = vec![false; items.len()];
    for index in order {
        match seen.get_mut(*index) {
            Some(flag) if !*flag => *flag = true,
            _ => return false,
        }
    }
    let mut slots = std::mem::take(items)
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>();
    for index in order {
        if let Some(item) = slots.get_mut(*index).and_then(Option::take) {
            items.push(item);
        }
    }
    true
}

/// Whether `source` is a sequence diagram, i.e. whether the structured editor
/// can open it at all. Cheap enough to call before parsing.
pub fn is_sequence_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| header_rest(line).is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an
/// editable document.
pub fn parse_sequence(source: &str) -> Result<SequenceDocument> {
    let scan = scan_source(source)?;
    if let Some(problem) = unsupported_construct(&scan.statements) {
        bail!("{problem}");
    }
    let settings = declared_settings(&scan.statements)?;
    if scan.statements.is_empty() {
        return Ok(SequenceDocument {
            title: None,
            participants: Vec::new(),
            boxes: Vec::new(),
            items: Vec::new(),
            preserved_lines: scan.preserved,
        });
    }

    let engine = Engine::new();
    let parsed = engine
        .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
        .map_err(|error| anyhow!("{error}"))?
        .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
    let RenderSemanticModel::Sequence(model) = parsed.model else {
        bail!("この図はシーケンス図ではありません。");
    };

    let document = document_from_model(model, scan.preserved, &settings)?;
    // Keeps [`SequenceDocument::structure_error`]'s promise: a document the
    // editor cannot write back never reaches it in the first place.
    if let Some(problem) = document.structure_error() {
        bail!("{problem}テキストのまま編集してください。");
    }
    Ok(document)
}

fn document_from_model(
    model: SequenceDiagramRenderModel,
    preserved: Vec<PreservedLine>,
    settings: &HashMap<String, Option<String>>,
) -> Result<SequenceDocument> {
    let mut box_of_participant: HashMap<&str, usize> = HashMap::new();
    for (index, group) in model.boxes.iter().enumerate() {
        for id in &group.actor_keys {
            box_of_participant.entry(id.as_str()).or_insert(index);
        }
    }

    let mut participants = Vec::with_capacity(model.actor_order.len());
    for model_id in &model.actor_order {
        let actor = model
            .actors
            .get(model_id)
            .ok_or_else(|| anyhow!("参加者 `{model_id}` の情報が見つかりませんでした。"))?;
        let id = source_id(model_id);
        // `merman`'s placeholder characters can also be typed as themselves,
        // and those would come back as a `#…;` shorthand that is not in the
        // source. Only an id the shorthand spells exactly is kept.
        if encode_entity_shorthands(&id) != *model_id {
            bail!(
                "参加者の id `{model_id}` に Mermaid の内部文字が含まれているため、構造化編集に対応していません。テキストのまま編集してください。"
            );
        }
        let preserved_settings = settings.get(&id).cloned().flatten();
        // The config object's `type` is what `merman` reports as the kind,
        // and Mermaid only accepts the object after `participant`, so the
        // keyword is `participant` whatever the object says it draws.
        let kind = match (actor.actor_type.as_str(), &preserved_settings) {
            (_, Some(_)) | ("participant", None) => SequenceParticipantKind::Participant,
            ("actor", None) => SequenceParticipantKind::Actor,
            (other, None) => bail!(
                "参加者の種類 `{other}` は構造化編集に対応していません。テキストのまま編集してください。"
            ),
        };
        let description = decode_text(&actor.description);
        participants.push(SequenceParticipant {
            alias: (description != decode_text(model_id)).then_some(description),
            kind,
            wrap: actor.wrap,
            box_index: box_of_participant.get(model_id.as_str()).copied(),
            preserved_settings,
            id,
        });
    }

    let boxes = model
        .boxes
        .iter()
        .map(|group| SequenceGroupBox {
            name: group.name.as_deref().map(decode_text),
            fill: (group.fill != "transparent" && !group.fill.is_empty())
                .then(|| group.fill.clone()),
            wrap: group.wrap,
        })
        .collect::<Vec<_>>();

    let items = items_from_model(&model)?;

    Ok(SequenceDocument {
        title: model.title.as_deref().map(decode_text),
        participants,
        boxes,
        items,
        preserved_lines: preserved,
    })
}

/// `create` and `destroy` are the two statements `merman` reports beside the
/// message list rather than inside it, as "how many messages came before me".
/// Turning that back into rows means merging the two streams.
fn lifecycle_items(model: &SequenceDiagramRenderModel) -> Vec<(usize, SequenceItem)> {
    let mut lifecycle = model
        .created_actors
        .iter()
        .map(|(id, position)| (*position, SequenceItem::Create(source_id(id))))
        .chain(
            model
                .destroyed_actors
                .iter()
                .map(|(id, position)| (*position, SequenceItem::Destroy(source_id(id)))),
        )
        .collect::<Vec<_>>();
    lifecycle.sort_by_key(|(position, _)| *position);
    lifecycle
}

/// Moves every `create` / `destroy` that belongs before the message about to be
/// read into `items`.
fn push_lifecycle(
    items: &mut Vec<SequenceItem>,
    lifecycle: &[(usize, SequenceItem)],
    cursor: &mut usize,
    consumed: usize,
) {
    while let Some((position, item)) = lifecycle.get(*cursor) {
        if *position > consumed {
            return;
        }
        items.push(item.clone());
        *cursor += 1;
    }
}

fn items_from_model(model: &SequenceDiagramRenderModel) -> Result<Vec<SequenceItem>> {
    let messages = &model.messages;
    let lifecycle = lifecycle_items(model);
    let mut cursor = 0usize;
    let mut items = Vec::with_capacity(messages.len() + lifecycle.len());
    let mut index = 0;

    while let Some(message) = messages.get(index) {
        // `index` is the number of messages read so far, which is the position
        // `merman` records a `create` / `destroy` at.
        push_lifecycle(&mut items, &lifecycle, &mut cursor, index);
        index += 1;
        if message.central_connection != 0 {
            bail!(
                "`()` を使った中央接続は構造化編集に対応していません。テキストのまま編集してください。"
            );
        }

        let message_type = message.message_type;
        if let Some(arrow) = SequenceArrow::from_model(message_type) {
            let from = message
                .from
                .clone()
                .ok_or_else(|| anyhow!("送信元のない矢印が含まれています。"))?;
            let to = message
                .to
                .clone()
                .ok_or_else(|| anyhow!("宛先のない矢印が含まれています。"))?;

            // `A ->>+ B` and `A ->>- B` each add a second model entry for the
            // activation they imply. Folding it back into the arrow is what
            // makes the item list match the statements the user wrote.
            let mut deactivate = false;
            if message.activate {
                if messages.get(index).is_some_and(|next| {
                    next.message_type == LINETYPE_ACTIVE_START
                        && next.from.as_deref() == Some(to.as_str())
                }) {
                    index += 1;
                }
            } else if messages.get(index).is_some_and(|next| {
                next.message_type == LINETYPE_ACTIVE_END
                    && next.from.as_deref() == Some(from.as_str())
            }) && !lifecycle
                .get(cursor)
                .is_some_and(|(position, _)| *position == index)
            {
                // A `create` / `destroy` recorded at this very position stood
                // between the arrow and the `deactivate` in the source, and
                // folding the two together would write the `deactivate` back
                // on the other side of a statement whose place is its meaning.
                deactivate = true;
                index += 1;
            }

            items.push(SequenceItem::Message(SequenceMessage {
                from: source_id(&from),
                to: source_id(&to),
                arrow,
                activate: message.activate,
                deactivate,
                text: decode_text(message.message_text()),
                wrap: message.wrap,
            }));
            continue;
        }

        if let Some(kind) = SequenceBlockKind::start_from_model(message_type) {
            items.push(SequenceItem::BlockStart(SequenceBlock {
                kind,
                label: decode_text(message.message_text()),
                wrap: message.wrap,
            }));
            continue;
        }

        if let Some(kind) = SequenceBranchKind::from_model(message_type) {
            items.push(SequenceItem::BlockBranch(SequenceBranch {
                kind,
                label: decode_text(message.message_text()),
                wrap: message.wrap,
            }));
            continue;
        }

        if SequenceBlockKind::is_end(message_type) {
            items.push(SequenceItem::BlockEnd);
            continue;
        }

        match message_type {
            LINETYPE_NOTE => {
                let placement = message
                    .placement
                    .and_then(SequenceNotePlacement::from_model)
                    .ok_or_else(|| anyhow!("Note の位置を解釈できませんでした。"))?;
                let from = message
                    .from
                    .clone()
                    .ok_or_else(|| anyhow!("対象のない Note が含まれています。"))?;
                let to = message.to.clone().unwrap_or_else(|| from.clone());
                items.push(SequenceItem::Note(SequenceNote {
                    placement,
                    from: source_id(&from),
                    to: source_id(&to),
                    text: decode_text(message.message_text()),
                    wrap: message.wrap,
                }));
            }
            LINETYPE_ACTIVE_START => {
                let id = message
                    .from
                    .clone()
                    .ok_or_else(|| anyhow!("対象のない activate が含まれています。"))?;
                items.push(SequenceItem::Activate(source_id(&id)));
            }
            LINETYPE_ACTIVE_END => {
                let id = message
                    .from
                    .clone()
                    .ok_or_else(|| anyhow!("対象のない deactivate が含まれています。"))?;
                items.push(SequenceItem::Deactivate(source_id(&id)));
            }
            LINETYPE_AUTONUMBER => {
                let autonumber = match &message.message {
                    SequenceMessagePayload::Autonumber(autonumber) => SequenceAutonumber {
                        start: autonumber.start,
                        step: autonumber.step,
                        visible: autonumber.visible,
                    },
                    SequenceMessagePayload::Text(_) => SequenceAutonumber {
                        start: None,
                        step: None,
                        visible: true,
                    },
                };
                items.push(SequenceItem::Autonumber(autonumber));
            }
            other => bail!(
                "未対応の構文 (種別 {other}) が含まれています。テキストのまま編集してください。"
            ),
        }
    }
    push_lifecycle(&mut items, &lifecycle, &mut cursor, messages.len());

    Ok(items)
}

/// Writes `document` back as Mermaid source, without a trailing newline.
pub fn emit_sequence(document: &SequenceDocument) -> String {
    let mut out = String::new();
    // A `%%{init}%%` directive can turn wrapping on for the whole diagram, in
    // which case the absence of a prefix no longer means "do not wrap" and
    // every text has to say which it is.
    let explicit_wrap = document.preserved_lines.iter().any(|line| {
        let text = line.text.trim_start();
        text.starts_with("%%{") && text.contains("wrap")
    });

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }

    out.push_str(HEADER);
    out.push('\n');

    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }

    if let Some(title) = &document.title {
        let text = escape_text(title);
        if text.is_empty() {
            out.push_str(INDENT);
            out.push_str("title:\n");
        } else {
            let _ = writeln!(out, "{INDENT}title: {text}");
        }
    }

    emit_participants(document, explicit_wrap, &mut out);

    let mut depth = 0usize;
    for item in &document.items {
        match item {
            SequenceItem::BlockStart(block) => {
                let statement = keyword_statement(
                    block.kind.keyword(),
                    &block.label,
                    block.wrap,
                    explicit_wrap,
                );
                let _ = writeln!(out, "{}{statement}", INDENT.repeat(depth + 1));
                depth += 1;
            }
            SequenceItem::BlockBranch(branch) => {
                let statement = keyword_statement(
                    branch.kind.keyword(),
                    &branch.label,
                    branch.wrap,
                    explicit_wrap,
                );
                let _ = writeln!(out, "{}{statement}", INDENT.repeat(depth.max(1)));
            }
            SequenceItem::BlockEnd => {
                depth = depth.saturating_sub(1);
                let _ = writeln!(out, "{}end", INDENT.repeat(depth + 1));
            }
            SequenceItem::Message(message) => {
                let _ = writeln!(
                    out,
                    "{}{}",
                    INDENT.repeat(depth + 1),
                    message_statement(message, explicit_wrap)
                );
            }
            SequenceItem::Note(note) => {
                let _ = writeln!(
                    out,
                    "{}{}",
                    INDENT.repeat(depth + 1),
                    note_statement(note, explicit_wrap)
                );
            }
            SequenceItem::Activate(id) => {
                let _ = writeln!(out, "{}activate {id}", INDENT.repeat(depth + 1));
            }
            SequenceItem::Deactivate(id) => {
                let _ = writeln!(out, "{}deactivate {id}", INDENT.repeat(depth + 1));
            }
            SequenceItem::Create(id) => {
                let declaration = document
                    .participants
                    .iter()
                    .find(|participant| participant.id == *id)
                    .map(|participant| participant_declaration(participant, explicit_wrap))
                    // A `create` for an id no participant carries is reported
                    // by `structure_error`; the bare declaration at least keeps
                    // the statement readable instead of dropping it.
                    .unwrap_or_else(|| format!("participant {id}"));
                let _ = writeln!(out, "{}create {declaration}", INDENT.repeat(depth + 1));
            }
            SequenceItem::Destroy(id) => {
                let _ = writeln!(out, "{}destroy {id}", INDENT.repeat(depth + 1));
            }
            SequenceItem::Autonumber(autonumber) => {
                let _ = writeln!(
                    out,
                    "{}{}",
                    INDENT.repeat(depth + 1),
                    autonumber_statement(autonumber)
                );
            }
        }
    }

    for line in preserved_at(document, PreservedPosition::Bottom) {
        out.push_str(line);
        out.push('\n');
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn preserved_at(
    document: &SequenceDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

fn emit_participants(document: &SequenceDocument, explicit_wrap: bool, out: &mut String) {
    let mut emitted_boxes = vec![false; document.boxes.len()];
    // A `create`d participant is declared by its own statement further down;
    // declaring it here as well is the one error Mermaid refuses outright.
    let created = document.created_ids();

    for participant in &document.participants {
        if created.contains(&participant.id.as_str()) {
            continue;
        }
        let Some(box_index) = participant.box_index else {
            let _ = writeln!(
                out,
                "{INDENT}{}",
                participant_declaration(participant, explicit_wrap)
            );
            continue;
        };
        let Some(group) = document.boxes.get(box_index) else {
            // A dangling index is reported by `structure_error`; writing the
            // participant outside any box at least keeps it in the diagram.
            let _ = writeln!(
                out,
                "{INDENT}{}",
                participant_declaration(participant, explicit_wrap)
            );
            continue;
        };
        if emitted_boxes.get(box_index).copied().unwrap_or(true) {
            continue;
        }
        if let Some(emitted) = emitted_boxes.get_mut(box_index) {
            *emitted = true;
        }

        let _ = writeln!(out, "{INDENT}{}", box_header(group, explicit_wrap));
        for member in &document.participants {
            if member.box_index == Some(box_index) && !created.contains(&member.id.as_str()) {
                let _ = writeln!(
                    out,
                    "{}{}",
                    INDENT.repeat(2),
                    participant_declaration(member, explicit_wrap)
                );
            }
        }
        let _ = writeln!(out, "{INDENT}end");
    }

    // A `box` with no participants draws an empty group, so it would otherwise
    // disappear from a diagram that has one.
    for (index, group) in document.boxes.iter().enumerate() {
        if emitted_boxes.get(index).copied().unwrap_or(true) {
            continue;
        }
        let _ = writeln!(out, "{INDENT}{}", box_header(group, explicit_wrap));
        let _ = writeln!(out, "{INDENT}end");
    }
}

fn participant_declaration(participant: &SequenceParticipant, explicit_wrap: bool) -> String {
    // Mermaid has no spelling for a config object together with `actor` or
    // `as`; `structure_error` reports both before the editor writes.
    if let Some(settings) = &participant.preserved_settings {
        return format!(
            "{} {}@{{{settings}}}",
            SequenceParticipantKind::Participant.keyword(),
            participant.id
        );
    }
    let keyword = participant.kind.keyword();
    match &participant.alias {
        Some(alias) => format!(
            "{keyword} {} as {}{}",
            participant.id,
            wrap_prefix(participant.wrap, explicit_wrap),
            escape_text(alias)
        ),
        None => format!("{keyword} {}", participant.id),
    }
}

fn box_header(group: &SequenceGroupBox, explicit_wrap: bool) -> String {
    let name = group
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .map(|name| {
            format!(
                "{}{}",
                wrap_prefix(group.wrap, explicit_wrap),
                escape_text(name)
            )
        })
        // A box with no name still carries its wrap setting, which a bare
        // prefix keeps: Mermaid reads `box wrap:` as a nameless wrapping box.
        .unwrap_or_else(|| wrap_prefix(group.wrap, explicit_wrap).to_string());
    match &group.fill {
        Some(fill) if name.is_empty() => format!("box {fill}"),
        Some(fill) => format!("box {fill} {name}"),
        // Mermaid reads the first word of a `box` header as a CSS color when it
        // is one, so a group called "Gold" needs the default color spelled out
        // or its name would be read as the color and lost.
        None if header_starts_with_color(&name) => format!("box transparent {name}"),
        None if name.is_empty() => "box".to_string(),
        None => format!("box {name}"),
    }
}

fn message_statement(message: &SequenceMessage, explicit_wrap: bool) -> String {
    let activation = if message.activate {
        "+"
    } else if message.deactivate {
        "-"
    } else {
        ""
    };
    let text = format!(
        "{}{}",
        wrap_prefix(message.wrap, explicit_wrap),
        escape_text(&message.text)
    );
    let separator = if text.is_empty() { "" } else { " " };
    // `A-` written straight against `->>` reads as `A` followed by `-->>`, a
    // different arrow from a different participant. A space ends the id first.
    let gap = if message.from.ends_with('-') { " " } else { "" };
    format!(
        "{}{gap}{}{activation}{}:{separator}{text}",
        message.from,
        message.arrow.operator(),
        message.to
    )
}

fn note_statement(note: &SequenceNote, explicit_wrap: bool) -> String {
    let target = if note.placement == SequenceNotePlacement::Over && note.from != note.to {
        format!("{},{}", note.from, note.to)
    } else {
        note.from.clone()
    };
    let text = format!(
        "{}{}",
        wrap_prefix(note.wrap, explicit_wrap),
        escape_text(&note.text)
    );
    let separator = if text.is_empty() { "" } else { " " };
    format!(
        "Note {} {target}:{separator}{text}",
        note.placement.keyword()
    )
}

fn keyword_statement(keyword: &str, label: &str, wrap: bool, explicit_wrap: bool) -> String {
    let label = format!("{}{}", wrap_prefix(wrap, explicit_wrap), escape_text(label));
    if label.is_empty() {
        keyword.to_string()
    } else {
        format!("{keyword} {label}")
    }
}

fn autonumber_statement(autonumber: &SequenceAutonumber) -> String {
    if !autonumber.visible {
        return "autonumber off".to_string();
    }
    let Some(start) = autonumber.start else {
        return "autonumber".to_string();
    };
    format!(
        "autonumber {} {}",
        format_number(start),
        format_number(autonumber.step.unwrap_or(1.0))
    )
}

/// Mermaid's number lexer takes at most two decimal places and no sign, so a
/// value outside that never round-trips.
fn format_number(value: f64) -> String {
    if !value.is_finite() || value < 0.0 {
        return "0".to_string();
    }
    if value.fract() == 0.0 && value < 1e15 {
        return format!("{}", value as u64);
    }
    let formatted = format!("{value:.2}");
    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

/// The `wrap:` / `nowrap:` prefix Mermaid strips off a text token.
fn wrap_prefix(wrap: bool, explicit: bool) -> &'static str {
    match (wrap, explicit) {
        (true, _) => "wrap:",
        (false, true) => "nowrap:",
        (false, false) => "",
    }
}

/// Whether `id` can be written as a bare Mermaid participant id.
///
/// The lexer ends an id at whitespace and at the characters that start
/// something else — a text (`:`), an activation (`+`), a list separator (`,`),
/// a statement separator (`;`), an arrow, a config object (`@{`) or a central
/// connection (`()`) — and reads a leading digit as a number, which would turn
/// `007` into `7`.
///
/// This is the editor's rule for what a user may type, and is stricter than
/// [`is_writable_participant_id`]: it also refuses the characters that do
/// survive but only with an escape or a separating space.
///
/// The `#…;` shorthand is the one exception to the ban on `#`: Mermaid turns it
/// into a placeholder before it reads the id, and the editor keeps the id in
/// that spelling, so it comes back as typed.
pub fn is_valid_participant_id(id: &str) -> bool {
    is_writable_participant_id(id)
        && !id.ends_with('-')
        && !mask_entity_shorthands(id).contains(['>', '@', '(', ')', '#', '%', '"'])
}

/// Whether `id` can be written back as itself at all.
///
/// A diagram written by hand may name a participant with characters the editor
/// would not offer, and those still have to survive being written out again,
/// so this is the rule Mermaid's own lexer imposes rather than the editor's.
fn is_writable_participant_id(id: &str) -> bool {
    // A leading digit is read as a number; a leading `-` after an arrow is the
    // deactivation marker; a leading `#` or `%%` opens a comment; a trailing
    // `()` in front of an arrow is the central connection marker. The `;` that
    // closes a `#…;` shorthand is gone before the lexer looks for statement
    // ends, so only the rest of the id is held to the lexer's rules.
    if id.starts_with('#') {
        return false;
    }
    let masked = mask_entity_shorthands(id);
    let id = masked.as_str();
    if id.is_empty()
        || id.starts_with(|character: char| character.is_ascii_digit())
        || id.starts_with(['-', '#', '%', '('])
        || id.ends_with("()")
        || id.contains("%%")
    {
        return false;
    }
    let mut characters = id.chars().peekable();
    while let Some(character) = characters.next() {
        if character.is_whitespace() || matches!(character, ':' | '+' | ',' | ';' | '<') {
            return false;
        }
        let ends_the_id = match character {
            '@' => characters.peek() == Some(&'{'),
            '-' => characters
                .peek()
                .is_some_and(|next| matches!(next, '-' | '>' | 'x' | ')')),
            _ => false,
        };
        if ends_the_id {
            return false;
        }
    }
    true
}

/// `id` with every `#…;` shorthand replaced by one ordinary character, which
/// is how Mermaid's lexer sees it: the shorthand becomes a placeholder before
/// the lexer runs.
fn mask_entity_shorthands(id: &str) -> String {
    let mut masked = String::with_capacity(id.len());
    let mut rest = id;
    while let Some(hash) = rest.find('#') {
        masked.push_str(rest.get(..hash).unwrap_or_default());
        let after = rest.get(hash + 1..).unwrap_or_default();
        if is_entity_shorthand(rest, hash) {
            let word_end = after.find(';').unwrap_or(after.len());
            masked.push('_');
            rest = after.get(word_end + 1..).unwrap_or_default();
        } else {
            masked.push('#');
            rest = after;
        }
    }
    masked.push_str(rest);
    masked
}

/// `merman` hands ids back with each `#…;` shorthand already turned into its
/// placeholder characters. An id is written back verbatim, so it is kept in
/// the spelling the source used rather than decoded like a text.
fn source_id(id: &str) -> String {
    if !id.contains(['\u{FB02}', '\u{B6}']) {
        return id.to_string();
    }
    id.replace("\u{FB02}\u{B0}\u{B0}", "#")
        .replace("\u{FB02}\u{B0}", "#")
        .replace("\u{B6}\u{DF}", ";")
}

/// `id` with each `#…;` shorthand turned into `merman`'s placeholder, the way
/// its preprocessor does it: the inverse of [`source_id`].
fn encode_entity_shorthands(id: &str) -> String {
    let mut encoded = String::with_capacity(id.len());
    let mut rest = id;
    while let Some(hash) = rest.find('#') {
        encoded.push_str(rest.get(..hash).unwrap_or_default());
        let after = rest.get(hash + 1..).unwrap_or_default();
        if is_entity_shorthand(rest, hash) {
            let word_end = after.find(';').unwrap_or(after.len());
            let word = after.get(..word_end).unwrap_or_default();
            if word.bytes().all(|byte| byte.is_ascii_digit()) {
                encoded.push_str("\u{FB02}\u{B0}\u{B0}");
            } else {
                encoded.push_str("\u{FB02}\u{B0}");
            }
            encoded.push_str(word);
            encoded.push_str("\u{B6}\u{DF}");
            rest = after.get(word_end + 1..).unwrap_or_default();
        } else {
            encoded.push('#');
            rest = after;
        }
    }
    encoded.push_str(rest);
    encoded
}

/// The sequence lexer turns `#…;` into a placeholder before reading the text,
/// and the renderer turns the placeholder into the character it names. Reading
/// a model therefore means undoing both steps.
fn decode_text(text: &str) -> String {
    merman::entities::decode_mermaid_entities_to_unicode(text).into_owned()
}

/// Writes `text` so that Mermaid reads it back unchanged.
///
/// `#` starts a comment, `;` ends the statement and `%%` starts a comment, so
/// each becomes the `#…;` entity shorthand that Mermaid decodes after lexing.
/// Text tokens are also trimmed, and a leading `wrap:` / `nowrap:` would be
/// read as the wrap prefix rather than as text.
fn escape_text(text: &str) -> String {
    let needs_entities = text.contains(['#', ';', '\n', '\r'])
        || text.contains("%%")
        || has_wrap_prefix(text)
        || text.trim_matches([' ', '\t']).len() != text.len();
    let mut body = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '#' => body.push_str("#35;"),
            ';' => body.push_str("#59;"),
            // Only the second `%` of a pair starts a comment, so a lone `%`
            // stays readable as itself.
            '%' if characters.peek() == Some(&'%') => body.push_str("#37;"),
            // A colon directly after a `#` inside a line that also says
            // `style` or `classDef` makes Mermaid delete the `;` that ends the
            // entity, which would then swallow the rest of the line.
            ':' if needs_entities => body.push_str("#58;"),
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                body.push_str("<br/>");
            }
            '\n' => body.push_str("<br/>"),
            _ => body.push(character),
        }
    }
    protect_outer_whitespace(&body)
}

/// Replaces the spaces and tabs at either end of `text` with entities, because
/// Mermaid trims every text token.
fn protect_outer_whitespace(text: &str) -> String {
    const OUTER: [char; 2] = [' ', '\t'];
    let start = text.len() - text.trim_start_matches(OUTER).len();
    let end = text.trim_end_matches(OUTER).len();
    if start == 0 && end == text.len() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    if end <= start {
        for character in text.chars() {
            out.push_str(whitespace_entity(character));
        }
        return out;
    }
    for character in text.get(..start).unwrap_or_default().chars() {
        out.push_str(whitespace_entity(character));
    }
    out.push_str(text.get(start..end).unwrap_or_default());
    for character in text.get(end..).unwrap_or_default().chars() {
        out.push_str(whitespace_entity(character));
    }
    out
}

fn whitespace_entity(character: char) -> &'static str {
    match character {
        '\t' => "#9;",
        _ => "#32;",
    }
}

/// Whether Mermaid would read the start of `text` as the wrap prefix.
fn has_wrap_prefix(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.len() < 5 {
        return false;
    }
    ["wrap:", "nowrap:", ":wrap:", ":nowrap:"]
        .iter()
        .any(|prefix| {
            trimmed
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        })
}

/// Whether Mermaid's `box` header would read the first word of `name` as the
/// group's color.
fn header_starts_with_color(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    for prefix in ["rgba", "rgb", "hsla", "hsl"] {
        if lower.starts_with(prefix) && name.contains(')') {
            return true;
        }
    }
    let word_end = name
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(name.len());
    let Some(word) = name.get(..word_end) else {
        return false;
    };
    if word.is_empty()
        || !word
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        return false;
    }
    let word = word.to_ascii_lowercase();
    CSS_COLOR_KEYWORDS.contains(&format!("|{word}|"))
}

/// The CSS color keywords Mermaid accepts in a `box` header, as one string so
/// that the table does not dominate the file. Mermaid also accepts
/// `transparent`, which is the default and so is listed here too.
const CSS_COLOR_KEYWORDS: &str = concat!(
    "|transparent|aliceblue|antiquewhite|aqua|aquamarine|azure|beige|bisque|black|blanchedalmond|",
    "blue|blueviolet|brown|burlywood|cadetblue|chartreuse|chocolate|coral|cornflowerblue|cornsilk|",
    "crimson|cyan|darkblue|darkcyan|darkgoldenrod|darkgray|darkgreen|darkgrey|darkkhaki|",
    "darkmagenta|darkolivegreen|darkorange|darkorchid|darkred|darksalmon|darkseagreen|",
    "darkslateblue|darkslategray|darkslategrey|darkturquoise|darkviolet|deeppink|deepskyblue|",
    "dimgray|dimgrey|dodgerblue|firebrick|floralwhite|forestgreen|fuchsia|gainsboro|ghostwhite|",
    "gold|goldenrod|gray|green|greenyellow|grey|honeydew|hotpink|indianred|indigo|ivory|khaki|",
    "lavender|lavenderblush|lawngreen|lemonchiffon|lightblue|lightcoral|lightcyan|",
    "lightgoldenrodyellow|lightgray|lightgreen|lightgrey|lightpink|lightsalmon|lightseagreen|",
    "lightskyblue|lightslategray|lightslategrey|lightsteelblue|lightyellow|lime|limegreen|linen|",
    "magenta|maroon|mediumaquamarine|mediumblue|mediumorchid|mediumpurple|mediumseagreen|",
    "mediumslateblue|mediumspringgreen|mediumturquoise|mediumvioletred|midnightblue|mintcream|",
    "mistyrose|moccasin|navajowhite|navy|oldlace|olive|olivedrab|orange|orangered|orchid|",
    "palegoldenrod|palegreen|paleturquoise|palevioletred|papayawhip|peachpuff|peru|pink|plum|",
    "powderblue|purple|rebeccapurple|red|rosybrown|royalblue|saddlebrown|salmon|sandybrown|",
    "seagreen|seashell|sienna|silver|skyblue|slateblue|slategray|slategrey|snow|springgreen|",
    "steelblue|tan|teal|thistle|tomato|turquoise|violet|wheat|white|whitesmoke|yellow|yellowgreen|",
);

/// What the line scanner recovers from the source text that the parsed model
/// cannot express.
struct SourceScan {
    preserved: Vec<PreservedLine>,
    /// The lines the scanner handed to the parser, i.e. everything that is
    /// neither the header nor preserved. Only the model speaks for these, so
    /// they are where a construct the model drops has to be caught.
    statements: Vec<String>,
}

fn first_statement_line(source: &str) -> Option<&str> {
    let mut lines = source.lines().map(str::trim);
    let mut in_frontmatter = false;
    let mut seen_content = false;
    let mut in_directive = false;

    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        if in_directive {
            if line.contains("}%%") {
                in_directive = false;
            }
            continue;
        }
        if in_frontmatter {
            if line == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        if !seen_content && line == "---" {
            seen_content = true;
            in_frontmatter = true;
            continue;
        }
        seen_content = true;
        if line.starts_with("%%") {
            if line.starts_with("%%{") && !line.contains("}%%") {
                in_directive = true;
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        return Some(line);
    }
    None
}

/// The rest of the header line, when `line` opens a sequence diagram.
fn header_rest(line: &str) -> Option<&str> {
    let head = line.get(..HEADER.len())?;
    if !head.eq_ignore_ascii_case(HEADER) {
        return None;
    }
    let rest = line.get(HEADER.len()..)?;
    if rest.is_empty()
        || rest.starts_with(|character: char| character.is_whitespace() || character == ';')
    {
        Some(rest)
    } else {
        None
    }
}

/// Whether `line` is a whole-line comment.
fn is_comment_line(line: &str) -> bool {
    line.starts_with("%%") || line.starts_with('#')
}

/// Statements that `merman`'s sequence model does not carry, and that are
/// therefore written back verbatim.
fn preserved_keyword(line: &str) -> bool {
    const KEYWORDS: [&str; 6] = [
        "accTitle",
        "accDescr",
        "links",
        "link",
        "properties",
        "details",
    ];
    KEYWORDS.iter().any(|keyword| {
        line.get(..keyword.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(keyword))
            && line.get(keyword.len()..).is_some_and(|rest| {
                rest.is_empty()
                    || rest.starts_with(|character: char| {
                        character.is_whitespace() || character == ':' || character == '{'
                    })
            })
    })
}

fn scan_source(source: &str) -> Result<SourceScan> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut preserved = Vec::new();
    let mut statements = Vec::new();
    let mut index = 0;
    let mut seen_content = false;
    let mut header = None;

    while let Some(line) = lines.get(index).copied() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            index += 1;
            continue;
        }
        if !seen_content && trimmed == "---" {
            seen_content = true;
            let start = index;
            index += 1;
            while lines.get(index).is_some_and(|line| line.trim() != "---") {
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            preserved.push(PreservedLine {
                position: PreservedPosition::Preamble,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }
        seen_content = true;
        if is_comment_line(trimmed) {
            let start = index;
            index += 1;
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            preserved.push(PreservedLine {
                position: PreservedPosition::Preamble,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }
        let Some(rest) = header_rest(trimmed) else {
            bail!("この図はシーケンス図ではありません。");
        };
        header = Some(());
        let rest = rest.trim_start_matches(';').trim();
        if !rest.is_empty() {
            statements.push(rest.to_string());
        }
        index += 1;
        break;
    }

    if header.is_none() {
        // An empty fence: the editor opens on a blank diagram, and whatever
        // comments the user left keep their place.
        return Ok(SourceScan {
            preserved,
            statements: Vec::new(),
        });
    }

    let mut position = PreservedPosition::Top;
    while let Some(line) = lines.get(index).copied() {
        let trimmed = line.trim();
        index += 1;
        if trimmed.is_empty() {
            continue;
        }
        if is_comment_line(trimmed) {
            let start = index - 1;
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            preserved.push(PreservedLine {
                position,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }
        if preserved_keyword(trimmed) {
            let start = index - 1;
            if trimmed.ends_with('{') {
                while lines.get(index).is_some_and(|line| line.trim() != "}") {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            preserved.push(PreservedLine {
                position,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }

        statements.push(trimmed.to_string());
        position = PreservedPosition::Bottom;
    }

    Ok(SourceScan {
        preserved,
        statements,
    })
}

fn join_lines(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Why `statements` cannot be edited as a form, if there is a reason.
///
/// A statement the model carries only in part is worse than one it refuses:
/// the editor would write the diagram back without the part it dropped.
fn unsupported_construct(statements: &[String]) -> Option<String> {
    // `merman` reports one position per destroyed column, so a column that is
    // destroyed twice would come back with only the second statement.
    let mut destroyed: Vec<&str> = Vec::new();
    // Mermaid's lexer ends a statement at `;` as well as at a line break, so a
    // line holding two of them has to be read as two statements here too.
    for statement in statements
        .iter()
        .flat_map(|statement| split_statements(statement))
    {
        let mut words = statement.split_whitespace();
        if !words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("destroy"))
        {
            continue;
        }
        let Some(id) = words.next() else {
            continue;
        };
        if destroyed.contains(&id) {
            return Some(format!(
                "`destroy {id}` が 2 回書かれているため、構造化編集に対応していません。テキストのまま編集してください。"
            ));
        }
        destroyed.push(id);
    }

    for statement in statements {
        if let Some(offset) = comment_start(statement) {
            let comment = statement.get(offset..).unwrap_or_default();
            return Some(format!(
                "文の末尾のコメント ({comment}) は構造化編集で失われます。コメントを独立した行に移すか、テキストのまま編集してください。"
            ));
        }
    }
    None
}

/// `line` cut at every `;` that ends a statement. The `;` closing a `#…;`
/// shorthand does not: Mermaid replaces the shorthand before it lexes.
fn split_statements(line: &str) -> Vec<&str> {
    let mut statements = Vec::new();
    let mut start = 0;
    let mut entity_end = None;
    for (offset, character) in line.char_indices() {
        match character {
            '#' if is_entity_shorthand(line, offset) => {
                entity_end = line
                    .get(offset..)
                    .and_then(|rest| rest.find(';'))
                    .map(|end| offset + end);
            }
            ';' if entity_end == Some(offset) => entity_end = None,
            ';' => {
                statements.push(line.get(start..offset).unwrap_or_default());
                start = offset + 1;
            }
            _ => {}
        }
    }
    statements.push(line.get(start..).unwrap_or_default());
    statements
}

/// The rest of `statement` after `keyword` and the whitespace that has to
/// follow it, matching the keyword the way Mermaid's lexer does: without
/// regard to case.
fn strip_keyword<'a>(statement: &'a str, keyword: &str) -> Option<&'a str> {
    let head = statement.get(..keyword.len())?;
    let rest = statement.get(keyword.len()..)?;
    (head.eq_ignore_ascii_case(keyword) && rest.starts_with(char::is_whitespace))
        .then(|| rest.trim_start())
}

/// The config object each participant's last declaration carried, keyed by
/// the id as the source spells it, or `None` for a declaration without one.
///
/// `merman` reads only the `type` out of the object and keeps nothing of its
/// spelling, so the text between the braces is taken from the source and
/// written back as it was. An object that spans lines cannot be carried that
/// way and is refused instead.
fn declared_settings(statements: &[String]) -> Result<HashMap<String, Option<String>>> {
    let mut settings = HashMap::new();
    for statement in statements
        .iter()
        .flat_map(|statement| split_statements(statement))
    {
        let statement = statement.trim();
        let statement = strip_keyword(statement, "create").unwrap_or(statement);
        let Some(declaration) =
            strip_keyword(statement, "participant").or_else(|| strip_keyword(statement, "actor"))
        else {
            continue;
        };
        let id_end = declaration
            .find(char::is_whitespace)
            .unwrap_or(declaration.len());
        let token = declaration.get(..id_end).unwrap_or_default();
        let Some(config_start) = token.find("@{") else {
            settings.insert(token.to_string(), None);
            continue;
        };
        let id = declaration.get(..config_start).unwrap_or_default();
        let body = declaration.get(config_start + 2..).unwrap_or_default();
        let Some(body_end) = body.find('}') else {
            bail!(
                "参加者 `{id}` の設定オブジェクト (`@{{ … }}`) が複数行にまたがっているため、構造化編集に対応していません。テキストのまま編集してください。"
            );
        };
        settings.insert(
            id.to_string(),
            Some(body.get(..body_end).unwrap_or_default().to_string()),
        );
    }
    Ok(settings)
}

/// Where a comment starts inside a statement, if it has one.
///
/// `#` also opens the `#…;` entity shorthand, which Mermaid replaces before it
/// looks for comments, so only a `#` that no `;` closes is a comment.
fn comment_start(statement: &str) -> Option<usize> {
    for (offset, character) in statement.char_indices() {
        match character {
            '%' if statement.get(offset..offset + 2) == Some("%%") => return Some(offset),
            '#' if !is_entity_shorthand(statement, offset) => return Some(offset),
            _ => {}
        }
    }
    None
}

fn is_entity_shorthand(statement: &str, hash: usize) -> bool {
    let rest = statement.get(hash + 1..).unwrap_or_default();
    let word_end = rest
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(rest.len());
    word_end > 0
        && rest
            .get(word_end..)
            .is_some_and(|rest| rest.starts_with(';'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    fn round_trip(source: &str) -> (SequenceDocument, String, SequenceDocument, String) {
        let first = parse_sequence(source).expect("source should parse");
        let first_text = emit_sequence(&first);
        let second = parse_sequence(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        let second_text = emit_sequence(&second);
        (first, first_text, second, second_text)
    }

    /// The emitted source must parse to the same document and emit the same
    /// text, otherwise confirming the editor twice would keep rewriting the
    /// fence.
    fn assert_stable(source: &str) -> String {
        let (first, first_text, second, second_text) = round_trip(source);
        assert_eq!(
            first_text, second_text,
            "emitting twice changed the source:\n{first_text}\n---\n{second_text}"
        );
        assert_eq!(
            first.participants, second.participants,
            "participants changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first.boxes, second.boxes,
            "boxes changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first.items, second.items,
            "items changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first.title, second.title,
            "title changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first.structure_error(),
            None,
            "a parsed document should always be writable back:\n{first_text}"
        );
        first_text
    }

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("emitted source should render: {error}\n{source}"));
        assert!(
            !svg.contains("Syntax error in text"),
            "emitted source rendered an error diagram:\n{source}"
        );
    }

    const EXAMPLES: [&str; 12] = [
        "sequenceDiagram\n    participant User as 利用者\n    participant App as アプリ\n    User->>App: 要求\n    App-->>User: 応答\n    Note over App: 補足",
        "sequenceDiagram\n  A->>B: hi",
        "sequenceDiagram\n  actor アリス\n  participant Bob\n  アリス->>Bob: こんにちは\n  Bob--)アリス: 非同期の返事",
        "sequenceDiagram\n  autonumber\n  A->>B: 1 つめ\n  B->>A: 2 つめ",
        "sequenceDiagram\n  autonumber 10 5\n  A->>B: x\n  autonumber off\n  B->>A: y",
        "sequenceDiagram\n  A->>+B: 開始\n  B-->>-A: 終了",
        "sequenceDiagram\n  participant A\n  participant B\n  activate A\n  A->>B: x\n  deactivate A",
        "sequenceDiagram\n  loop 毎日\n    A->>B: 確認\n  end\n  alt 成功\n    B->>A: OK\n  else 失敗\n    B->>A: NG\n  end",
        "sequenceDiagram\n  par 並行\n    A->>B: x\n  and もう一つ\n    A->>C: y\n  end\n  critical 重要\n    A->>B: z\n  option 代替\n    A->>C: w\n  end",
        "sequenceDiagram\n  opt 任意\n    A->>B: x\n  end\n  break 中断\n    A->>B: y\n  end\n  rect rgb(200, 200, 255)\n    A->>B: z\n  end",
        "sequenceDiagram\n  box 業務システム\n    participant App as アプリ\n    participant Db as DB\n  end\n  participant User as 利用者\n  User->>App: 要求\n  App->>Db: 問い合わせ",
        "sequenceDiagram\n  title: 認証の流れ\n  Note left of A: 左\n  Note right of B: 右\n  Note over A,B: 両方\n  A->>B: x",
    ];

    #[test]
    fn examples_round_trip_and_render() {
        for source in EXAMPLES {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn sequence_template_round_trips() {
        let template = "sequenceDiagram\n    %% ->> は呼び出し、-->> は応答\n    participant User as 利用者\n    participant App as アプリ\n    User->>App: 要求\n    App-->>User: 応答\n    Note over App: 補足";
        let emitted = assert_stable(template);
        assert!(
            emitted.contains("%% ->> は呼び出し"),
            "the template comment should survive:\n{emitted}"
        );
        assert!(
            emitted.contains("Note over App: 補足"),
            "the note should survive:\n{emitted}"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn every_arrow_round_trips() {
        for arrow in SequenceArrow::ALL {
            let source = format!("sequenceDiagram\n  A{}B: ラベル", arrow.operator());
            let document = parse_sequence(&source)
                .unwrap_or_else(|error| panic!("`{}` should parse: {error}", arrow.operator()));
            let Some(SequenceItem::Message(message)) = document.items.first() else {
                panic!("`{}` should produce a message", arrow.operator());
            };
            assert_eq!(message.arrow, arrow, "arrow of `{}`", arrow.operator());
            assert_eq!(message.from, "A");
            assert_eq!(message.to, "B");
            assert_eq!(message.text, "ラベル");
            let emitted = assert_stable(&source);
            assert_renders(&emitted);
        }
    }

    /// `+` and `-` carry information the model keeps in two places, so both
    /// spellings and the standalone statements have to survive.
    #[test]
    fn activation_spellings_round_trip() {
        let document = parse_sequence("sequenceDiagram\n  A->>+B: 開始\n  B-->>-A: 終了")
            .expect("source should parse");
        let activations = document
            .items
            .iter()
            .filter_map(|item| match item {
                SequenceItem::Message(message) => Some((message.activate, message.deactivate)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(activations, vec![(true, false), (false, true)]);
        assert_eq!(
            document.items.len(),
            2,
            "the activations the parser adds should be folded into the arrows"
        );

        // The long spelling means exactly the same thing, so it is written
        // back as the short one and then stays put.
        let emitted =
            assert_stable("sequenceDiagram\n  A->>+B: 開始\n  B-->>A: 終了\n  deactivate B");
        assert!(
            emitted.contains("B-->>-A: 終了"),
            "a deactivation right after a message belongs to it:\n{emitted}"
        );
        assert_renders(&emitted);

        let standalone = assert_stable(
            "sequenceDiagram\n  participant A\n  participant B\n  activate B\n  A->>B: x\n  deactivate B",
        );
        assert!(
            standalone.contains("activate B"),
            "a standalone activation should stay standalone:\n{standalone}"
        );
        assert_renders(&standalone);
    }

    #[test]
    fn nested_blocks_keep_their_nesting() {
        let source = "sequenceDiagram\n  loop 外\n    alt 内\n      A->>B: x\n    else 他\n      A->>B: y\n    end\n  end";
        let emitted = assert_stable(source);
        let alt_line = emitted
            .lines()
            .find(|line| line.trim_start().starts_with("alt"))
            .expect("the alt block should survive");
        let loop_line = emitted
            .lines()
            .find(|line| line.trim_start().starts_with("loop"))
            .expect("the loop block should survive");
        assert!(
            alt_line.len() - alt_line.trim_start().len()
                > loop_line.len() - loop_line.trim_start().len(),
            "the inner block should be indented further:\n{emitted}"
        );
        assert_eq!(emitted.matches("end").count(), 2, "{emitted}");
        assert_renders(&emitted);
    }

    #[test]
    fn every_block_kind_round_trips() {
        for kind in SequenceBlockKind::ALL {
            let mut source = format!(
                "sequenceDiagram\n  {} ラベル\n    A->>B: x\n",
                kind.keyword()
            );
            if let Some(branch) = kind.branch() {
                source.push_str(&format!(
                    "  {} 別のラベル\n    A->>B: y\n",
                    branch.keyword()
                ));
            }
            source.push_str("  end");

            let document = parse_sequence(&source)
                .unwrap_or_else(|error| panic!("`{}` should parse: {error}", kind.keyword()));
            let Some(SequenceItem::BlockStart(block)) = document.items.first() else {
                panic!("`{}` should open a block", kind.keyword());
            };
            assert_eq!(block.kind, kind);
            assert_eq!(block.label, "ラベル");
            let emitted = assert_stable(&source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn notes_keep_their_placement() {
        let document =
            parse_sequence("sequenceDiagram\n  Note left of A: 左\n  Note right of A: 右\n  Note over A: 上\n  Note over A,B: 両方")
                .expect("source should parse");
        let notes = document
            .items
            .iter()
            .filter_map(|item| match item {
                SequenceItem::Note(note) => {
                    Some((note.placement, note.from.clone(), note.to.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            notes,
            vec![
                (SequenceNotePlacement::LeftOf, "A".into(), "A".into()),
                (SequenceNotePlacement::RightOf, "A".into(), "A".into()),
                (SequenceNotePlacement::Over, "A".into(), "A".into()),
                (SequenceNotePlacement::Over, "A".into(), "B".into()),
            ]
        );
        let emitted = assert_stable("sequenceDiagram\n  Note left of A: 左\n  Note over A,B: 両方");
        assert_renders(&emitted);
    }

    #[test]
    fn autonumber_forms_round_trip() {
        for (source, expected) in [
            (
                "autonumber",
                SequenceAutonumber {
                    start: None,
                    step: None,
                    visible: true,
                },
            ),
            (
                "autonumber off",
                SequenceAutonumber {
                    start: None,
                    step: None,
                    visible: false,
                },
            ),
            (
                "autonumber 10",
                SequenceAutonumber {
                    start: Some(10.0),
                    step: Some(1.0),
                    visible: true,
                },
            ),
            (
                "autonumber 10 5",
                SequenceAutonumber {
                    start: Some(10.0),
                    step: Some(5.0),
                    visible: true,
                },
            ),
        ] {
            let text = format!("sequenceDiagram\n  {source}\n  A->>B: x");
            let document = parse_sequence(&text)
                .unwrap_or_else(|error| panic!("`{source}` should parse: {error}"));
            let Some(SequenceItem::Autonumber(autonumber)) = document.items.first() else {
                panic!("`{source}` should produce an autonumber item");
            };
            assert_eq!(*autonumber, expected, "for `{source}`");
            let emitted = assert_stable(&text);
            assert_renders(&emitted);
        }
    }

    /// Characters Mermaid's sequence lexer reads as syntax have to come back as
    /// themselves, because the editor has no other way to spell them.
    #[test]
    fn texts_with_syntax_characters_survive() {
        for text in [
            "コメント %% ではない",
            "番号 # 記号",
            "セミコロン ; あり",
            "コロン : あり",
            "引用 \" と ' あり",
            "矢印 ->> のような文字",
            "style: #fff のような文字",
            "wrap: で始まる文",
            "nowrap: で始まる文",
            "  前後に空白  ",
            "百分率 100% 完了",
            "波かっこ {あり} と [かっこ]",
        ] {
            let message = SequenceMessage {
                from: "A".into(),
                to: "B".into(),
                arrow: SequenceArrow::Solid,
                activate: false,
                deactivate: false,
                text: text.into(),
                wrap: false,
            };
            let source = format!(
                "sequenceDiagram\n{INDENT}{}",
                message_statement(&message, false)
            );
            let document = parse_sequence(&source)
                .unwrap_or_else(|error| panic!("`{text}` should parse: {error}\n{source}"));
            let Some(SequenceItem::Message(parsed)) = document.items.first() else {
                panic!("`{text}` should produce a message\n{source}");
            };
            assert_eq!(parsed.text, text, "text of {source}");
            assert!(!parsed.wrap, "wrap of {source}");
            assert_renders(&source);
        }
    }

    /// A description, a block label and a box name all go through the same
    /// text token as a message, so they need the same escaping.
    #[test]
    fn descriptions_and_labels_with_syntax_characters_survive() {
        for text in [
            "利用者 ; と # と %% あり",
            "wrap: で始まる",
            "コロン : と style: あり",
            "  前後に空白  ",
        ] {
            let mut document = SequenceDocument::empty();
            document.participants.push(SequenceParticipant {
                id: "A".into(),
                alias: Some(text.to_string()),
                kind: SequenceParticipantKind::Participant,
                wrap: false,
                box_index: Some(0),
                preserved_settings: None,
            });
            document.boxes.push(SequenceGroupBox {
                name: Some(text.to_string()),
                fill: None,
                wrap: false,
            });
            document.items.push(SequenceItem::BlockStart(SequenceBlock {
                kind: SequenceBlockKind::Loop,
                label: text.to_string(),
                wrap: false,
            }));
            document.items.push(SequenceItem::Note(SequenceNote {
                placement: SequenceNotePlacement::Over,
                from: "A".into(),
                to: "A".into(),
                text: text.to_string(),
                wrap: false,
            }));
            document.items.push(SequenceItem::BlockEnd);

            let emitted = emit_sequence(&document);
            let parsed = parse_sequence(&emitted)
                .unwrap_or_else(|error| panic!("`{text}` should parse: {error}\n{emitted}"));
            assert_eq!(parsed.participants, document.participants, "{emitted}");
            assert_eq!(parsed.boxes, document.boxes, "{emitted}");
            assert_eq!(parsed.items, document.items, "{emitted}");
            assert_renders(&emitted);
        }
    }

    #[test]
    fn autonumber_keeps_fractional_steps() {
        let document = parse_sequence("sequenceDiagram\n  autonumber 1.5 0.25\n  A->>B: x")
            .expect("source should parse");
        assert_eq!(
            document.items.first(),
            Some(&SequenceItem::Autonumber(SequenceAutonumber {
                start: Some(1.5),
                step: Some(0.25),
                visible: true,
            }))
        );
        assert_stable("sequenceDiagram\n  autonumber 1.5 0.25\n  A->>B: x");
    }

    #[test]
    fn a_wrap_prefix_survives() {
        let document =
            parse_sequence("sequenceDiagram\n  A->>B: wrap:長い文章\n  Note over A: nowrap:短い")
                .expect("source should parse");
        let Some(SequenceItem::Message(message)) = document.items.first() else {
            panic!("the first item should be a message");
        };
        assert!(message.wrap, "the wrap prefix should be recorded");
        assert_eq!(message.text, "長い文章");
        let emitted = assert_stable("sequenceDiagram\n  A->>B: wrap:長い文章");
        assert!(
            emitted.contains("wrap:長い文章"),
            "the prefix should be written back:\n{emitted}"
        );
    }

    /// A `%%{init}%%` directive that sets `wrap` makes the absence of a prefix
    /// mean the opposite of what it usually does, so every text has to say
    /// which it is — including a box name and a participant description.
    #[test]
    fn a_wrap_directive_makes_every_prefix_explicit() {
        for (setting, prefix) in [("true", "wrap:"), ("false", "nowrap:")] {
            let source = format!(
                "%%{{init: {{\"wrap\": {setting}}}}}%%\nsequenceDiagram\n  box G\n    participant A as アリス\n  end\n  A->>A: メッセージ\n  Note over A: メモ\n  loop 毎日\n    A->>A: x\n  end"
            );
            let emitted = assert_stable(&source);
            for line in [
                "box ",
                "participant A as ",
                "A->>A: ",
                "Note over A: ",
                "loop ",
            ] {
                assert!(
                    emitted.contains(&format!("{line}{prefix}")),
                    "`{line}` should carry `{prefix}`:\n{emitted}"
                );
            }
            assert_renders(&emitted);
        }
    }

    #[test]
    fn newlines_and_crlf_become_line_breaks() {
        let message = SequenceMessage {
            from: "A".into(),
            to: "B".into(),
            arrow: SequenceArrow::Solid,
            activate: false,
            deactivate: false,
            text: "1 行目\n2 行目".into(),
            wrap: false,
        };
        let source = format!(
            "sequenceDiagram\n{INDENT}{}",
            message_statement(&message, false)
        );
        assert!(source.contains("<br/>"), "{source}");
        let document = parse_sequence(&source).expect("source should parse");
        let Some(SequenceItem::Message(parsed)) = document.items.first() else {
            panic!("the first item should be a message");
        };
        assert_eq!(parsed.text, "1 行目<br/>2 行目");

        let crlf = "sequenceDiagram\r\n  participant A\r\n  A->>B: x\r\n";
        let document = parse_sequence(crlf).expect("CRLF source should parse");
        assert_eq!(document.participants.len(), 2, "{document:?}");
        assert_stable(crlf);
    }

    #[test]
    fn boxes_keep_their_colour_and_name() {
        for (source, fill, name) in [
            ("box 業務\n    participant A\n  end", None, Some("業務")),
            (
                "box rgb(200, 200, 255) 業務\n    participant A\n  end",
                Some("rgb(200, 200, 255)"),
                Some("業務"),
            ),
            ("box Aqua\n    participant A\n  end", Some("Aqua"), None),
            (
                "box transparent Gold\n    participant A\n  end",
                None,
                Some("Gold"),
            ),
            ("box\n    participant A\n  end", None, None),
        ] {
            let text = format!("sequenceDiagram\n  {source}\n  A->>A: x");
            let document = parse_sequence(&text)
                .unwrap_or_else(|error| panic!("`{source}` should parse: {error}"));
            let group = document
                .boxes
                .first()
                .unwrap_or_else(|| panic!("`{source}` should produce a box"));
            assert_eq!(group.fill.as_deref(), fill, "fill of `{source}`");
            assert_eq!(group.name.as_deref(), name, "name of `{source}`");
            let emitted = assert_stable(&text);
            assert_renders(&emitted);
        }
    }

    /// An empty `box` draws a group of its own, so it may not silently vanish.
    #[test]
    fn an_empty_box_survives() {
        let emitted = assert_stable("sequenceDiagram\n  box 空\n  end\n  A->>B: x");
        assert!(emitted.contains("box 空"), "{emitted}");
    }

    #[test]
    fn preserved_lines_keep_their_side_of_the_first_statement() {
        let source = "%%{init: {\"theme\": \"dark\"}}%%\nsequenceDiagram\n  %% 先頭のメモ\n  A->>B: x\n  %% 末尾のメモ\n  accTitle: 図の名前\n  links A: {\"詳細\": \"https://example.com\"}";
        let document = parse_sequence(source).expect("source should parse");
        let positions = document
            .preserved_lines
            .iter()
            .map(|line| (line.position, line.text.trim().to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![
                (
                    PreservedPosition::Preamble,
                    "%%{init: {\"theme\": \"dark\"}}%%".to_string()
                ),
                (PreservedPosition::Top, "%% 先頭のメモ".to_string()),
                (PreservedPosition::Bottom, "%% 末尾のメモ".to_string()),
                (PreservedPosition::Bottom, "accTitle: 図の名前".to_string()),
                (
                    PreservedPosition::Bottom,
                    "links A: {\"詳細\": \"https://example.com\"}".to_string()
                ),
            ]
        );

        let emitted = emit_sequence(&document);
        assert!(
            emitted
                .lines()
                .next()
                .unwrap_or_default()
                .starts_with("%%{init"),
            "the init directive must stay before the diagram keyword:\n{emitted}"
        );
        assert!(
            emitted
                .lines()
                .nth(1)
                .unwrap_or_default()
                .starts_with("sequenceDiagram"),
            "{emitted}"
        );
        assert_stable(source);
    }

    #[test]
    fn a_frontmatter_block_stays_ahead_of_the_keyword() {
        let source = "---\ntitle: 図の名前\n---\nsequenceDiagram\n  A->>B: x";
        let emitted = assert_stable(source);
        assert!(
            emitted.starts_with("---\ntitle: 図の名前\n---\n"),
            "{emitted}"
        );
        assert_eq!(
            parse_sequence(source).expect("source should parse").title,
            None,
            "a frontmatter title is preserved as text, not as the model's title"
        );
    }

    #[test]
    fn a_title_statement_is_written_once() {
        let emitted = assert_stable("sequenceDiagram\n  title: 認証の流れ\n  A->>B: x");
        assert_eq!(
            emitted.matches("認証の流れ").count(),
            1,
            "the title must not be written twice:\n{emitted}"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn the_declaration_order_of_participants_is_kept() {
        let document = parse_sequence("sequenceDiagram\n  participant C\n  A->>B: x\n  C->>A: y")
            .expect("source should parse");
        let ids = document
            .participants
            .iter()
            .map(|participant| participant.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["C", "A", "B"]);
        let emitted = assert_stable("sequenceDiagram\n  participant C\n  A->>B: x\n  C->>A: y");
        let first = emitted
            .lines()
            .position(|line| line.trim() == "participant C")
            .expect("C should be declared");
        let second = emitted
            .lines()
            .position(|line| line.trim() == "participant A")
            .expect("A should be declared");
        assert!(
            first < second,
            "the column order must not change:\n{emitted}"
        );
    }

    /// Mermaid also separates statements with `;` and comments with `#`, both
    /// of which the line scanner has to survive.
    #[test]
    fn semicolons_and_hash_comments_are_handled() {
        let document = parse_sequence("sequenceDiagram\n  A->>B: x;B->>A: y")
            .expect("semicolon separated statements should parse");
        assert_eq!(document.items.len(), 2, "{document:?}");
        let emitted = assert_stable("sequenceDiagram\n  A->>B: x;B->>A: y");
        assert_eq!(
            emitted.lines().count(),
            5,
            "the two statements should each get a line:\n{emitted}"
        );
        assert_renders(&emitted);

        let emitted = assert_stable("sequenceDiagram\n  # 覚え書き\n  A->>B: x");
        assert!(
            emitted.contains("# 覚え書き"),
            "a hash comment line should survive:\n{emitted}"
        );
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_diagram() {
        let document = parse_sequence("").expect("an empty fence should parse");
        assert!(document.participants.is_empty());
        assert!(document.items.is_empty());
        assert_eq!(emit_sequence(&document), "sequenceDiagram");
        assert_eq!(emit_sequence(&SequenceDocument::empty()), "sequenceDiagram");
        assert_stable("sequenceDiagram");
    }

    #[test]
    fn other_diagram_types_are_refused() {
        for source in [
            "flowchart TD\n  A --> B",
            "pie title x\n  \"a\" : 1",
            "erDiagram\n  A ||--|| B : x",
        ] {
            assert!(!is_sequence_source(source), "{source}");
            assert!(parse_sequence(source).is_err(), "{source}");
        }
        assert!(is_sequence_source("sequenceDiagram\n  A->>B: x"));
        assert!(is_sequence_source("%% note\nsequenceDiagram\n  A->>B: x"));
        assert!(is_sequence_source("---\ntitle: x\n---\nsequenceDiagram"));
    }

    /// Spellings that carry information neither the model nor the line scanner
    /// holds. Opening the editor on one and writing it back would drop it, so
    /// the parser refuses instead.
    #[test]
    fn constructs_that_would_be_dropped_are_refused() {
        for source in [
            "sequenceDiagram\n  A->>B: x %% 行末のメモ",
            "sequenceDiagram\n  A->>B: x # 行末のメモ",
            // A config object over several lines has no single line to keep.
            "sequenceDiagram\n  participant A@{\n    \"type\": \"actor\"\n  }\n  A->>B: x",
            // `create` declares its column where the statement stands, so the
            // top of the diagram can no longer declare `A` after it.
            "sequenceDiagram\n  create participant C\n  A->>C: x",
            // Only the second of the two `destroy` statements would survive.
            "sequenceDiagram\n  A->>B: x\n  destroy B\n  A->>B: y\n  destroy B\n  A->>B: z",
            // `;` ends a statement too, so the same two `destroy`s fit on one
            // line and have to be found there as well.
            "sequenceDiagram\n  A->>B: x;destroy B;A->>B: y;destroy B;A->>B: z",
        ] {
            let error = parse_sequence(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains("テキストのまま編集してください"),
                "unexpected message for `{source}`: {message}"
            );
        }

        // The same characters inside a message text are the user's words.
        for source in [
            "sequenceDiagram\n  A->>B: 引用 #quot; あり",
            "sequenceDiagram\n  A->>B: 100#37; 完了",
            "sequenceDiagram\n  Note over A: メール a#64;b.example",
        ] {
            assert!(
                parse_sequence(source).is_ok(),
                "`{source}` should still open"
            );
        }
    }

    #[test]
    fn participant_ids_are_validated() {
        assert!(is_valid_participant_id("A"));
        assert!(is_valid_participant_id("app_1"));
        assert!(is_valid_participant_id("アプリ"));
        assert!(!is_valid_participant_id(""));
        assert!(!is_valid_participant_id("a b"));
        assert!(!is_valid_participant_id("a:b"));
        assert!(!is_valid_participant_id("a,b"));
        assert!(!is_valid_participant_id("a+b"));
        assert!(!is_valid_participant_id("a-->b"));
        assert!(!is_valid_participant_id("007"));
        assert!(!is_valid_participant_id("a@b"));
        assert!(!is_valid_participant_id("-a"));
        assert!(!is_valid_participant_id("a-"));
    }

    /// The editor's rule is stricter than what Mermaid can read back, so a
    /// diagram that already names a participant with one of those characters
    /// still opens and still round-trips.
    #[test]
    fn ids_the_editor_would_not_offer_still_round_trip() {
        for id in ["a@b", "a%b", "a\"b", "a>b", "a(b)", "A-"] {
            assert!(
                is_writable_participant_id(id),
                "`{id}` should be writable back"
            );
            let mut document = SequenceDocument::empty();
            for name in [id, "Z"] {
                document.participants.push(SequenceParticipant {
                    id: name.to_string(),
                    alias: None,
                    kind: SequenceParticipantKind::Participant,
                    wrap: false,
                    box_index: None,
                    preserved_settings: None,
                });
            }
            document.items.push(SequenceItem::Message(SequenceMessage {
                from: id.to_string(),
                to: "Z".into(),
                arrow: SequenceArrow::Solid,
                activate: false,
                deactivate: false,
                text: "x".into(),
                wrap: false,
            }));
            document.items.push(SequenceItem::Note(SequenceNote {
                placement: SequenceNotePlacement::Over,
                from: id.to_string(),
                to: "Z".into(),
                text: "メモ".into(),
                wrap: false,
            }));
            assert_eq!(document.structure_error(), None, "`{id}`");
            let emitted = emit_sequence(&document);
            let parsed = parse_sequence(&emitted)
                .unwrap_or_else(|error| panic!("`{id}`: {error}\n{emitted}"));
            assert_eq!(parsed.participants, document.participants, "{emitted}");
            assert_eq!(parsed.items, document.items, "{emitted}");
        }

        for id in [
            "", "-a", "a b", "a:b", "a,b", "a+b", "a;b", "a<b", "a@{b", "a()", "a%%b", "007",
            "a-->b",
        ] {
            assert!(!is_writable_participant_id(id), "`{id}` should be refused");
        }
    }

    /// Ids the editor accepts must survive as themselves, which is the whole
    /// point of the check above.
    #[test]
    fn accepted_ids_round_trip() {
        for id in ["A", "app_1", "アプリ", "a.b", "a-b", "end", "note", "loop"] {
            assert!(is_valid_participant_id(id), "`{id}` should be accepted");
            let source = format!("sequenceDiagram\n{INDENT}{id}->>{id}: x");
            let document = parse_sequence(&source)
                .unwrap_or_else(|error| panic!("`{id}` should parse: {error}\n{source}"));
            assert_eq!(
                document
                    .participants
                    .first()
                    .map(|participant| participant.id.as_str()),
                Some(id),
                "{source}"
            );
            assert_stable(&source);
        }
    }

    #[test]
    fn unused_participant_id_skips_taken_names() {
        let mut document = SequenceDocument::empty();
        for id in ["P", "P1"] {
            document.participants.push(SequenceParticipant {
                id: id.into(),
                alias: None,
                kind: SequenceParticipantKind::Participant,
                wrap: false,
                box_index: None,
                preserved_settings: None,
            });
        }
        assert_eq!(document.unused_participant_id("P"), "P2");
        assert_eq!(document.unused_participant_id("Q"), "Q");
    }

    #[test]
    fn structure_error_catches_documents_mermaid_would_reject() {
        let mut document = SequenceDocument::empty();
        document.items.push(SequenceItem::BlockEnd);
        assert!(
            document
                .structure_error()
                .is_some_and(|message| message.contains("end")),
            "an unmatched end should be reported"
        );

        let mut document = SequenceDocument::empty();
        document.items.push(SequenceItem::BlockStart(SequenceBlock {
            kind: SequenceBlockKind::Loop,
            label: "x".into(),
            wrap: false,
        }));
        assert!(
            document.structure_error().is_some(),
            "an unclosed block should be reported"
        );

        let mut document = SequenceDocument::empty();
        document.items.push(SequenceItem::BlockStart(SequenceBlock {
            kind: SequenceBlockKind::Loop,
            label: "x".into(),
            wrap: false,
        }));
        document
            .items
            .push(SequenceItem::BlockBranch(SequenceBranch {
                kind: SequenceBranchKind::Else,
                label: "y".into(),
                wrap: false,
            }));
        document.items.push(SequenceItem::BlockEnd);
        assert!(
            document
                .structure_error()
                .is_some_and(|message| message.contains("else")),
            "`else` does not belong to a loop"
        );

        let mut document = SequenceDocument::empty();
        document
            .items
            .push(SequenceItem::Deactivate("A".to_string()));
        assert!(
            document.structure_error().is_some(),
            "an undeclared participant should be reported"
        );

        let mut document = SequenceDocument::empty();
        document.participants.push(SequenceParticipant {
            id: "A".into(),
            alias: None,
            kind: SequenceParticipantKind::Participant,
            wrap: false,
            box_index: None,
            preserved_settings: None,
        });
        document
            .items
            .push(SequenceItem::Deactivate("A".to_string()));
        assert!(
            document.structure_error().is_some(),
            "deactivating an inactive participant should be reported"
        );
        document
            .items
            .insert(0, SequenceItem::Activate("A".to_string()));
        assert_eq!(document.structure_error(), None);
    }

    /// A document the editor built by hand must still emit source Mermaid
    /// accepts, which is what the checks above are for.
    #[test]
    fn a_document_built_by_hand_emits_valid_source() {
        let mut document = SequenceDocument::empty();
        document.title = Some("手で組んだ図".into());
        for (id, alias, kind) in [
            ("User", Some("利用者"), SequenceParticipantKind::Actor),
            ("App", None, SequenceParticipantKind::Participant),
        ] {
            document.participants.push(SequenceParticipant {
                id: id.into(),
                alias: alias.map(str::to_string),
                kind,
                wrap: false,
                box_index: None,
                preserved_settings: None,
            });
        }
        document.items.push(SequenceItem::Message(SequenceMessage {
            from: "User".into(),
            to: "App".into(),
            arrow: SequenceArrow::Solid,
            activate: true,
            deactivate: false,
            text: "要求 ; と # を含む".into(),
            wrap: false,
        }));
        document.items.push(SequenceItem::Message(SequenceMessage {
            from: "App".into(),
            to: "User".into(),
            arrow: SequenceArrow::Dotted,
            activate: false,
            deactivate: true,
            text: String::new(),
            wrap: false,
        }));
        assert_eq!(document.structure_error(), None);

        let emitted = emit_sequence(&document);
        let parsed = parse_sequence(&emitted).unwrap_or_else(|error| {
            panic!("a hand built document should parse: {error}\n{emitted}")
        });
        assert_eq!(parsed.title.as_deref(), Some("手で組んだ図"));
        assert_eq!(parsed.participants, document.participants);
        assert_eq!(parsed.items, document.items);
        assert_renders(&emitted);
    }

    /// `participant A-` is a legal declaration, and `A-` written straight
    /// against `->>` would read as `A` followed by `-->>`.
    #[test]
    fn an_id_ending_in_a_dash_keeps_its_arrow() {
        let source = "sequenceDiagram\n  participant A-\n  participant B\n  A- ->>B: x";
        let document = parse_sequence(source).expect("source should parse");
        let emitted = assert_stable(source);
        let Some(SequenceItem::Message(message)) = document.items.first() else {
            panic!("the first item should be a message");
        };
        assert_eq!(message.from, "A-");
        assert_eq!(message.arrow, SequenceArrow::Solid);
        assert_eq!(
            document.participants.len(),
            2,
            "no third participant may appear:\n{emitted}"
        );
        assert_renders(&emitted);
    }

    /// A `box` wraps a run of declarations, so a diagram whose box members are
    /// not neighbours in the column order cannot be written back. Re-declaring
    /// an already mentioned participant inside a box is the way to get one.
    #[test]
    fn a_box_that_would_move_a_column_is_refused() {
        let source =
            "sequenceDiagram\n  A->>C: hi\n  box Group\n  participant A\n  participant B\n  end";
        let error = parse_sequence(source)
            .err()
            .unwrap_or_else(|| panic!("`{source}` should be refused"));
        assert!(
            format!("{error}").contains("テキストのまま編集してください"),
            "{error}"
        );

        let mut document = SequenceDocument::empty();
        for (id, box_index) in [("A", Some(0)), ("B", None), ("C", Some(0))] {
            document.participants.push(SequenceParticipant {
                id: id.into(),
                alias: None,
                kind: SequenceParticipantKind::Participant,
                wrap: false,
                box_index,
                preserved_settings: None,
            });
        }
        document.boxes.push(SequenceGroupBox {
            name: Some("G".into()),
            fill: None,
            wrap: false,
        });
        assert!(
            document.structure_error().is_some(),
            "a box whose members are not neighbours should be reported"
        );
    }

    /// The `-` suffix deactivates the sender of its own arrow, so a
    /// `deactivate` that names anybody else has to stay a statement of its own.
    #[test]
    fn a_deactivation_only_folds_into_the_arrow_before_it() {
        let source = "sequenceDiagram\n  participant A\n  participant B\n  activate A\n  activate B\n  A->>B: x\n  deactivate B\n  deactivate A";
        let document = parse_sequence(source).expect("source should parse");
        let deactivated = document
            .items
            .iter()
            .filter_map(|item| match item {
                SequenceItem::Deactivate(id) => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deactivated, vec!["B", "A"], "{:?}", document.items);
        let emitted = assert_stable(source);
        assert!(
            !emitted.contains("A->>-B"),
            "the deactivation of B may not become the sender's own:\n{emitted}"
        );
        assert_renders(&emitted);

        // A note, and a block boundary, between the arrow and the statement.
        for source in [
            "sequenceDiagram\n  A->>+B: x\n  B->>A: y\n  Note over A: メモ\n  deactivate B",
            "sequenceDiagram\n  A->>+B: x\n  loop 毎回\n    B->>A: y\n  end\n  deactivate B",
        ] {
            let document = parse_sequence(source).expect("source should parse");
            assert!(
                matches!(document.items.last(), Some(SequenceItem::Deactivate(id)) if id == "B"),
                "{:?}",
                document.items
            );
            assert_renders(&assert_stable(source));
        }
    }

    #[test]
    fn further_upstream_examples_round_trip_and_render() {
        for source in [
            "sequenceDiagram\n  loop 毎日\n    alt 平日\n      A->>B: 出社\n    else 休日\n      A->>B: 休む\n    end\n  end",
            "sequenceDiagram\n  par A へ\n    X->>A: 1\n  and B へ\n    X->>B: 2\n  and C へ\n    X->>C: 3\n  end",
            "sequenceDiagram\n  critical 接続\n    S->>D: 接続\n  option 切断\n    S->>S: 記録\n  option 停止\n    S->>S: 待機\n  end",
            "sequenceDiagram\n  A->>B: x\n  break 失敗した\n    B->>A: 理由\n  end",
            "sequenceDiagram\n  rect rgb(191, 223, 255)\n    A->>B: x\n    rect rgba(200, 150, 255, 0.5)\n      B->>A: y\n    end\n  end",
            "sequenceDiagram\n  Note right of A: 1 行目<br/>2 行目",
            "sequenceDiagram\n  participant 利用者-1\n  participant A2\n  利用者-1->>A2: こんにちは",
            "sequenceDiagram\n  A->>B: 割合 50% と #35; と \"引用\" と #59; と : コロン",
            "sequenceDiagram\n  actor A\n  participant B as \"引用付きの別名\"\n  A->>B: x",
            "sequenceDiagram\n  autonumber 1 1\n  A->>B: x\n  autonumber\n  B->>A: y",
            "sequenceDiagram\n  A->>+B: x\n  B->>+C: y\n  C-->>-B: z\n  B-->>-A: w",
        ] {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    /// `structure_error` is the editor's only gate before it writes the fence
    /// back, so an id Mermaid could not read has to fail it.
    #[test]
    fn structure_error_catches_ids_the_editor_could_not_write() {
        for id in ["", "a b", "a:b", "-A", "007"] {
            let mut document = SequenceDocument::empty();
            document.participants.push(SequenceParticipant {
                id: id.to_string(),
                alias: None,
                kind: SequenceParticipantKind::Participant,
                wrap: false,
                box_index: None,
                preserved_settings: None,
            });
            assert!(
                document.structure_error().is_some(),
                "`{id}` should be reported, emitted:\n{}",
                emit_sequence(&document)
            );
        }

        let mut document = SequenceDocument::empty();
        for _ in 0..2 {
            document.participants.push(SequenceParticipant {
                id: "A".into(),
                alias: None,
                kind: SequenceParticipantKind::Participant,
                wrap: false,
                box_index: None,
                preserved_settings: None,
            });
        }
        assert!(
            document.structure_error().is_some(),
            "a duplicate id should be reported, emitted:\n{}",
            emit_sequence(&document)
        );

        // A dangling box index must be reported rather than lose the column.
        let mut document = SequenceDocument::empty();
        document.participants.push(SequenceParticipant {
            id: "A".into(),
            alias: None,
            kind: SequenceParticipantKind::Participant,
            wrap: false,
            box_index: Some(7),
            preserved_settings: None,
        });
        assert!(document.structure_error().is_some());
        assert!(
            emit_sequence(&document).contains("participant A"),
            "the participant may not vanish with its box"
        );
    }

    /// The other half of the same contract: whatever the parser accepts, the
    /// editor can write back.
    #[test]
    fn parse_never_yields_a_document_it_could_not_write_back() {
        for source in [
            "sequenceDiagram\n  A->>B: x\n  deactivate B",
            "sequenceDiagram\n  A->>-B: x",
            "sequenceDiagram\n  activate A\n  A->>B: x",
            "sequenceDiagram\n  box G\n  participant A\n  end\n  box H\n  participant B\n  end\n  A->>B: x",
            "sequenceDiagram\n  participant -A\n  -A->>B: x",
            "sequenceDiagram\n  loop 閉じ忘れ\n    A->>B: x",
            "sequenceDiagram\n  else 単独",
            "sequenceDiagram\n  Note over A",
            "sequenceDiagram\n  A->>B:",
            "sequenceDiagram\n  😀->>😁: 絵文字",
            "sequenceDiagram\n  A->>B: #",
            "sequenceDiagram\n  A->>B: %",
            "sequenceDiagram\n  %%{init: {\"theme\": \"dark\"}",
            "sequenceDiagram\n  links A: {",
            "---\ntitle: x",
        ] {
            if let Ok(document) = parse_sequence(source) {
                assert_eq!(
                    document.structure_error(),
                    None,
                    "parsed but not writable back: {source}"
                );
                parse_sequence(&emit_sequence(&document)).unwrap_or_else(|error| {
                    panic!("`{source}` emitted source Mermaid rejects: {error}")
                });
            }
        }
    }

    fn participant(id: &str, box_index: Option<usize>) -> SequenceParticipant {
        SequenceParticipant {
            id: id.to_string(),
            alias: None,
            kind: SequenceParticipantKind::Participant,
            wrap: false,
            box_index,
            preserved_settings: None,
        }
    }

    fn message(from: &str, to: &str, text: &str) -> SequenceItem {
        SequenceItem::Message(SequenceMessage {
            from: from.to_string(),
            to: to.to_string(),
            arrow: SequenceArrow::Solid,
            activate: false,
            deactivate: false,
            text: text.to_string(),
            wrap: false,
        })
    }

    /// Where a `create` / `destroy` sits between the messages is the whole
    /// point of the statement, so the item list has to carry it in place.
    #[test]
    fn create_and_destroy_keep_their_place() {
        let source = "sequenceDiagram\n    アリス->>ボブ: こんにちは\n    create participant カール\n    アリス->>カール: はじめまして\n    create actor D as ドナルド\n    カール->>D: どうも\n    destroy カール\n    アリス-xカール: 人が多すぎます\n    destroy ボブ\n    ボブ->>アリス: 賛成です";
        let document = parse_sequence(source).expect("source should parse");
        let lifecycle = document
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match item {
                SequenceItem::Create(id) => Some((index, "create", id.as_str())),
                SequenceItem::Destroy(id) => Some((index, "destroy", id.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lifecycle,
            vec![
                (1, "create", "カール"),
                (3, "create", "D"),
                (5, "destroy", "カール"),
                (7, "destroy", "ボブ"),
            ],
            "a `create` belongs between the messages it sat between"
        );
        assert_eq!(
            document
                .participants
                .iter()
                .map(|participant| participant.id.as_str())
                .collect::<Vec<_>>(),
            vec!["アリス", "ボブ", "カール", "D"]
        );

        let emitted = assert_stable(source);
        assert!(emitted.contains("create participant カール"), "{emitted}");
        assert!(emitted.contains("create actor D as ドナルド"), "{emitted}");
        assert!(emitted.contains("destroy ボブ"), "{emitted}");
        // A `create`d column is declared by its own statement, and Mermaid
        // refuses a second declaration of the same id outright.
        assert!(
            !emitted.contains("\n    participant カール"),
            "the created column was declared twice:\n{emitted}"
        );
        assert_renders(&emitted);
    }

    /// Opening a diagram that already holds `create` and confirming it without
    /// touching anything must leave the fence byte for byte as it was.
    #[test]
    fn a_create_diagram_confirmed_unchanged_is_left_alone() {
        let source = "sequenceDiagram\n  アリス->>ボブ: 依頼\n  create participant カール\n  アリス->>カール: 追加\n  destroy ボブ\n  ボブ->>アリス: 退出";
        let canonical = emit_sequence(&parse_sequence(source).expect("source should parse"));
        assert_eq!(
            canonical,
            "sequenceDiagram\n    participant アリス\n    participant ボブ\n    アリス->>ボブ: 依頼\n    create participant カール\n    アリス->>カール: 追加\n    destroy ボブ\n    ボブ->>アリス: 退出"
        );
        let again = emit_sequence(&parse_sequence(&canonical).expect("the fence should reopen"));
        assert_eq!(
            again, canonical,
            "confirming an unedited diagram rewrote it"
        );
        assert_renders(&canonical);
    }

    /// The editor's "参加者の作成" button writes the statement before there is
    /// a message for it, so the half-finished diagram still has to draw.
    #[test]
    fn a_create_with_no_message_yet_still_draws() {
        let source = "sequenceDiagram\n    participant A\n    A->>A: x\n    create participant C";
        let document = parse_sequence(source).expect("source should parse");
        assert_eq!(document.structure_error(), None);
        let emitted = assert_stable(source);
        assert!(emitted.contains("create participant C"), "{emitted}");
        assert_renders(&emitted);
    }

    /// `merman` reports the two lifecycle statements as two tables keyed by
    /// participant, so the row order says nothing about which came first and
    /// only the positions do.
    #[test]
    fn a_destroy_before_a_create_keeps_the_two_in_order() {
        let source = "sequenceDiagram\n    participant A\n    participant B\n    A->>B: 依頼\n    destroy B\n    A-xB: 終了\n    create participant C\n    A->>C: 開始";
        let document = parse_sequence(source).expect("source should parse");
        assert_eq!(
            document
                .items
                .iter()
                .filter_map(|item| match item {
                    SequenceItem::Create(id) => Some(("create", id.as_str())),
                    SequenceItem::Destroy(id) => Some(("destroy", id.as_str())),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![("destroy", "B"), ("create", "C")]
        );
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        assert_renders(&emitted);
    }

    /// A `deactivate` right after a message is normally written back as the
    /// arrow's own `-`. A `create` standing between the two is what says when
    /// the new column appears, so the fold would move the `deactivate` past it.
    #[test]
    fn a_lifecycle_statement_stops_a_deactivate_folding_into_the_arrow() {
        for source in [
            "sequenceDiagram\n    participant A\n    participant B\n    activate A\n    A->>B: x\n    create participant C\n    deactivate A\n    A->>C: y",
            "sequenceDiagram\n    participant A\n    participant B\n    activate A\n    A->>B: x\n    destroy B\n    deactivate A\n    A-xB: y",
        ] {
            let emitted = assert_stable(source);
            assert_eq!(emitted, source, "the fold moved a statement");
            assert_renders(&emitted);
        }

        // With nothing in between, the fold is still what the item list wants.
        let folded = assert_stable(
            "sequenceDiagram\n    participant A\n    participant B\n    activate A\n    A->>B: x\n    deactivate A",
        );
        assert!(folded.contains("A->>-B: x"), "{folded}");
    }

    /// The top of the diagram is written before any statement, so a column that
    /// only appears after a `create` cannot be declared there without changing
    /// which column is where.
    #[test]
    fn a_column_first_used_after_a_create_is_refused() {
        for source in [
            "sequenceDiagram\n  create participant C\n  A->>C: x",
            "sequenceDiagram\n  A->>B: x\n  create participant C\n  B->>C: y\n  B->>D: z",
        ] {
            let error = parse_sequence(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains("create"),
                "unexpected message for `{source}`: {message}"
            );
        }
    }

    /// The rules Mermaid itself enforces around `create` / `destroy`, which the
    /// editor has to apply before it writes rather than after.
    #[test]
    fn structure_error_catches_create_and_destroy_mermaid_would_reject() {
        let base = |items: Vec<SequenceItem>| SequenceDocument {
            title: None,
            participants: vec![participant("A", None), participant("C", None)],
            boxes: Vec::new(),
            items,
            preserved_lines: Vec::new(),
        };

        assert_eq!(
            base(vec![
                SequenceItem::Create("C".into()),
                message("A", "C", "作成"),
            ])
            .structure_error(),
            None,
            "the documented shape of a `create` should be accepted"
        );

        for (items, expected) in [
            (
                vec![SequenceItem::Create("C".into()), message("A", "A", "別件")],
                "`create C` の次のメッセージは `C` 宛てでなければなりません。",
            ),
            (
                vec![
                    SequenceItem::Create("C".into()),
                    message("A", "C", "作成"),
                    SequenceItem::Destroy("C".into()),
                    message("A", "A", "別件"),
                ],
                "`destroy C` の次のメッセージは `C` が関わるものでなければなりません。",
            ),
            (
                vec![
                    message("A", "C", "先に登場"),
                    SequenceItem::Create("C".into()),
                ],
                "`C` は `create` より前に登場しています。`create` を先に置いてください。",
            ),
            (
                vec![
                    SequenceItem::Create("C".into()),
                    message("A", "C", "作成"),
                    SequenceItem::Create("C".into()),
                ],
                "参加者 `C` が二重に `create` されています。",
            ),
            (
                vec![SequenceItem::Destroy("X".into())],
                "参加者 `X` が宣言されていません。",
            ),
        ] {
            assert_eq!(
                base(items).structure_error().as_deref(),
                Some(expected),
                "unexpected reason"
            );
        }

        // A created column may not come before a declared one, and it may not
        // sit in a `box` either: both would need a second declaration.
        let document = base(vec![
            SequenceItem::Create("A".into()),
            message("C", "A", "作成"),
        ]);
        assert_eq!(
            document.structure_error().as_deref(),
            Some("`create` で作られる参加者より後ろに `C` を並べることはできません。")
        );

        let mut document = base(vec![
            SequenceItem::Create("C".into()),
            message("A", "C", "作成"),
        ]);
        document.boxes.push(SequenceGroupBox {
            name: Some("業務".into()),
            fill: None,
            wrap: false,
        });
        if let Some(created) = document.participants.get_mut(1) {
            created.box_index = Some(0);
        }
        assert_eq!(
            document.structure_error().as_deref(),
            Some("`create` で作られる参加者 `C` は box に入れられません。")
        );
    }

    /// Two `create`s whose columns are ordered the other way round would swap
    /// the columns when they are written back.
    #[test]
    fn the_order_of_created_columns_has_to_match_the_statements() {
        let mut document = SequenceDocument {
            title: None,
            participants: vec![
                participant("A", None),
                participant("C", None),
                participant("D", None),
            ],
            boxes: Vec::new(),
            items: vec![
                SequenceItem::Create("C".into()),
                message("A", "C", "先"),
                SequenceItem::Create("D".into()),
                message("A", "D", "後"),
            ],
            preserved_lines: Vec::new(),
        };
        assert_eq!(document.structure_error(), None);
        document.participants.swap(1, 2);
        assert_eq!(
            document.structure_error().as_deref(),
            Some(
                "`create` の順番と参加者の並び順が食い違っています。`create` した順に並べてください。"
            )
        );
    }

    /// A column that joins a `box` joins the run of declarations the box is
    /// written as, and everybody else keeps their place.
    #[test]
    fn a_box_keeps_its_members_and_their_order() {
        let source = "sequenceDiagram\n  box 業務システム\n    participant アプリ\n  end\n  participant 利用者\n  利用者->>アプリ: 要求";
        let mut document = parse_sequence(source).expect("source should parse");
        // Where the editor puts a column whose group changed: next to the
        // member the box already has.
        document
            .participants
            .insert(1, participant("台帳", Some(0)));
        document.items.push(message("アプリ", "台帳", "問い合わせ"));
        assert_eq!(document.structure_error(), None);

        let emitted = emit_sequence(&document);
        let reopened = parse_sequence(&emitted).expect("emitted source should parse");
        assert_eq!(
            reopened
                .participants
                .iter()
                .map(|participant| (participant.id.as_str(), participant.box_index))
                .collect::<Vec<_>>(),
            vec![("アプリ", Some(0)), ("台帳", Some(0)), ("利用者", None)],
            "{emitted}"
        );
        assert_eq!(
            reopened
                .boxes
                .first()
                .and_then(|group| group.name.as_deref()),
            Some("業務システム"),
            "{emitted}"
        );
        assert_eq!(emit_sequence(&reopened), emitted);
        assert_renders(&emitted);
    }

    /// A `box` the editor emptied still draws a group of its own, so it stays
    /// in the source instead of vanishing under the user.
    #[test]
    fn a_box_emptied_by_the_editor_stays_in_the_source() {
        let source = "sequenceDiagram\n  box 業務システム\n    participant アプリ\n  end\n  participant 利用者\n  利用者->>アプリ: 要求";
        let mut document = parse_sequence(source).expect("source should parse");
        for participant in &mut document.participants {
            participant.box_index = None;
        }
        assert_eq!(document.structure_error(), None);

        let emitted = emit_sequence(&document);
        assert!(emitted.contains("box 業務システム"), "{emitted}");
        let reopened = parse_sequence(&emitted).expect("emitted source should parse");
        assert_eq!(reopened.boxes.len(), 1, "{emitted}");
        assert!(
            reopened
                .participants
                .iter()
                .all(|participant| participant.box_index.is_none()),
            "{emitted}"
        );
        assert_renders(&emitted);
    }

    /// Mermaid reads the first word of a `box` header as the group's color, so
    /// a group called "Gold" has to survive every fill the editor offers.
    #[test]
    fn a_box_named_after_a_colour_survives_every_fill() {
        for (fill, label) in BOX_FILL_PRESETS {
            assert_eq!(box_fill_label(fill), label);
            let mut document = SequenceDocument {
                title: None,
                participants: vec![participant("A", Some(0)), participant("B", Some(0))],
                boxes: vec![SequenceGroupBox {
                    name: Some("Gold".into()),
                    fill: fill.map(str::to_string),
                    wrap: false,
                }],
                items: vec![message("A", "B", "x")],
                preserved_lines: Vec::new(),
            };
            assert_eq!(document.structure_error(), None);
            let emitted = emit_sequence(&document);
            let reopened = parse_sequence(&emitted)
                .unwrap_or_else(|error| panic!("`{emitted}` should parse: {error}"));
            let group = reopened
                .boxes
                .first()
                .unwrap_or_else(|| panic!("the box should survive:\n{emitted}"));
            assert_eq!(group.name.as_deref(), Some("Gold"), "{emitted}");
            assert_eq!(group.fill.as_deref(), fill, "{emitted}");
            assert_renders(&emitted);

            // A fill written by hand is not one of the presets, and the label
            // falls back to the color itself rather than to an empty menu row.
            document.boxes.clear();
            document.boxes.push(SequenceGroupBox {
                name: Some("Gold".into()),
                fill: Some("rgb(200, 200, 255)".into()),
                wrap: false,
            });
            assert_eq!(
                box_fill_label(Some("rgb(200, 200, 255)")),
                "rgb(200, 200, 255)"
            );
            let emitted = emit_sequence(&document);
            let reopened = parse_sequence(&emitted)
                .unwrap_or_else(|error| panic!("`{emitted}` should parse: {error}"));
            assert_eq!(
                reopened
                    .boxes
                    .first()
                    .and_then(|group| group.name.as_deref()),
                Some("Gold"),
                "{emitted}"
            );
        }
    }

    fn ids(document: &SequenceDocument) -> Vec<&str> {
        document
            .participants
            .iter()
            .map(|participant| participant.id.as_str())
            .collect()
    }

    /// `participant A@{ … }` is Mermaid 11's way of choosing a shape. `merman`
    /// keeps only the `type`, so the object's own text has to come back from
    /// the source, byte for byte.
    #[test]
    fn a_config_object_is_kept_as_written() {
        let source = "sequenceDiagram\n    participant A@{ \"type\": \"boundary\" }\n    participant B@{type: database}\n    participant C\n    A->>B: 問い合わせ\n    B-->>C: 結果";
        let document = parse_sequence(source).expect("a config object should open");
        assert_eq!(
            document
                .participants
                .iter()
                .map(|participant| participant.preserved_settings.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some(" \"type\": \"boundary\" "),
                Some("type: database"),
                None
            ]
        );
        assert!(
            document
                .participants
                .iter()
                .all(|participant| participant.alias.is_none()
                    && participant.kind == SequenceParticipantKind::Participant),
            "{:?}",
            document.participants
        );
        let emitted = assert_stable(source);
        assert_eq!(emitted, source, "an unedited diagram should not move");
        assert_renders(&emitted);
    }

    /// The object belongs to the column, so renaming the column carries it
    /// along — and so does a `create` statement that declares it.
    #[test]
    fn a_config_object_follows_its_column() {
        let source = "sequenceDiagram\n    participant A@{ \"type\": \"queue\" }\n    participant B\n    A->>B: x\n    create participant C@{ \"type\": \"database\" }\n    B->>C: 作成";
        let mut document = parse_sequence(source).expect("source should parse");
        assert_eq!(
            document
                .participants
                .get(2)
                .and_then(|participant| participant.preserved_settings.as_deref()),
            Some(" \"type\": \"database\" ")
        );
        assert_eq!(emit_sequence(&document), source);

        if let Some(participant) = document.participants.first_mut() {
            participant.id = "Q".into();
        }
        if let Some(SequenceItem::Message(message)) = document.items.first_mut() {
            message.from = "Q".into();
        }
        assert_eq!(document.structure_error(), None);
        let emitted = emit_sequence(&document);
        assert!(
            emitted.contains("participant Q@{ \"type\": \"queue\" }\n"),
            "{emitted}"
        );
        assert!(
            emitted.contains("create participant C@{ \"type\": \"database\" }\n"),
            "{emitted}"
        );
        assert_eq!(
            emit_sequence(&parse_sequence(&emitted).expect("reopens")),
            emitted
        );
        assert_renders(&emitted);
    }

    /// The `;` closing a `#…;` shorthand does not end the statement, so the
    /// object after such an id still belongs to it, and a second `destroy` of
    /// that id is still seen as one.
    #[test]
    fn a_config_object_after_an_id_with_an_entity_is_kept() {
        let source = "sequenceDiagram\n    participant A#59;B@{ \"type\": \"database\" }\n    participant C\n    C->>A#59;B: 問い合わせ";
        let document = parse_sequence(source).expect("the object should open");
        assert_eq!(
            document
                .participants
                .first()
                .and_then(|participant| participant.preserved_settings.as_deref()),
            Some(" \"type\": \"database\" ")
        );
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        assert_renders(&emitted);

        let error = parse_sequence(
            "sequenceDiagram\n    participant A\n    A->>X#59;Y: x\n    destroy X#59;Y; A-xX#59;Y: y\n    destroy X#59;Y\n    A-xX#59;Y: z",
        )
        .err()
        .expect("a second destroy should be refused");
        assert!(format!("{error}").contains("2 回"), "{error}");
    }

    /// Mermaid has no spelling for an object together with `actor` or `as`, and
    /// an object that spans lines is not one the line scanner can carry.
    #[test]
    fn config_objects_that_cannot_be_written_are_refused() {
        let error = parse_sequence(
            "sequenceDiagram\n    participant A@{\n      \"type\": \"database\"\n    }\n    A->>A: x",
        )
        .err()
        .expect("a config object over several lines should be refused");
        assert!(
            format!("{error}").contains("複数行"),
            "unexpected message: {error}"
        );

        let base = SequenceDocument {
            title: None,
            participants: vec![SequenceParticipant {
                preserved_settings: Some(" \"type\": \"boundary\" ".into()),
                ..participant("A", None)
            }],
            boxes: Vec::new(),
            items: vec![message("A", "A", "x")],
            preserved_lines: Vec::new(),
        };
        assert_eq!(base.structure_error(), None);

        let mut document = base.clone();
        if let Some(participant) = document.participants.first_mut() {
            participant.alias = Some("別名".into());
        }
        assert_eq!(
            document.structure_error().as_deref(),
            Some("設定オブジェクト (`@{ … }`) を持つ参加者 `A` には別名 (as) を付けられません。")
        );

        let mut document = base.clone();
        if let Some(participant) = document.participants.first_mut() {
            participant.kind = SequenceParticipantKind::Actor;
        }
        assert!(
            document
                .structure_error()
                .is_some_and(|message| message.contains("participant")),
            "{:?}",
            document.structure_error()
        );

        let mut document = base;
        if let Some(participant) = document.participants.first_mut() {
            participant.preserved_settings = Some("type: a } b".into());
        }
        assert!(document.structure_error().is_some());
    }

    /// An id spelled with the `#…;` shorthand is written back verbatim, so it
    /// is kept in that spelling rather than decoded, and the diagram opens.
    #[test]
    fn an_id_with_an_entity_keeps_its_spelling() {
        for source in [
            "sequenceDiagram\n    participant A#59;B\n    participant C\n    A#59;B->>C: x\n    Note over A#59;B,C: メモ",
            "sequenceDiagram\n    participant x#quot;y as 表示名\n    participant C\n    x#quot;y->>+C: x\n    C-->>-x#quot;y: y\n    activate x#quot;y\n    deactivate x#quot;y",
            "sequenceDiagram\n    participant A\n    A->>A: x\n    create participant B#35;1\n    A->>B#35;1: 作成\n    destroy B#35;1\n    A-xB#35;1: 破棄",
        ] {
            let document = parse_sequence(source)
                .unwrap_or_else(|error| panic!("`{source}` should open: {error}"));
            assert!(
                document
                    .participants
                    .iter()
                    .all(|participant| participant.id.is_ascii()),
                "an id should not carry merman's placeholder characters: {:?}",
                document.participants
            );
            let emitted = assert_stable(source);
            assert_eq!(emitted, source, "an unedited diagram should not move");
            assert_renders(&emitted);
        }

        let document = parse_sequence(
            "sequenceDiagram\n    participant A#59;B\n    participant C\n    A#59;B->>C: x",
        )
        .expect("source should parse");
        assert_eq!(ids(&document), vec!["A#59;B", "C"]);
        assert_eq!(
            document
                .participants
                .first()
                .and_then(|participant| participant.alias.clone()),
            None,
            "the id's own description is not an alias"
        );
        assert!(is_valid_participant_id("A#59;B"));
        assert!(is_valid_participant_id("x#quot;y"));
        assert!(
            !is_valid_participant_id("#59;B"),
            "a leading `#` opens a comment"
        );
        assert!(
            !is_valid_participant_id("A#B"),
            "a bare `#` is still refused"
        );
        assert!(
            !is_valid_participant_id("A;B"),
            "a bare `;` still ends the statement"
        );
    }

    /// `merman`'s placeholder characters typed as themselves would come back
    /// as a `#…;` shorthand, turning `A\u{FB02}\u{B0}x` into `A#x` — a column
    /// named `A` followed by a comment. A complete placeholder typed out is
    /// the shorthand's own spelling to the renderer, so it may come back as one.
    #[test]
    fn an_id_holding_placeholder_characters_is_refused() {
        for source in [
            "sequenceDiagram\n    participant A\u{FB02}\u{B0}x\n    participant C\n    C->>C: x",
            "sequenceDiagram\n    A\u{B6}\u{DF}B->>C: x",
        ] {
            assert!(
                parse_sequence(source).is_err(),
                "`{source}` should be refused"
            );
        }
        assert_eq!(
            encode_entity_shorthands("A#59;B#quot;C#x"),
            "A\u{FB02}\u{B0}\u{B0}59\u{B6}\u{DF}B\u{FB02}\u{B0}quot\u{B6}\u{DF}C#x"
        );
    }

    /// The wrap setting of a `box` is the `wrap:` prefix of its name, which a
    /// box with no name still has to be able to carry.
    #[test]
    fn a_box_keeps_its_wrap_setting() {
        for (group, expected) in [
            (
                SequenceGroupBox {
                    name: Some("業務".into()),
                    fill: None,
                    wrap: true,
                },
                "box wrap:業務",
            ),
            (
                SequenceGroupBox {
                    name: Some("業務".into()),
                    fill: Some("lightblue".into()),
                    wrap: true,
                },
                "box lightblue wrap:業務",
            ),
            (
                SequenceGroupBox {
                    name: None,
                    fill: None,
                    wrap: true,
                },
                "box wrap:",
            ),
            (
                SequenceGroupBox {
                    name: None,
                    fill: Some("lavender".into()),
                    wrap: true,
                },
                "box lavender wrap:",
            ),
        ] {
            let document = SequenceDocument {
                title: None,
                participants: vec![participant("A", Some(0)), participant("B", None)],
                boxes: vec![group.clone()],
                items: vec![message("A", "B", "x")],
                preserved_lines: Vec::new(),
            };
            let emitted = emit_sequence(&document);
            assert!(
                emitted.contains(&format!("\n    {expected}\n")),
                "expected `{expected}`:\n{emitted}"
            );
            let reopened = parse_sequence(&emitted)
                .unwrap_or_else(|error| panic!("`{emitted}` should parse: {error}"));
            assert_eq!(reopened.boxes, vec![group], "{emitted}");
            assert_eq!(emit_sequence(&reopened), emitted);
            assert_renders(&emitted);
        }
    }

    /// The editor could build a second `destroy` of the same column, which
    /// would come back from the fence with only one of the two.
    #[test]
    fn a_second_destroy_of_a_column_is_reported() {
        let document = SequenceDocument {
            title: None,
            participants: vec![participant("A", None), participant("B", None)],
            boxes: Vec::new(),
            items: vec![
                SequenceItem::Destroy("B".into()),
                message("A", "B", "x"),
                SequenceItem::Destroy("B".into()),
                message("A", "B", "y"),
            ],
            preserved_lines: Vec::new(),
        };
        assert_eq!(
            document.structure_error().as_deref(),
            Some("参加者 `B` が二重に `destroy` されています。")
        );
    }

    fn boxed_document() -> SequenceDocument {
        // Columns: [A B](0) C [D](1) E, then the created F; box 2 is empty.
        SequenceDocument {
            title: None,
            participants: vec![
                participant("A", Some(0)),
                participant("B", Some(0)),
                participant("C", None),
                participant("D", Some(1)),
                participant("E", None),
                participant("F", None),
            ],
            boxes: vec![
                SequenceGroupBox {
                    name: Some("一".into()),
                    fill: None,
                    wrap: false,
                },
                SequenceGroupBox {
                    name: Some("二".into()),
                    fill: None,
                    wrap: false,
                },
                SequenceGroupBox {
                    name: Some("空".into()),
                    fill: None,
                    wrap: false,
                },
            ],
            items: vec![
                message("A", "E", "x"),
                SequenceItem::Create("F".into()),
                message("A", "F", "y"),
            ],
            preserved_lines: Vec::new(),
        }
    }

    fn moved(document: &SequenceDocument, order: &[usize]) -> SequenceDocument {
        let mut document = document.clone();
        assert!(document.reorder_participants(order), "{order:?}");
        document
    }

    /// A column steps only where the result can still be written back: inside
    /// its own box, or — for a column in no box — over a whole box at once.
    #[test]
    fn a_participant_moves_only_where_the_diagram_stays_writable() {
        let document = boxed_document();
        assert_eq!(document.structure_error(), None);

        // Inside a box.
        let order = document
            .participant_move_order(0, true)
            .expect("A can trade places with B");
        assert_eq!(
            ids(&moved(&document, &order)),
            vec!["B", "A", "C", "D", "E", "F"]
        );

        // Out of the edge of a box is the group menu's job.
        assert_eq!(document.participant_move_order(1, true), None);
        assert_eq!(document.participant_move_order(0, false), None);
        assert_eq!(document.participant_move_order(3, false), None);

        // A column in no box steps over a whole box.
        let order = document
            .participant_move_order(2, false)
            .expect("C can step over the first box");
        let after = moved(&document, &order);
        assert_eq!(ids(&after), vec!["C", "A", "B", "D", "E", "F"]);
        assert_eq!(after.structure_error(), None);
        let order = document
            .participant_move_order(2, true)
            .expect("C can step over the second box");
        let after = moved(&document, &order);
        assert_eq!(ids(&after), vec!["A", "B", "D", "C", "E", "F"]);
        assert_eq!(after.structure_error(), None);

        // A created column is placed by its statement.
        assert_eq!(document.participant_move_order(4, true), None);
        assert_eq!(document.participant_move_order(5, false), None);
        assert_eq!(document.participant_move_order(9, false), None);

        // Every step the editor offers leaves a writable diagram.
        for index in 0..document.participants.len() {
            for forward in [false, true] {
                if let Some(order) = document.participant_move_order(index, forward) {
                    assert_eq!(
                        moved(&document, &order).structure_error(),
                        None,
                        "moving {index} forward={forward}"
                    );
                }
            }
        }
    }

    /// Two boxes trade their whole runs of columns; an empty box is drawn
    /// after every other one, so it only trades with another empty box.
    #[test]
    fn a_box_moves_with_its_columns() {
        let document = boxed_document();
        assert_eq!(document.canonical_box_order(), vec![0, 1, 2]);

        let (participant_order, box_order) = document
            .box_move_order(1, false)
            .expect("the second box can move up");
        let mut after = moved(&document, &participant_order);
        assert!(after.reorder_boxes(&box_order));
        assert_eq!(ids(&after), vec!["D", "C", "A", "B", "E", "F"]);
        assert_eq!(
            after
                .boxes
                .iter()
                .map(|group| group.name.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["二", "一", "空"]
        );
        assert_eq!(
            after
                .participants
                .iter()
                .map(|participant| participant.box_index)
                .collect::<Vec<_>>(),
            vec![Some(0), None, Some(1), Some(1), None, None]
        );
        assert_eq!(after.structure_error(), None);
        assert_eq!(after.canonical_box_order(), vec![0, 1, 2]);

        // The fence reopens with the boxes in the order the list shows.
        let emitted = emit_sequence(&after);
        let reopened = parse_sequence(&emitted).expect("the moved boxes should reopen");
        assert_eq!(reopened.boxes, after.boxes, "{emitted}");
        assert_eq!(ids(&reopened), ids(&after), "{emitted}");
        assert_renders(&emitted);

        assert_eq!(document.box_move_order(0, false), None);
        assert_eq!(
            document.box_move_order(1, true),
            None,
            "the next box is empty"
        );
        assert_eq!(document.box_move_order(2, false), None);
        assert_eq!(document.box_move_order(2, true), None);
    }

    /// A box that gets its first member in front of an older box is drawn
    /// first, so the list has to follow.
    #[test]
    fn the_canonical_box_order_follows_the_columns() {
        let mut document = boxed_document();
        document.participants = vec![
            participant("E", Some(2)),
            participant("A", Some(0)),
            participant("B", Some(0)),
            participant("C", None),
            participant("D", None),
            participant("F", None),
        ];
        assert_eq!(document.structure_error(), None);
        assert_eq!(document.canonical_box_order(), vec![2, 0, 1]);
        assert!(document.reorder_boxes(&[2, 0, 1]));
        assert_eq!(
            document
                .boxes
                .iter()
                .map(|group| group.name.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["空", "一", "二"]
        );
        assert_eq!(
            document
                .participants
                .iter()
                .map(|participant| participant.box_index)
                .collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(1), None, None, None]
        );
        assert_eq!(document.canonical_box_order(), vec![0, 1, 2]);

        // What the fence reopens with is the list as it now stands.
        let emitted = emit_sequence(&document);
        let reopened = parse_sequence(&emitted).expect("the fence should reopen");
        assert_eq!(reopened.boxes, document.boxes, "{emitted}");
    }

    #[test]
    fn created_columns_go_last_in_statement_order() {
        let document = SequenceDocument {
            title: None,
            participants: vec![
                participant("A", None),
                participant("D", None),
                participant("B", None),
                participant("C", None),
            ],
            boxes: Vec::new(),
            items: vec![
                SequenceItem::Create("C".into()),
                message("A", "C", "x"),
                SequenceItem::Create("D".into()),
                message("A", "D", "y"),
            ],
            preserved_lines: Vec::new(),
        };
        assert!(document.structure_error().is_some());
        let order = document.created_last_order();
        let after = moved(&document, &order);
        assert_eq!(ids(&after), vec!["A", "B", "C", "D"]);
        assert_eq!(after.structure_error(), None);
    }

    #[test]
    fn apply_order_refuses_anything_but_a_permutation() {
        let mut items = vec!['a', 'b', 'c'];
        assert!(!apply_order(&mut items, &[0, 1]));
        assert!(!apply_order(&mut items, &[0, 0, 1]));
        assert!(!apply_order(&mut items, &[0, 1, 3]));
        assert_eq!(items, vec!['a', 'b', 'c']);
        assert!(apply_order(&mut items, &[2, 0, 1]));
        assert_eq!(items, vec!['c', 'a', 'b']);
    }

    /// Every statement a sequence diagram can hold, in one diagram: none of
    /// them may be missing from what the editor writes back. Comparing the
    /// written text line by line is what catches a statement both parses lose.
    #[test]
    fn no_statement_is_dropped_on_the_way_back() {
        let source = "%%{init: {\"theme\": \"forest\"}}%%\nsequenceDiagram\n    %% 先頭のメモ\n    title: 棚卸し\n    accTitle: 図の名前\n    accDescr: 図の説明\n    box lightblue wrap:業務\n        participant A as 利用者\n        actor B\n    end\n    box 空\n    end\n    participant C@{ \"type\": \"control\" }\n    participant X#59;Y\n    links A: {\"詳細\": \"https://example.com\"}\n    link B: 窓口 @ https://example.com/b\n    properties A: {\"class\": \"internal\"}\n    details A: {\"x\": \"y\"}\n    autonumber 10 10\n    A->>+B: 依頼\n    B-->>-A: 応答\n    A-)X#59;Y: 非同期\n    autonumber off\n    rect rgb(200, 220, 255)\n        loop 毎回\n            A->>C: 問い合わせ\n        end\n    end\n    critical 接続\n        A->>C: 接続\n    option 失敗\n        A-xC: 切断\n    end\n    break 中断\n        A->>B: 理由\n    end\n    par 並行\n        A->>B: 1\n    and 別\n        A->>C: 2\n    end\n    par_over 重ね\n        A->>B: 3\n    end\n    alt 成功\n        A->>B: OK\n    else 失敗\n        A->>B: NG\n    end\n    opt 任意\n        Note left of A: 左\n        Note right of B: 右\n        Note over A,B: 両方\n    end\n    activate C\n    deactivate C\n    create participant D as 新規\n    A->>D: 作成\n    destroy D\n    A-xD: 破棄\n    %% 末尾のメモ";
        let document = parse_sequence(source).expect("the inventory should open");
        let emitted = emit_sequence(&document);
        let written = emitted.lines().map(str::trim).collect::<Vec<_>>();
        for line in source.lines().map(str::trim) {
            assert!(written.contains(&line), "`{line}` was dropped:\n{emitted}");
        }
        assert_eq!(
            written.len(),
            source.lines().count(),
            "no statement may appear or vanish:\n{emitted}"
        );
        assert_stable(source);
        assert_renders(&emitted);
    }
}
