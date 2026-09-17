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
//! Sources the parser refuses rather than rewrite, because writing them back
//! would drop something: `create` / `destroy` participants (their column
//! position depends on where the statement sits between messages), participant
//! config objects (`A@{ … }`), the central-connection spellings (`A ->>() B`)
//! and end-of-line comments.

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

        for item in &self.items {
            match item {
                SequenceItem::Message(message) => {
                    for id in [&message.from, &message.to] {
                        if self.participant_index(id).is_none() {
                            return Some(format!("参加者 `{id}` が宣言されていません。"));
                        }
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
                    }
                }
                SequenceItem::Activate(id) => {
                    if self.participant_index(id).is_none() {
                        return Some(format!("参加者 `{id}` が宣言されていません。"));
                    }
                    *activations.entry(id.as_str()).or_default() += 1;
                }
                SequenceItem::Deactivate(id) => {
                    if self.participant_index(id).is_none() {
                        return Some(format!("参加者 `{id}` が宣言されていません。"));
                    }
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

    let document = document_from_model(model, scan.preserved)?;
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
) -> Result<SequenceDocument> {
    if !model.created_actors.is_empty() || !model.destroyed_actors.is_empty() {
        bail!(
            "`create` / `destroy` を使った図は構造化編集に対応していません (参加者の並び順を保てません)。テキストのまま編集してください。"
        );
    }

    let mut box_of_participant: HashMap<&str, usize> = HashMap::new();
    for (index, group) in model.boxes.iter().enumerate() {
        for id in &group.actor_keys {
            box_of_participant.entry(id.as_str()).or_insert(index);
        }
    }

    let mut participants = Vec::with_capacity(model.actor_order.len());
    for id in &model.actor_order {
        let actor = model
            .actors
            .get(id)
            .ok_or_else(|| anyhow!("参加者 `{id}` の情報が見つかりませんでした。"))?;
        let kind = match actor.actor_type.as_str() {
            "participant" => SequenceParticipantKind::Participant,
            "actor" => SequenceParticipantKind::Actor,
            other => bail!(
                "参加者の種類 `{other}` は構造化編集に対応していません。テキストのまま編集してください。"
            ),
        };
        // Every other string out of the model is entity-decoded before it
        // reaches the editor, but an id is also written back verbatim and the
        // id token has no escape of its own: `participant a#quot;b` would have
        // to be re-encoded to survive, and `#` is not a character an id may
        // hold. A diagram that spells an id with an entity is therefore left to
        // the text editor instead of being shown with `merman`'s placeholder
        // characters in it.
        if decode_text(id) != *id {
            bail!(
                "参加者の id がエンティティ (`#…;`) で書かれているため、構造化編集に対応していません。テキストのまま編集してください。"
            );
        }
        let description = decode_text(&actor.description);
        participants.push(SequenceParticipant {
            alias: (description != *id).then_some(description),
            kind,
            wrap: actor.wrap,
            box_index: box_of_participant.get(id.as_str()).copied(),
            id: id.clone(),
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

fn items_from_model(model: &SequenceDiagramRenderModel) -> Result<Vec<SequenceItem>> {
    let messages = &model.messages;
    let mut items = Vec::with_capacity(messages.len());
    let mut index = 0;

    while let Some(message) = messages.get(index) {
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
            }) {
                deactivate = true;
                index += 1;
            }

            items.push(SequenceItem::Message(SequenceMessage {
                from,
                to,
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
                    from,
                    to,
                    text: decode_text(message.message_text()),
                    wrap: message.wrap,
                }));
            }
            LINETYPE_ACTIVE_START => {
                let id = message
                    .from
                    .clone()
                    .ok_or_else(|| anyhow!("対象のない activate が含まれています。"))?;
                items.push(SequenceItem::Activate(id));
            }
            LINETYPE_ACTIVE_END => {
                let id = message
                    .from
                    .clone()
                    .ok_or_else(|| anyhow!("対象のない deactivate が含まれています。"))?;
                items.push(SequenceItem::Deactivate(id));
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

    for participant in &document.participants {
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
            if member.box_index == Some(box_index) {
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
        .unwrap_or_default();
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
pub fn is_valid_participant_id(id: &str) -> bool {
    is_writable_participant_id(id)
        && !id.ends_with('-')
        && !id.contains(['>', '@', '(', ')', '#', '%', '"'])
}

/// Whether `id` can be written back as itself at all.
///
/// A diagram written by hand may name a participant with characters the editor
/// would not offer, and those still have to survive being written out again,
/// so this is the rule Mermaid's own lexer imposes rather than the editor's.
fn is_writable_participant_id(id: &str) -> bool {
    // A leading digit is read as a number; a leading `-` after an arrow is the
    // deactivation marker; a leading `#` or `%%` opens a comment; a trailing
    // `()` in front of an arrow is the central connection marker.
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
    for statement in statements {
        if let Some(offset) = comment_start(statement) {
            let comment = statement.get(offset..).unwrap_or_default();
            return Some(format!(
                "文の末尾のコメント ({comment}) は構造化編集で失われます。コメントを独立した行に移すか、テキストのまま編集してください。"
            ));
        }
        if statement.contains("@{") {
            return Some(format!(
                "参加者の設定オブジェクト (`@{{ … }}`) を使った図は構造化編集に対応していません ({statement})。テキストのまま編集してください。"
            ));
        }
    }
    None
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
            "sequenceDiagram\n  participant A@{ \"type\": \"actor\" }\n  A->>B: x",
            "sequenceDiagram\n  create participant C\n  A->>C: x",
            "sequenceDiagram\n  participant A\n  participant B\n  destroy B\n  A->>B: x",
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
}
