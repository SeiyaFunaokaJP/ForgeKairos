//! The fill and text colors the user paints onto the grid, and where they live.
//!
//! A CSV or TSV file has nowhere to keep formatting, so the colors are stored
//! beside the file's absolute path in Zed's key value store instead of in the
//! file. They are addressed by *physical* row - the line of the file, counting
//! the header row as line 0 - so that turning "先頭行を見出しにする" on and off
//! leaves them on the rows the user painted.
//!
//! One file has one [`GridStylesModel`], shared by every grid open on it, so
//! that a split or a second window paints and saves the same colors instead of
//! overwriting each other's.
//!
//! Rows and columns the grid itself inserts or deletes take their colors with
//! them, and undoing such an edit takes them back. Lines added or removed from
//! the file's text tab do not: the grid only sees the reparsed result, which
//! says nothing about which lines moved where, so the colors stay on the line
//! numbers they were painted on.

use std::{
    any::TypeId,
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use command_palette_hooks::CommandPaletteFilter;
use db::kvp::KeyValueStore;
use editor::Editor;
use gpui::{Action, AppContext as _, Entity, Global, Hsla, WeakEntity, actions, hsla};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use text::TransactionId;
use theme::Appearance;
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};

use crate::{
    TabularDataPreviewPane,
    edit_engine::{RowInsertPosition, RowRef},
    grid::{
        GridCell, GridRect, GridRow, GridSelectionKind, MAX_GRID_OPERATION_CELLS, grouped_digits,
    },
};

/// 選択範囲を指定した色で塗りつぶします。
#[derive(PartialEq, Clone, Default, Deserialize, JsonSchema, Action)]
#[action(namespace = tabular_data)]
#[serde(deny_unknown_fields)]
pub struct SetFillColor {
    #[serde(default)]
    pub color: CellColor,
}

/// 選択範囲の文字の色を変えます。
#[derive(PartialEq, Clone, Default, Deserialize, JsonSchema, Action)]
#[action(namespace = tabular_data)]
#[serde(deny_unknown_fields)]
pub struct SetTextColor {
    #[serde(default)]
    pub color: CellColor,
}

actions!(
    tabular_data,
    [
        /// 選択範囲の塗りつぶしと文字の色を消します。
        ClearCellColors,
    ]
);

/// Keeps the two color actions out of the command palette.
///
/// The palette can only dispatch an action's default, and the default color is
/// "なし", so both would show up there as a way to take a color away - which is
/// what `ClearCellColors` is for. They are reached from the palette menus.
pub(crate) fn hide_color_actions_from_the_command_palette(cx: &mut App) {
    CommandPaletteFilter::update_global(cx, |filter, _cx| {
        filter.hide_action_types(&[TypeId::of::<SetFillColor>(), TypeId::of::<SetTextColor>()]);
    });
}

/// One of the colors the palette offers, standing in for the standard colors a
/// spreadsheet fills cells with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CellColor {
    Yellow,
    Green,
    Blue,
    Orange,
    Red,
    Purple,
    Gray,
    /// Takes the color away again. On a cell this cancels the color of its row,
    /// its column or the table; on any of those it removes the color itself.
    #[default]
    None,
}

impl CellColor {
    /// The palette, in the order it is offered.
    pub(crate) const PALETTE: [CellColor; 8] = [
        CellColor::Yellow,
        CellColor::Green,
        CellColor::Blue,
        CellColor::Orange,
        CellColor::Red,
        CellColor::Purple,
        CellColor::Gray,
        CellColor::None,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            CellColor::Yellow => i18n::tc!("cell color", "Yellow"),
            CellColor::Green => i18n::tc!("cell color", "Green"),
            CellColor::Blue => i18n::tc!("cell color", "Blue"),
            CellColor::Orange => i18n::tc!("cell color", "Orange"),
            CellColor::Red => i18n::tc!("cell color", "Red"),
            CellColor::Purple => i18n::tc!("cell color", "Purple"),
            CellColor::Gray => i18n::tc!("cell color", "Gray"),
            CellColor::None => i18n::tc!("cell color", "None"),
        }
    }

    fn stored_name(self) -> &'static str {
        match self {
            CellColor::Yellow => "yellow",
            CellColor::Green => "green",
            CellColor::Blue => "blue",
            CellColor::Orange => "orange",
            CellColor::Red => "red",
            CellColor::Purple => "purple",
            CellColor::Gray => "gray",
            CellColor::None => "none",
        }
    }

    /// `None` for a name this version does not know, which is how a file written
    /// by a newer Zed loses one color instead of all of them.
    fn from_stored_name(name: &str) -> Option<CellColor> {
        CellColor::PALETTE
            .into_iter()
            .find(|color| color.stored_name() == name)
    }

    /// Hue in degrees and saturation. The lightness and the alpha come from the
    /// theme's appearance, because one pair cannot read on both a white and a
    /// near-black background.
    fn hue_and_saturation(self) -> Option<(f32, f32)> {
        let hue_and_saturation = match self {
            CellColor::Yellow => (48., 0.95),
            CellColor::Green => (130., 0.60),
            CellColor::Blue => (212., 0.80),
            CellColor::Orange => (28., 0.90),
            CellColor::Red => (2., 0.75),
            CellColor::Purple => (282., 0.55),
            CellColor::Gray => (0., 0.0),
            CellColor::None => return None,
        };
        Some(hue_and_saturation)
    }

    /// The background a cell painted with this color gets. Translucent, so that
    /// the buffer text stays readable through it; a light theme needs more of it
    /// than a dark one before the color is visible at all.
    pub(crate) fn fill(self, appearance: Appearance) -> Option<Hsla> {
        let (hue, saturation) = self.hue_and_saturation()?;
        let alpha = match appearance {
            Appearance::Light => 0.45,
            Appearance::Dark => 0.30,
        };
        Some(hsla(hue / 360., saturation, 0.55, alpha))
    }

    /// The color a cell's text is painted with: dark enough to read on a light
    /// background, light enough to read on a dark one.
    pub(crate) fn text(self, appearance: Appearance) -> Option<Hsla> {
        let (hue, saturation) = self.hue_and_saturation()?;
        let lightness = match appearance {
            Appearance::Light => 0.35,
            Appearance::Dark => 0.65,
        };
        Some(hsla(hue / 360., saturation, lightness, 1.0))
    }
}

/// An unrecognized color name is dropped on its own rather than taken as a parse
/// failure that would throw away everything else the file was painted with.
fn color_from_stored_name<'de, D>(deserializer: D) -> Result<Option<CellColor>, D::Error>
where
    D: Deserializer<'de>,
{
    let name = Option::<String>::deserialize(deserializer)?;
    Ok(name.as_deref().and_then(CellColor::from_stored_name))
}

/// Which of the two colors an operation is setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StyleField {
    Fill,
    Text,
    Both,
}

impl StyleField {
    fn covers_fill(self) -> bool {
        matches!(self, StyleField::Fill | StyleField::Both)
    }

    fn covers_text(self) -> bool {
        matches!(self, StyleField::Text | StyleField::Both)
    }
}

/// The colors painted onto one row, column, cell, or the whole table.
///
/// `Some(CellColor::None)` is not the same as `None`: on a cell it is an
/// explicit "no color" that cancels what the row, the column or the table would
/// otherwise show through, while `None` means the cell says nothing at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CellStyle {
    #[serde(
        default,
        deserialize_with = "color_from_stored_name",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) fill: Option<CellColor>,
    #[serde(
        default,
        deserialize_with = "color_from_stored_name",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) text: Option<CellColor>,
}

impl CellStyle {
    pub(crate) fn is_empty(&self) -> bool {
        self.fill.is_none() && self.text.is_none()
    }

    fn set(&mut self, field: StyleField, color: Option<CellColor>) {
        if field.covers_fill() {
            self.fill = color;
        }
        if field.covers_text() {
            self.text = color;
        }
    }

    /// `self` laid over `base`: whatever `self` does not say is taken from it.
    fn over(self, base: CellStyle) -> CellStyle {
        CellStyle {
            fill: self.fill.or(base.fill),
            text: self.text.or(base.text),
        }
    }

    fn fill_color(self, appearance: Appearance) -> Option<Hsla> {
        self.fill.and_then(|color| color.fill(appearance))
    }

    fn text_color(self, appearance: Appearance) -> Option<Hsla> {
        self.text.and_then(|color| color.text(appearance))
    }
}

/// Every color painted onto one file, keyed by physical row and column.
///
/// A row or a column selection is kept as a single entry rather than as one
/// entry per cell, so that painting a 50,000 row file costs one entry per row
/// instead of one per cell. The maps are ordered, so inserting or deleting a row
/// only has to touch the entries at or below it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GridStyles {
    default_style: CellStyle,
    rows: BTreeMap<usize, CellStyle>,
    cols: BTreeMap<usize, CellStyle>,
    cells: BTreeMap<(usize, usize), CellStyle>,
}

/// The entries a deletion dropped, at the indices they had before it, so that
/// undoing the deletion can put them back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemovedStyles {
    rows: BTreeMap<usize, CellStyle>,
    cols: BTreeMap<usize, CellStyle>,
    cells: BTreeMap<(usize, usize), CellStyle>,
}

/// What a color operation covers, in physical rows and columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StyleScope {
    Cells { rows: Vec<usize>, cols: Vec<usize> },
    Rows(Vec<usize>),
    Columns(Vec<usize>),
    All,
}

impl GridStyles {
    pub(crate) fn is_empty(&self) -> bool {
        self.default_style.is_empty()
            && self.rows.is_empty()
            && self.cols.is_empty()
            && self.cells.is_empty()
    }

    /// What a cell inherits when it says nothing itself: its row wins over its
    /// column, which wins over the whole table.
    fn inherited(&self, row: usize, col: usize) -> CellStyle {
        let row = self.rows.get(&row).copied().unwrap_or_default();
        let col = self.cols.get(&col).copied().unwrap_or_default();
        row.over(col).over(self.default_style)
    }

    /// The colors of one cell: what was painted onto the cell itself wins over
    /// its row, which wins over its column, which wins over the whole table.
    pub(crate) fn style_for(&self, row: usize, col: usize) -> CellStyle {
        let cell = self.cells.get(&(row, col)).copied().unwrap_or_default();
        cell.over(self.inherited(row, col))
    }

    /// `self` laid over `base`, entry by entry. Used when the stored colors come
    /// back after the user has already painted: what they just did is newer.
    fn merged_over(&self, base: &GridStyles) -> GridStyles {
        let mut merged = base.clone();
        merged.default_style = self.default_style.over(base.default_style);
        for (&row, &style) in &self.rows {
            let entry = merged.rows.entry(row).or_default();
            *entry = style.over(*entry);
        }
        for (&col, &style) in &self.cols {
            let entry = merged.cols.entry(col).or_default();
            *entry = style.over(*entry);
        }
        for (&cell, &style) in &self.cells {
            let entry = merged.cells.entry(cell).or_default();
            *entry = style.over(*entry);
        }
        merged
    }

    pub(crate) fn set(&mut self, scope: &StyleScope, field: StyleField, color: CellColor) {
        match scope {
            StyleScope::Cells { rows, cols } => {
                for &row in rows {
                    for &col in cols {
                        self.set_cell(row, col, field, color);
                    }
                }
            }
            StyleScope::Rows(rows) => {
                for &row in rows {
                    set_scope_entry(&mut self.rows, row, field, color);
                }
                // A cell entry outranks the row it sits in, so a row that kept
                // its painted cells would show the new color everywhere except
                // where the user is looking.
                let painted: HashSet<usize> = rows.iter().copied().collect();
                clear_field(&mut self.cells, field, |&(row, _)| painted.contains(&row));
            }
            StyleScope::Columns(cols) => {
                for &col in cols {
                    set_scope_entry(&mut self.cols, col, field, color);
                }
                let painted: HashSet<usize> = cols.iter().copied().collect();
                clear_field(&mut self.cells, field, |&(_, col)| painted.contains(&col));
            }
            StyleScope::All => {
                self.default_style.set(field, scope_value(color));
                clear_field(&mut self.rows, field, |_| true);
                clear_field(&mut self.cols, field, |_| true);
                clear_field(&mut self.cells, field, |_| true);
            }
        }
    }

    fn set_cell(&mut self, row: usize, col: usize, field: StyleField, color: CellColor) {
        let inherited = self.inherited(row, col);
        let key = (row, col);
        let entry = self.cells.entry(key).or_default();
        if field.covers_fill() {
            entry.fill = cell_value(color, inherited.fill);
        }
        if field.covers_text() {
            entry.text = cell_value(color, inherited.text);
        }
        if entry.is_empty() {
            self.cells.remove(&key);
        }
    }

    pub(crate) fn insert_rows(&mut self, at: usize, count: usize) {
        if count == 0 {
            return;
        }
        let tail = self.rows.split_off(&at);
        self.rows
            .extend(tail.into_iter().map(|(row, style)| (row + count, style)));
        let tail = self.cells.split_off(&(at, 0));
        self.cells.extend(
            tail.into_iter()
                .map(|((row, col), style)| ((row + count, col), style)),
        );
    }

    /// Removes the given physical rows, reporting the entries that went with
    /// them so that an undo can put them back.
    pub(crate) fn delete_rows(&mut self, rows: &[usize]) -> RemovedStyles {
        let deleted = sorted_unique(rows);
        let Some(&first) = deleted.first() else {
            return RemovedStyles::default();
        };
        let mut removed = RemovedStyles::default();

        for (row, style) in self.rows.split_off(&first) {
            match after_deletion(row, &deleted) {
                Some(shifted) => self.rows.insert(shifted, style),
                None => removed.rows.insert(row, style),
            };
        }
        for ((row, col), style) in self.cells.split_off(&(first, 0)) {
            match after_deletion(row, &deleted) {
                Some(shifted) => self.cells.insert((shifted, col), style),
                None => removed.cells.insert((row, col), style),
            };
        }
        removed
    }

    /// Undoes a [`Self::delete_rows`] of the same rows.
    pub(crate) fn restore_rows(&mut self, rows: &[usize], removed: &RemovedStyles) {
        let deleted = sorted_unique(rows);
        let Some(&first) = deleted.first() else {
            return;
        };
        let tail = self.rows.split_off(&first);
        self.rows.extend(
            tail.into_iter()
                .map(|(row, style)| (before_deletion(row, &deleted), style)),
        );
        let tail = self.cells.split_off(&(first, 0));
        self.cells.extend(
            tail.into_iter()
                .map(|((row, col), style)| ((before_deletion(row, &deleted), col), style)),
        );
        self.rows.extend(removed.rows.iter());
        self.cells.extend(removed.cells.iter());
    }

    /// A column is the second half of a cell's key, so unlike a row shift this
    /// has to walk every painted cell. There are only ever as many of those as
    /// the user painted by hand.
    pub(crate) fn insert_columns(&mut self, at: usize, count: usize) {
        if count == 0 {
            return;
        }
        let tail = self.cols.split_off(&at);
        self.cols
            .extend(tail.into_iter().map(|(col, style)| (col + count, style)));
        self.cells = std::mem::take(&mut self.cells)
            .into_iter()
            .map(|((row, col), style)| {
                let col = if col >= at { col + count } else { col };
                ((row, col), style)
            })
            .collect();
    }

    pub(crate) fn delete_columns(&mut self, cols: &[usize]) -> RemovedStyles {
        let deleted = sorted_unique(cols);
        let Some(&first) = deleted.first() else {
            return RemovedStyles::default();
        };
        let mut removed = RemovedStyles::default();

        for (col, style) in self.cols.split_off(&first) {
            match after_deletion(col, &deleted) {
                Some(shifted) => self.cols.insert(shifted, style),
                None => removed.cols.insert(col, style),
            };
        }
        let mut kept = BTreeMap::new();
        for ((row, col), style) in std::mem::take(&mut self.cells) {
            match after_deletion(col, &deleted) {
                Some(shifted) => kept.insert((row, shifted), style),
                None => removed.cells.insert((row, col), style),
            };
        }
        self.cells = kept;
        removed
    }

    /// Undoes a [`Self::delete_columns`] of the same columns.
    pub(crate) fn restore_columns(&mut self, cols: &[usize], removed: &RemovedStyles) {
        let deleted = sorted_unique(cols);
        let Some(&first) = deleted.first() else {
            return;
        };
        let tail = self.cols.split_off(&first);
        self.cols.extend(
            tail.into_iter()
                .map(|(col, style)| (before_deletion(col, &deleted), style)),
        );
        self.cells = std::mem::take(&mut self.cells)
            .into_iter()
            .map(|((row, col), style)| ((row, before_deletion(col, &deleted)), style))
            .collect();
        self.cols.extend(removed.cols.iter());
        self.cells.extend(removed.cells.iter());
    }

    /// The payload to store for this file, or `None` when nothing is painted and
    /// the stored row should be deleted instead.
    pub(crate) fn to_json(&self) -> Result<Option<String>> {
        if self.is_empty() {
            return Ok(None);
        }
        let serialized = SerializedGridStyles {
            default_style: self.default_style,
            rows: self.rows.clone(),
            cols: self.cols.clone(),
            // JSON object keys are strings, and a `(row, col)` key has none.
            cells: self
                .cells
                .iter()
                .map(|(&(row, col), &style)| SerializedCell { row, col, style })
                .collect(),
        };
        Ok(Some(serde_json::to_string(&serialized)?))
    }

    pub(crate) fn from_json(json: &str) -> Result<Self> {
        let serialized: SerializedGridStyles = serde_json::from_str(json)?;
        let mut styles = GridStyles {
            default_style: serialized.default_style,
            rows: serialized.rows,
            cols: serialized.cols,
            cells: serialized
                .cells
                .into_iter()
                .map(|cell| ((cell.row, cell.col), cell.style))
                .collect(),
        };
        // A color this version did not recognize left its entry behind empty.
        styles.rows.retain(|_, style| !style.is_empty());
        styles.cols.retain(|_, style| !style.is_empty());
        styles.cells.retain(|_, style| !style.is_empty());
        Ok(styles)
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SerializedGridStyles {
    #[serde(skip_serializing_if = "CellStyle::is_empty")]
    default_style: CellStyle,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    rows: BTreeMap<usize, CellStyle>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    cols: BTreeMap<usize, CellStyle>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cells: Vec<SerializedCell>,
}

#[derive(Serialize, Deserialize)]
struct SerializedCell {
    row: usize,
    col: usize,
    #[serde(flatten)]
    style: CellStyle,
}

/// "なし" at row, column or table level removes the entry: there is nothing
/// coarser for it to cancel out.
fn scope_value(color: CellColor) -> Option<CellColor> {
    (color != CellColor::None).then_some(color)
}

/// At cell level "なし" is an override that cancels the row, column or table
/// color, so it is only dropped when nothing would show through anyway.
fn cell_value(color: CellColor, inherited: Option<CellColor>) -> Option<CellColor> {
    if color == CellColor::None && inherited.is_none() {
        None
    } else {
        Some(color)
    }
}

fn set_scope_entry<K: Ord + Copy>(
    map: &mut BTreeMap<K, CellStyle>,
    key: K,
    field: StyleField,
    color: CellColor,
) {
    let style = map.entry(key).or_default();
    style.set(field, scope_value(color));
    if style.is_empty() {
        map.remove(&key);
    }
}

fn clear_field<K: Ord>(
    map: &mut BTreeMap<K, CellStyle>,
    field: StyleField,
    matches: impl Fn(&K) -> bool,
) {
    map.retain(|key, style| {
        if matches(key) {
            style.set(field, None);
        }
        !style.is_empty()
    });
}

pub(crate) fn sorted_unique(indices: &[usize]) -> Vec<usize> {
    let mut sorted = indices.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted
}

/// Where `index` ends up once every index in `deleted` is gone, or `None` when
/// `index` is one of them. `deleted` has to be sorted and free of duplicates.
pub(crate) fn after_deletion(index: usize, deleted: &[usize]) -> Option<usize> {
    match deleted.binary_search(&index) {
        Ok(_) => None,
        Err(position) => Some(index - position),
    }
}

/// The inverse of [`after_deletion`]: the index `index` had before the rows in
/// `deleted` were removed.
pub(crate) fn before_deletion(index: usize, deleted: &[usize]) -> usize {
    let mut restored = index;
    for &removed in deleted {
        if removed <= restored {
            restored += 1;
        }
    }
    restored
}

///// The shared per-file model /////

/// How long a change waits before it is written back, so that painting cell
/// after cell does not write the stored row on every click.
const STYLE_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Prefix of the key value store row a file's colors live in.
const STYLE_KEY_PREFIX: &str = "tabular_data_styles:";

/// How many grid operations' colors can be taken back. Past this the oldest is
/// forgotten, and undoing it simply leaves the colors where they are.
const MAX_RECORDED_SHIFTS: usize = 64;

/// A structural change to the table, remembered so that undoing the buffer edit
/// takes the colors back with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StyleShift {
    InsertRows {
        at: usize,
        count: usize,
    },
    DeleteRows {
        rows: Vec<usize>,
        removed: RemovedStyles,
    },
    InsertColumns {
        at: usize,
        count: usize,
    },
    DeleteColumns {
        cols: Vec<usize>,
        removed: RemovedStyles,
    },
}

impl StyleShift {
    pub(crate) fn delete_rows(rows: Vec<usize>) -> Self {
        StyleShift::DeleteRows {
            rows,
            removed: RemovedStyles::default(),
        }
    }

    pub(crate) fn delete_columns(cols: Vec<usize>) -> Self {
        StyleShift::DeleteColumns {
            cols,
            removed: RemovedStyles::default(),
        }
    }

    /// Applies the shift, recording what a deletion dropped so that undoing it
    /// can put those colors back. Applied again on redo, where the user may have
    /// painted something new in the meantime.
    fn apply(&mut self, styles: &mut GridStyles) {
        match self {
            StyleShift::InsertRows { at, count } => styles.insert_rows(*at, *count),
            StyleShift::DeleteRows { rows, removed } => *removed = styles.delete_rows(rows),
            StyleShift::InsertColumns { at, count } => styles.insert_columns(*at, *count),
            StyleShift::DeleteColumns { cols, removed } => *removed = styles.delete_columns(cols),
        }
    }

    /// The inverse, applied to the colors as they are now, so that anything
    /// painted after the operation keeps whatever it should keep.
    fn revert(&self, styles: &mut GridStyles) {
        match self {
            StyleShift::InsertRows { at, count } => {
                styles.delete_rows(&(*at..*at + *count).collect::<Vec<_>>());
            }
            StyleShift::DeleteRows { rows, removed } => styles.restore_rows(rows, removed),
            StyleShift::InsertColumns { at, count } => {
                styles.delete_columns(&(*at..*at + *count).collect::<Vec<_>>());
            }
            StyleShift::DeleteColumns { cols, removed } => styles.restore_columns(cols, removed),
        }
    }
}

/// The colors of one file, shared by every grid open on it.
pub(crate) struct GridStylesModel {
    /// The key value store row the colors live in, or `None` for a buffer with
    /// no local file, whose colors last only as long as the session.
    key: Option<String>,
    styles: GridStyles,
    /// Bumped on every change. A save only writes while it still holds the
    /// newest one, which is what debounces the writes.
    save_generation: Arc<AtomicUsize>,
    undo_stack: Vec<(TransactionId, StyleShift)>,
    redo_stack: Vec<(TransactionId, StyleShift)>,
    /// Set once the user has been told that this buffer's colors are not stored.
    warned_not_stored: bool,
}

/// Every live [`GridStylesModel`], by the row its colors live in, so that a
/// second grid on the same file joins the first instead of starting a copy that
/// would overwrite it on the next save.
#[derive(Default)]
struct GridStylesRegistry {
    models: HashMap<String, WeakEntity<GridStylesModel>>,
}

impl Global for GridStylesRegistry {}

impl GridStylesModel {
    pub(crate) fn for_editor(editor: &Entity<Editor>, cx: &mut App) -> Entity<Self> {
        let key = styles_key_for_editor(editor, cx);
        if let Some(model) = key
            .as_ref()
            .and_then(|key| cx.try_global::<GridStylesRegistry>()?.models.get(key))
            .and_then(WeakEntity::upgrade)
        {
            return model;
        }

        let model = cx.new(|cx| {
            let mut model = GridStylesModel {
                key: key.clone(),
                styles: GridStyles::default(),
                save_generation: Arc::default(),
                undo_stack: Vec::new(),
                redo_stack: Vec::new(),
                warned_not_stored: false,
            };
            model.load(cx);
            model
        });

        if let Some(key) = key {
            let registry = cx.default_global::<GridStylesRegistry>();
            registry.models.retain(|_, model| model.upgrade().is_some());
            registry.models.insert(key, model.downgrade());
        }
        model
    }

    pub(crate) fn styles(&self) -> &GridStyles {
        &self.styles
    }

    /// Reports whether the user still has to be told that this buffer's colors
    /// go nowhere, and remembers that they have been.
    fn take_not_stored_warning(&mut self) -> bool {
        if self.key.is_some() || self.warned_not_stored {
            return false;
        }
        self.warned_not_stored = true;
        true
    }

    fn set(
        &mut self,
        scope: &StyleScope,
        field: StyleField,
        color: CellColor,
        cx: &mut Context<Self>,
    ) {
        self.styles.set(scope, field, color);
        self.save(cx);
        cx.notify();
    }

    /// Moves the colors along with a grid operation and records it against the
    /// buffer transaction it belongs to.
    ///
    /// No transaction means nothing reached the buffer - the edit engine
    /// refused the operation, or there was no buffer to edit - so the cells
    /// never moved and the colors must not move either. Moving them would paint
    /// every color past the operation onto its neighbour, with no undo step
    /// left to take it back.
    fn record_shift(
        &mut self,
        transaction_id: Option<TransactionId>,
        mut shift: StyleShift,
        cx: &mut Context<Self>,
    ) {
        let Some(transaction_id) = transaction_id else {
            return;
        };
        let was_painted = !self.styles.is_empty();
        shift.apply(&mut self.styles);
        // Recorded even with nothing painted yet, so that a color added between
        // the operation and its undo still moves back with it.
        self.redo_stack.clear();
        self.undo_stack.push((transaction_id, shift));
        if self.undo_stack.len() > MAX_RECORDED_SHIFTS {
            self.undo_stack.remove(0);
        }
        if was_painted || !self.styles.is_empty() {
            self.save(cx);
            cx.notify();
        }
    }

    /// Takes the colors back when the grid operation `transaction_id` belongs to
    /// is undone. Anything else - an edit made on the text side, or an operation
    /// whose record has been forgotten - leaves them where they are.
    fn undo(&mut self, transaction_id: TransactionId, cx: &mut Context<Self>) {
        if self.undo_stack.last().map(|(id, _)| *id) != Some(transaction_id) {
            return;
        }
        let Some((id, shift)) = self.undo_stack.pop() else {
            return;
        };
        shift.revert(&mut self.styles);
        self.redo_stack.push((id, shift));
        self.save(cx);
        cx.notify();
    }

    fn redo(&mut self, transaction_id: TransactionId, cx: &mut Context<Self>) {
        if self.redo_stack.last().map(|(id, _)| *id) != Some(transaction_id) {
            return;
        }
        let Some((id, mut shift)) = self.redo_stack.pop() else {
            return;
        };
        shift.apply(&mut self.styles);
        self.undo_stack.push((id, shift));
        self.save(cx);
        cx.notify();
    }

    /// The file was renamed or saved somewhere else, so the colors move to the
    /// row of the new path and the old one is dropped.
    fn rename(&mut self, key: Option<String>, cx: &mut Context<Self>) {
        if self.key == key {
            return;
        }
        let old_key = std::mem::replace(&mut self.key, key.clone());
        // Cancels any write still waiting to go to the old row, which the delete
        // below is about to remove.
        self.save_generation.fetch_add(1, Ordering::SeqCst);

        let handle = cx.entity().downgrade();
        let registry = cx.default_global::<GridStylesRegistry>();
        if let Some(old_key) = old_key.as_ref() {
            registry.models.remove(old_key);
        }
        if let Some(key) = key {
            registry.models.insert(key, handle);
        }

        if let Some(old_key) = old_key
            && let Some(store) = key_value_store(cx)
        {
            cx.background_spawn(async move {
                if let Err(err) = store.delete_kvp(old_key).await {
                    log::error!(
                        "tabular data preview: failed to drop the old path's cell colors: {err}"
                    );
                }
            })
            .detach();
        }
        self.save(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.key.clone() else {
            return;
        };
        let Some(store) = key_value_store(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let stored = match store.read_kvp(&key) {
                        Ok(stored) => stored?,
                        Err(err) => {
                            log::error!(
                                "tabular data preview: failed to read the stored cell colors: {err}"
                            );
                            return None;
                        }
                    };
                    match GridStyles::from_json(&stored) {
                        Ok(styles) => Some(styles),
                        Err(err) => {
                            log::error!(
                                "tabular data preview: failed to parse the stored cell colors: {err}"
                            );
                            None
                        }
                    }
                })
                .await;
            let Some(loaded) = loaded else {
                return;
            };
            this.update(cx, |this, cx| {
                // The read is a round trip late, so anything painted while it
                // was in flight is the newer of the two and wins.
                this.styles = this.styles.merged_over(&loaded);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Writes the colors back after a pause.
    ///
    /// The task holds its own copy and is detached, so closing the tab inside
    /// the debounce still stores what the user painted. Serializing happens on
    /// the background thread, and only for the write that actually goes out.
    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.key.clone() else {
            return;
        };
        let Some(store) = key_value_store(cx) else {
            return;
        };
        let generation = self.save_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let save_generation = self.save_generation.clone();
        let styles = self.styles.clone();
        let executor = cx.background_executor().clone();

        cx.background_spawn(async move {
            executor.timer(STYLE_SAVE_DEBOUNCE).await;
            if save_generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let payload = match styles.to_json() {
                Ok(payload) => payload,
                Err(err) => {
                    log::error!("tabular data preview: failed to serialize the cell colors: {err}");
                    return;
                }
            };
            let written = match payload {
                Some(payload) => store.write_kvp(key, payload).await,
                None => store.delete_kvp(key).await,
            };
            if let Err(err) = written {
                log::error!("tabular data preview: failed to store the cell colors: {err}");
            }
        })
        .detach();
    }
}

/// `None` when the application has no database, which is the case in tests that
/// do not set one up. Read rather than taken on faith, so that a missing
/// database is not a panic.
pub(crate) fn key_value_store(cx: &App) -> Option<KeyValueStore> {
    cx.try_global::<db::AppDatabase>()
        .map(KeyValueStore::from_app_db)
}

fn styles_key_for_editor(editor: &Entity<Editor>, cx: &App) -> Option<String> {
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let path = buffer.read(cx).file()?.as_local()?.abs_path(cx);
    Some(format!("{STYLE_KEY_PREFIX}{}", path.to_string_lossy()))
}

///// The pane's side /////

/// The colors one cell is drawn with, resolved once per cell per frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct CellPaint {
    pub(crate) background: Option<Hsla>,
    pub(crate) text: Option<Hsla>,
}

impl TabularDataPreviewPane {
    /// The line of the file `row` stands for, counting the header row as line 0.
    ///
    /// Without a header row the parser leaves line 1 among the data rows, so the
    /// two modes agree on which line each data row is and a painted row does not
    /// move when the header row is toggled.
    pub(crate) fn physical_row(&self, row: RowRef) -> usize {
        match row {
            RowRef::Header => 0,
            RowRef::Data(data_row) => *data_row + usize::from(self.has_header_row()),
        }
    }

    /// The number of lines the file holds, the header row included.
    fn physical_row_count(&self) -> usize {
        self.engine.contents.rows.len() + usize::from(self.has_header_row())
    }

    /// Resolved before the buffer edit, while the physical rows still mean what
    /// the stored colors were addressed against.
    pub(crate) fn physical_insert_position(&self, position: RowInsertPosition) -> usize {
        match position {
            RowInsertPosition::Before(row) => self.physical_row(row),
            RowInsertPosition::After(row) => self.physical_row(row) + 1,
            RowInsertPosition::AtEnd => self.physical_row_count(),
        }
    }

    /// `None` while nothing at all is painted on this file, which is the common
    /// case and skips the lookups entirely.
    fn cell_style(&self, cell: GridCell, cx: &App) -> Option<CellStyle> {
        let styles = self.styles.read(cx).styles();
        if styles.is_empty() {
            return None;
        }
        let row = self.row_ref(cell.row)?;
        Some(styles.style_for(self.physical_row(row), cell.col.get()))
    }

    /// Everything one cell's rendering needs, in a single lookup.
    ///
    /// The fill is composited onto the editor background before the selection
    /// goes over it, because one `bg` can only hold one color and the two have
    /// to read as layers.
    pub(crate) fn cell_paint(
        &self,
        cell: GridCell,
        is_selected: bool,
        is_active: bool,
        cx: &App,
    ) -> CellPaint {
        let colors = cx.theme().colors();
        let appearance = cx.theme().appearance();
        let selection = (is_selected && !is_active).then(|| colors.element_selected.opacity(0.5));
        let style = self.cell_style(cell, cx).unwrap_or_default();

        let background = match (style.fill_color(appearance), selection) {
            (Some(fill), Some(selection)) => {
                Some(colors.editor_background.blend(fill).blend(selection))
            }
            (Some(fill), None) => Some(colors.editor_background.blend(fill)),
            (None, selection) => selection,
        };
        // A search hit must stay findable over a colored or selected cell.
        let background = match self.search_match_background(cell, cx) {
            Some(search) => Some(background.unwrap_or(colors.editor_background).blend(search)),
            None => background,
        };
        CellPaint {
            background,
            text: style.text_color(appearance),
        }
    }

    fn physical_rows_of(&self, rect: GridRect) -> Vec<usize> {
        rect.rows()
            .filter_map(|row| self.row_ref(GridRow::from_index(row)))
            .map(|row| self.physical_row(row))
            .collect()
    }

    /// What one block of the selection paints, in the coarsest form that covers
    /// it.
    fn style_scope_for(&self, rect: GridRect, kind: GridSelectionKind) -> StyleScope {
        let cols: Vec<usize> = rect.cols().collect();
        match kind {
            GridSelectionKind::All => StyleScope::All,
            GridSelectionKind::Rows => StyleScope::Rows(self.physical_rows_of(rect)),
            GridSelectionKind::Columns => StyleScope::Columns(cols),
            GridSelectionKind::Cells => StyleScope::Cells {
                rows: self.physical_rows_of(rect),
                cols,
            },
        }
    }

    /// What every selected block paints, one scope per block.
    ///
    /// A scope is a set of rows crossed with a set of columns, which two blocks
    /// sitting apart cannot share without painting the empty corners between
    /// them, so they stay separate scopes and are applied one after the other.
    pub(crate) fn selection_style_scopes(&self) -> Vec<StyleScope> {
        self.selection_blocks()
            .into_iter()
            .map(|(rect, kind)| self.style_scope_for(rect, kind))
            .collect()
    }

    pub(crate) fn set_selection_color(
        &mut self,
        field: StyleField,
        color: CellColor,
        cx: &mut Context<Self>,
    ) {
        if self.grid_is_empty() || !self.within_color_budget(color, cx) {
            return;
        }
        let scopes = self.selection_style_scopes();
        let warn = self.styles.update(cx, |model, cx| {
            for scope in &scopes {
                model.set(scope, field, color, cx);
            }
            model.take_not_stored_warning()
        });
        if warn {
            self.set_status_message(i18n::t!("Colors aren't saved for this file"), cx);
        } else {
            self.clear_status_message();
        }
        cx.notify();
    }

    /// Refuses a selection whose colors would cost more entries than the grid is
    /// willing to build. A row or column selection is one entry per row or
    /// column however wide the table is, so only a cell range is counted by
    /// area - and taking colors away never adds an entry at all.
    fn within_color_budget(&mut self, color: CellColor, cx: &mut Context<Self>) -> bool {
        let mut entries: usize = 0;
        for (rect, kind) in self.selection_blocks() {
            if color == CellColor::None && kind != GridSelectionKind::Cells {
                continue;
            }
            entries = entries.saturating_add(match kind {
                GridSelectionKind::All => 1,
                GridSelectionKind::Rows => rect.row_count(),
                GridSelectionKind::Columns => rect.col_count(),
                GridSelectionKind::Cells => rect.cell_count(),
            });
        }
        if entries <= MAX_GRID_OPERATION_CELLS {
            return true;
        }
        let limit = grouped_digits(MAX_GRID_OPERATION_CELLS);
        self.set_status_message(
            i18n::t!("The selection is too large (up to {limit} cells) to set colors"),
            cx,
        );
        false
    }

    pub(crate) fn record_style_shift(
        &mut self,
        transaction_id: Option<TransactionId>,
        shift: StyleShift,
        cx: &mut Context<Self>,
    ) {
        self.styles.clone().update(cx, |model, cx| {
            model.record_shift(transaction_id, shift, cx);
        });
    }

    pub(crate) fn undo_style_shift(
        &mut self,
        transaction_id: TransactionId,
        cx: &mut Context<Self>,
    ) {
        self.styles
            .clone()
            .update(cx, |model, cx| model.undo(transaction_id, cx));
    }

    pub(crate) fn redo_style_shift(
        &mut self,
        transaction_id: TransactionId,
        cx: &mut Context<Self>,
    ) {
        self.styles
            .clone()
            .update(cx, |model, cx| model.redo(transaction_id, cx));
    }

    /// "Save as" gave the buffer a new path, and the colors are stored under the
    /// path rather than in the file.
    pub(crate) fn on_file_path_changed(&mut self, cx: &mut Context<Self>) {
        let key = styles_key_for_editor(self.editor(), cx);
        self.styles.clone().update(cx, |model, cx| {
            model.rename(key, cx);
        });
    }

    ///// Palette /////

    /// The two toolbar buttons that open the palette.
    pub(crate) fn render_color_buttons(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .gap_1()
            .items_center()
            .child(
                Label::new(i18n::t!("Color"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(self.render_color_menu(
                "tabular-fill-color",
                IconName::SquareDot,
                i18n::t!("Fill Color"),
                StyleField::Fill,
                cx,
            ))
            .child(self.render_color_menu(
                "tabular-text-color",
                IconName::Font,
                i18n::tc!("cell color", "Text Color"),
                StyleField::Text,
                cx,
            ))
            .into_any_element()
    }

    fn render_color_menu(
        &self,
        id: &'static str,
        icon: IconName,
        tooltip: &'static str,
        field: StyleField,
        cx: &mut Context<Self>,
    ) -> PopoverMenu<ContextMenu> {
        let view = cx.entity().downgrade();
        PopoverMenu::new(ElementId::Name(id.into()))
            .trigger_with_tooltip(
                IconButton::new(ElementId::NamedInteger(id.into(), 0), icon)
                    .icon_size(IconSize::Small),
                Tooltip::text(tooltip),
            )
            .menu(move |window, cx| {
                let view = view.clone();
                Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                    extend_with_color_palette(menu.header(tooltip), &view, field)
                }))
            })
    }
}

/// Adds the palette's eight entries to `menu`, each painting `field` when picked.
///
/// The grid is held weakly because the grid's own right click menu is stored on
/// the grid: a strong handle in an entry of it would be a cycle, and a menu left
/// open over a closing tab would keep the whole pane alive.
pub(crate) fn extend_with_color_palette(
    menu: ContextMenu,
    view: &WeakEntity<TabularDataPreviewPane>,
    field: StyleField,
) -> ContextMenu {
    let mut menu = menu;
    for color in CellColor::PALETTE {
        menu = menu.custom_entry(move |_window, cx| render_palette_entry(color, field, cx), {
            let view = view.clone();
            move |_window, cx| {
                view.update(cx, |this, cx| this.set_selection_color(field, color, cx))
                    .ok();
            }
        });
    }
    menu
}

fn render_palette_entry(color: CellColor, field: StyleField, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    let appearance = cx.theme().appearance();
    // The square shows what the entry actually does: the fill menu shows the
    // fill as it will look over the table, the text menu the letter color.
    let swatch = if field.covers_fill() {
        color
            .fill(appearance)
            .map(|fill| colors.elevated_surface_background.blend(fill))
    } else {
        color.text(appearance)
    };

    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .size_3()
                .rounded_xs()
                .border_1()
                .border_color(colors.border)
                .when_some(swatch, |this, swatch| this.bg(swatch)),
        )
        .child(Label::new(color.label()))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(rows: Vec<usize>, cols: Vec<usize>) -> StyleScope {
        StyleScope::Cells { rows, cols }
    }

    #[test]
    fn test_a_cell_wins_over_its_row_which_wins_over_its_column() {
        let mut styles = GridStyles::default();
        styles.set(&StyleScope::All, StyleField::Fill, CellColor::Gray);
        styles.set(
            &StyleScope::Columns(vec![1]),
            StyleField::Fill,
            CellColor::Blue,
        );
        styles.set(
            &StyleScope::Rows(vec![2]),
            StyleField::Fill,
            CellColor::Green,
        );
        styles.set(&cells(vec![2], vec![1]), StyleField::Fill, CellColor::Red);

        assert_eq!(styles.style_for(2, 1).fill, Some(CellColor::Red));
        assert_eq!(styles.style_for(2, 0).fill, Some(CellColor::Green));
        assert_eq!(styles.style_for(0, 1).fill, Some(CellColor::Blue));
        assert_eq!(styles.style_for(0, 0).fill, Some(CellColor::Gray));
    }

    #[test]
    fn test_clearing_one_cell_cancels_what_its_row_paints() {
        let mut styles = GridStyles::default();
        styles.set(
            &StyleScope::Rows(vec![3]),
            StyleField::Fill,
            CellColor::Green,
        );
        styles.set(
            &StyleScope::Columns(vec![1]),
            StyleField::Text,
            CellColor::Blue,
        );

        styles.set(&cells(vec![3], vec![1]), StyleField::Both, CellColor::None);

        // The explicit "no color" outranks both, and only on that one cell.
        assert_eq!(styles.style_for(3, 1).fill, Some(CellColor::None));
        assert_eq!(styles.style_for(3, 1).fill_color(Appearance::Dark), None);
        assert_eq!(styles.style_for(3, 1).text_color(Appearance::Dark), None);
        assert_eq!(styles.style_for(3, 0).fill, Some(CellColor::Green));
        assert_eq!(styles.style_for(9, 1).text, Some(CellColor::Blue));
    }

    #[test]
    fn test_clearing_a_cell_with_nothing_under_it_leaves_no_entry() {
        let mut styles = GridStyles::default();
        styles.set(
            &cells(vec![1], vec![1]),
            StyleField::Fill,
            CellColor::Yellow,
        );
        styles.set(&cells(vec![1], vec![1]), StyleField::Both, CellColor::None);
        assert!(styles.is_empty());
    }

    #[test]
    fn test_clearing_a_row_removes_its_entry() {
        let mut styles = GridStyles::default();
        styles.set(&StyleScope::Rows(vec![2]), StyleField::Fill, CellColor::Red);
        styles.set(
            &StyleScope::Rows(vec![2]),
            StyleField::Both,
            CellColor::None,
        );
        assert!(styles.is_empty());
    }

    #[test]
    fn test_the_two_colors_are_kept_apart() {
        let mut styles = GridStyles::default();
        styles.set(
            &cells(vec![1], vec![1]),
            StyleField::Fill,
            CellColor::Yellow,
        );
        styles.set(&cells(vec![1], vec![1]), StyleField::Text, CellColor::Red);

        let style = styles.style_for(1, 1);
        assert_eq!(style.fill, Some(CellColor::Yellow));
        assert_eq!(style.text, Some(CellColor::Red));
    }

    #[test]
    fn test_painting_a_row_covers_the_cells_inside_it() {
        let mut styles = GridStyles::default();
        styles.set(
            &cells(vec![3], vec![0, 1]),
            StyleField::Fill,
            CellColor::Red,
        );
        styles.set(&cells(vec![3], vec![0]), StyleField::Text, CellColor::Blue);
        styles.set(
            &StyleScope::Rows(vec![3]),
            StyleField::Fill,
            CellColor::Green,
        );

        assert_eq!(styles.style_for(3, 0).fill, Some(CellColor::Green));
        assert_eq!(styles.style_for(3, 1).fill, Some(CellColor::Green));
        // The other color the cell carried is left alone.
        assert_eq!(styles.style_for(3, 0).text, Some(CellColor::Blue));
    }

    #[test]
    fn test_painting_everything_drops_the_finer_entries() {
        let mut styles = GridStyles::default();
        styles.set(&StyleScope::Rows(vec![1]), StyleField::Fill, CellColor::Red);
        styles.set(
            &StyleScope::Columns(vec![2]),
            StyleField::Fill,
            CellColor::Blue,
        );
        styles.set(&StyleScope::All, StyleField::Fill, CellColor::Yellow);

        assert_eq!(styles.style_for(1, 2).fill, Some(CellColor::Yellow));
        assert_eq!(styles.style_for(9, 9).fill, Some(CellColor::Yellow));

        styles.set(&StyleScope::All, StyleField::Both, CellColor::None);
        assert!(styles.is_empty());
    }

    #[test]
    fn test_inserting_rows_pushes_the_colors_below_down() {
        let mut styles = GridStyles::default();
        for row in [0, 2, 5] {
            styles.set(
                &StyleScope::Rows(vec![row]),
                StyleField::Fill,
                CellColor::Red,
            );
        }
        styles.set(&cells(vec![4], vec![1]), StyleField::Text, CellColor::Blue);

        styles.insert_rows(2, 3);

        assert_eq!(styles.style_for(0, 0).fill, Some(CellColor::Red));
        assert_eq!(styles.style_for(2, 0).fill, None);
        assert_eq!(styles.style_for(5, 0).fill, Some(CellColor::Red));
        assert_eq!(styles.style_for(8, 0).fill, Some(CellColor::Red));
        assert_eq!(styles.style_for(7, 1).text, Some(CellColor::Blue));
    }

    #[test]
    fn test_inserting_at_the_ends() {
        let mut styles = GridStyles::default();
        styles.set(
            &StyleScope::Rows(vec![0, 1]),
            StyleField::Fill,
            CellColor::Red,
        );
        styles.insert_rows(0, 1);
        assert_eq!(styles.style_for(0, 0).fill, None);
        assert_eq!(styles.style_for(1, 0).fill, Some(CellColor::Red));
        assert_eq!(styles.style_for(2, 0).fill, Some(CellColor::Red));

        // Appending past the last painted row leaves everything where it was.
        styles.insert_rows(9, 2);
        assert_eq!(styles.style_for(1, 0).fill, Some(CellColor::Red));
        assert_eq!(styles.style_for(2, 0).fill, Some(CellColor::Red));
    }

    #[test]
    fn test_deleting_rows_closes_the_gap_and_can_be_undone() {
        let mut styles = GridStyles::default();
        for (row, color) in CellColor::PALETTE.into_iter().take(6).enumerate() {
            styles.set(&StyleScope::Rows(vec![row]), StyleField::Text, color);
        }
        styles.set(
            &cells(vec![5], vec![2]),
            StyleField::Fill,
            CellColor::Orange,
        );
        let before = styles.clone();

        let removed = styles.delete_rows(&[1, 3, 4]);

        assert_eq!(styles.style_for(0, 0).text, Some(CellColor::Yellow));
        assert_eq!(styles.style_for(1, 0).text, Some(CellColor::Blue));
        assert_eq!(styles.style_for(2, 0).text, Some(CellColor::Purple));
        assert_eq!(styles.style_for(2, 2).fill, Some(CellColor::Orange));
        assert_eq!(styles.style_for(3, 0).text, None);

        styles.restore_rows(&[1, 3, 4], &removed);
        assert_eq!(styles, before);
    }

    #[test]
    fn test_deleting_the_last_row_and_out_of_order_input() {
        let mut styles = GridStyles::default();
        styles.set(&StyleScope::Rows(vec![2]), StyleField::Fill, CellColor::Red);
        styles.set(
            &StyleScope::Rows(vec![4]),
            StyleField::Fill,
            CellColor::Blue,
        );

        let removed = styles.delete_rows(&[4, 0, 0]);

        assert_eq!(styles.style_for(1, 0).fill, Some(CellColor::Red));
        assert_eq!(styles.style_for(3, 0).fill, None);

        styles.restore_rows(&[4, 0, 0], &removed);
        assert_eq!(styles.style_for(2, 0).fill, Some(CellColor::Red));
        assert_eq!(styles.style_for(4, 0).fill, Some(CellColor::Blue));
    }

    #[test]
    fn test_columns_shift_the_same_way() {
        let mut styles = GridStyles::default();
        styles.set(
            &StyleScope::Columns(vec![0, 2]),
            StyleField::Fill,
            CellColor::Green,
        );
        styles.set(&cells(vec![1], vec![3]), StyleField::Fill, CellColor::Red);

        styles.insert_columns(1, 2);
        assert_eq!(styles.style_for(0, 0).fill, Some(CellColor::Green));
        assert_eq!(styles.style_for(0, 4).fill, Some(CellColor::Green));
        assert_eq!(styles.style_for(1, 5).fill, Some(CellColor::Red));

        let before = styles.clone();
        let removed = styles.delete_columns(&[1, 2]);
        assert_eq!(styles.style_for(0, 0).fill, Some(CellColor::Green));
        assert_eq!(styles.style_for(0, 2).fill, Some(CellColor::Green));
        assert_eq!(styles.style_for(1, 3).fill, Some(CellColor::Red));

        styles.restore_columns(&[1, 2], &removed);
        assert_eq!(styles, before);
    }

    #[test]
    fn test_a_deletion_that_takes_a_painted_row_gives_it_back_on_undo() {
        let mut styles = GridStyles::default();
        styles.set(&StyleScope::Rows(vec![2]), StyleField::Fill, CellColor::Red);
        let before = styles.clone();

        let mut shift = StyleShift::delete_rows(vec![2]);
        shift.apply(&mut styles);
        assert!(styles.is_empty());

        shift.revert(&mut styles);
        assert_eq!(styles, before);
    }

    #[test]
    fn test_reverting_an_insert_drops_what_was_painted_on_the_new_rows() {
        let mut styles = GridStyles::default();
        styles.set(&StyleScope::Rows(vec![1]), StyleField::Fill, CellColor::Red);

        let mut shift = StyleShift::InsertRows { at: 1, count: 2 };
        shift.apply(&mut styles);
        assert_eq!(styles.style_for(3, 0).fill, Some(CellColor::Red));

        // A color painted onto one of the new rows goes away with the row.
        styles.set(
            &StyleScope::Rows(vec![1]),
            StyleField::Fill,
            CellColor::Blue,
        );
        shift.revert(&mut styles);
        assert_eq!(styles.style_for(1, 0).fill, Some(CellColor::Red));
        assert_eq!(styles.rows.len(), 1);
    }

    #[test]
    fn test_json_round_trip() {
        let mut styles = GridStyles::default();
        styles.set(&StyleScope::All, StyleField::Text, CellColor::Gray);
        styles.set(
            &StyleScope::Rows(vec![0, 7]),
            StyleField::Fill,
            CellColor::Yellow,
        );
        styles.set(
            &StyleScope::Columns(vec![3]),
            StyleField::Fill,
            CellColor::Blue,
        );
        styles.set(
            &cells(vec![2], vec![1]),
            StyleField::Both,
            CellColor::Purple,
        );
        // An explicit "no color" on one cell has to survive the round trip too.
        styles.set(&cells(vec![7], vec![0]), StyleField::Fill, CellColor::None);

        let json = styles
            .to_json()
            .expect("serializing the colors should succeed")
            .expect("a painted table should produce a payload");
        assert_eq!(
            GridStyles::from_json(&json).expect("the payload should parse"),
            styles
        );

        // Nothing painted means the stored row goes away instead.
        assert_eq!(
            GridStyles::default()
                .to_json()
                .expect("serializing nothing should succeed"),
            None
        );
    }

    #[test]
    fn test_unknown_keys_and_colors_are_ignored() {
        let json = r#"{
            "default_style": { "fill": "yellow", "future": 1 },
            "rows": { "2": { "text": "red" }, "3": { "fill": "chartreuse" } },
            "cells": [
                { "row": 1, "col": 1, "fill": "blue", "future": true },
                { "row": 4, "col": 4, "text": "ultraviolet" }
            ],
            "future": { "anything": [] }
        }"#;

        let styles = GridStyles::from_json(json).expect("unknown keys should be skipped");
        assert_eq!(styles.style_for(0, 0).fill, Some(CellColor::Yellow));
        assert_eq!(styles.style_for(2, 0).text, Some(CellColor::Red));
        assert_eq!(styles.style_for(1, 1).fill, Some(CellColor::Blue));
        // The two entries naming a color this version does not know are gone,
        // rather than taking the rest of the file's colors down with them.
        assert!(!styles.rows.contains_key(&3));
        assert!(!styles.cells.contains_key(&(4, 4)));
    }

    #[test]
    fn test_the_newer_paint_wins_when_the_stored_colors_arrive_late() {
        let mut stored = GridStyles::default();
        stored.set(&StyleScope::Rows(vec![1]), StyleField::Fill, CellColor::Red);
        stored.set(&StyleScope::Rows(vec![2]), StyleField::Fill, CellColor::Red);

        let mut painted = GridStyles::default();
        painted.set(
            &StyleScope::Rows(vec![2]),
            StyleField::Fill,
            CellColor::Blue,
        );
        painted.set(
            &StyleScope::Rows(vec![3]),
            StyleField::Text,
            CellColor::Green,
        );

        let merged = painted.merged_over(&stored);
        assert_eq!(merged.style_for(1, 0).fill, Some(CellColor::Red));
        assert_eq!(merged.style_for(2, 0).fill, Some(CellColor::Blue));
        assert_eq!(merged.style_for(3, 0).text, Some(CellColor::Green));
    }

    #[test]
    fn test_both_appearances_get_a_readable_color() {
        for color in CellColor::PALETTE {
            for appearance in [Appearance::Light, Appearance::Dark] {
                if color == CellColor::None {
                    assert_eq!(color.fill(appearance), None);
                    assert_eq!(color.text(appearance), None);
                    continue;
                }
                let fill = color
                    .fill(appearance)
                    .expect("a palette color should have a fill");
                assert!(
                    fill.a < 1.0,
                    "the fill of {} has to stay translucent",
                    color.label()
                );
                let text = color
                    .text(appearance)
                    .expect("a palette color should have a text color");
                assert_eq!(text.a, 1.0);
                match appearance {
                    Appearance::Light => assert!(text.l < 0.5),
                    Appearance::Dark => assert!(text.l > 0.5),
                }
            }
        }
    }

    #[test]
    fn test_before_deletion_inverts_after_deletion() {
        let deleted = [1usize, 3, 4];
        for index in 0..12 {
            if let Some(shifted) = after_deletion(index, &deleted) {
                assert_eq!(before_deletion(shifted, &deleted), index);
            }
        }
    }

    #[gpui::test]
    fn test_only_the_actions_carrying_a_color_are_hidden(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            command_palette_hooks::init(cx);
            hide_color_actions_from_the_command_palette(cx);

            let filter =
                CommandPaletteFilter::try_global(cx).expect("the filter should have been set up");
            assert!(filter.is_hidden(&SetFillColor::default()));
            assert!(filter.is_hidden(&SetTextColor::default()));
            assert!(
                !filter.is_hidden(&ClearCellColors),
                "the one action the palette can dispatch usefully should stay"
            );
        });
    }
}

#[cfg(test)]
mod grid_tests {
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use project::ProjectPath;
    use util::{path, rel_path::rel_path};
    use workspace::{SaveIntent, item::Item};

    use super::*;
    use crate::{
        ClearCellColors, DeleteColumns, DeleteRows, InsertColumnLeft, InsertRowAbove,
        SelectAllCells, SetFillColor, SetTextColor, ToggleHeaderRow,
        grid::GridRow,
        grid_editing::tests::{open_grid, open_grid_in_workspace},
        types::{AnyColumn, DisplayRow},
    };

    fn body_cell(row: usize, col: usize) -> GridCell {
        GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
    }

    fn select_range(
        pane: &Entity<TabularDataPreviewPane>,
        from: GridCell,
        to: GridCell,
        cx: &mut VisualTestContext,
    ) {
        pane.update(cx, |pane, cx| {
            pane.select_cell(from, false, cx);
            pane.select_cell(to, true, cx);
        });
    }

    fn settle_save(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(STYLE_SAVE_DEBOUNCE * 2);
        cx.run_until_parked();
    }

    fn painted(pane: &Entity<TabularDataPreviewPane>, cx: &mut VisualTestContext) -> GridStyles {
        cx.update(|_window, cx| pane.read(cx).styles.read(cx).styles().clone())
    }

    fn stored(key: &str, cx: &mut VisualTestContext) -> Option<GridStyles> {
        cx.update(|_window, cx| {
            let payload = KeyValueStore::global(cx)
                .read_kvp(key)
                .expect("reading the key value store should succeed")?;
            Some(GridStyles::from_json(&payload).expect("the stored payload should parse"))
        })
    }

    fn styles_key(pane: &Entity<TabularDataPreviewPane>, cx: &mut VisualTestContext) -> String {
        cx.update(|_window, cx| styles_key_for_editor(pane.read(cx).editor(), cx))
            .expect("a grid on a saved file should have a key")
    }

    #[gpui::test]
    async fn test_painting_a_range_colors_its_cells(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        select_range(&pane, body_cell(0, 0), body_cell(1, 1), &mut cx);
        cx.dispatch_action(SetFillColor {
            color: CellColor::Yellow,
        });
        cx.run_until_parked();

        pane.read_with(&cx, |pane, cx| {
            let styles = pane.styles.read(cx).styles();
            // The header is line 0, so the two body rows are lines 1 and 2.
            assert_eq!(styles.style_for(1, 0).fill, Some(CellColor::Yellow));
            assert_eq!(styles.style_for(2, 1).fill, Some(CellColor::Yellow));
            assert_eq!(styles.style_for(0, 0).fill, None);
            let paint = pane.cell_paint(body_cell(0, 0), false, false, cx);
            assert!(paint.background.is_some());
            assert!(paint.text.is_none());
        });
    }

    #[gpui::test]
    async fn test_a_row_selection_is_one_entry_and_reaches_every_column(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\nd,e,f\ng,h,i", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(1)), false, cx);
        });
        cx.dispatch_action(SetTextColor {
            color: CellColor::Blue,
        });
        cx.run_until_parked();

        let styles = painted(&pane, &mut cx);
        assert_eq!(styles.rows.len(), 1);
        assert!(styles.cells.is_empty());
        for col in 0..3 {
            assert_eq!(styles.style_for(2, col).text, Some(CellColor::Blue));
        }
        assert_eq!(styles.style_for(1, 0).text, None);
    }

    #[gpui::test]
    async fn test_selecting_everything_paints_the_whole_table(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        cx.dispatch_action(SelectAllCells);
        cx.dispatch_action(SetFillColor {
            color: CellColor::Gray,
        });
        cx.run_until_parked();

        let styles = painted(&pane, &mut cx);
        // One entry covers the table however many rows it has, the header too.
        assert!(styles.rows.is_empty() && styles.cols.is_empty() && styles.cells.is_empty());
        for row in 0..4 {
            assert_eq!(styles.style_for(row, 0).fill, Some(CellColor::Gray));
        }
    }

    #[gpui::test]
    async fn test_clearing_one_cell_beats_the_color_of_its_row(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(0)), false, cx);
        });
        cx.dispatch_action(SetFillColor {
            color: CellColor::Green,
        });
        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(body_cell(0, 1), false, cx);
        });
        cx.dispatch_action(ClearCellColors);
        cx.run_until_parked();

        pane.read_with(&cx, |pane, cx| {
            let styles = pane.styles.read(cx).styles();
            assert_eq!(styles.style_for(1, 0).fill, Some(CellColor::Green));
            assert_eq!(styles.style_for(1, 1).fill, Some(CellColor::None));
            assert!(
                pane.cell_paint(body_cell(0, 1), false, false, cx)
                    .background
                    .is_none(),
                "the cleared cell should draw no fill at all"
            );
        });
    }

    #[gpui::test]
    async fn test_inserting_and_deleting_rows_carry_the_colors(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(1)), false, cx);
        });
        cx.dispatch_action(SetFillColor {
            color: CellColor::Green,
        });
        cx.run_until_parked();
        assert_eq!(
            painted(&pane, &mut cx).style_for(2, 0).fill,
            Some(CellColor::Green)
        );

        // A row inserted above the painted one pushes its color down with it.
        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(0)), false, cx);
        });
        cx.dispatch_action(InsertRowAbove);
        cx.run_until_parked();
        let styles = painted(&pane, &mut cx);
        assert_eq!(styles.style_for(2, 0).fill, None);
        assert_eq!(styles.style_for(3, 0).fill, Some(CellColor::Green));

        // Deleting it again brings the color back up.
        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(0)), false, cx);
        });
        cx.dispatch_action(DeleteRows);
        cx.run_until_parked();
        assert_eq!(
            painted(&pane, &mut cx).style_for(2, 0).fill,
            Some(CellColor::Green)
        );
    }

    #[gpui::test]
    async fn test_undo_and_redo_take_the_colors_with_them(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(1)), false, cx);
        });
        cx.dispatch_action(SetFillColor {
            color: CellColor::Green,
        });
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(0)), false, cx);
        });
        cx.dispatch_action(InsertRowAbove);
        cx.run_until_parked();
        assert_eq!(
            painted(&pane, &mut cx).style_for(3, 0).fill,
            Some(CellColor::Green)
        );

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(&cx, |editor, cx| editor.text(cx)),
            "a,b\nc,d\ne,f"
        );
        assert_eq!(
            painted(&pane, &mut cx).style_for(2, 0).fill,
            Some(CellColor::Green),
            "undoing the insert should bring the color back up with the row"
        );

        cx.dispatch_action(editor::actions::Redo);
        cx.run_until_parked();
        assert_eq!(
            painted(&pane, &mut cx).style_for(3, 0).fill,
            Some(CellColor::Green),
            "redoing it should push the color back down"
        );
    }

    #[gpui::test]
    async fn test_undoing_a_row_deletion_gives_its_color_back(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(0)), false, cx);
        });
        cx.dispatch_action(SetFillColor {
            color: CellColor::Red,
        });
        cx.run_until_parked();

        cx.dispatch_action(DeleteRows);
        cx.run_until_parked();
        assert!(painted(&pane, &mut cx).is_empty());

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(
            painted(&pane, &mut cx).style_for(1, 0).fill,
            Some(CellColor::Red)
        );
    }

    #[gpui::test]
    async fn test_a_text_side_edit_leaves_the_colors_where_they_are(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(1)), false, cx);
        });
        cx.dispatch_action(SetFillColor {
            color: CellColor::Red,
        });
        cx.run_until_parked();

        // An edit the grid did not make has no recorded shift, so undoing it
        // must not move the colors either.
        let buffer = editor
            .read_with(&cx, |editor, cx| editor.buffer().read(cx).as_singleton())
            .expect("the fixture editor should have a singleton buffer");
        buffer.update(&mut cx, |buffer, cx| {
            buffer.edit([(0..0, "x,y\n")], None, cx)
        });
        cx.executor()
            .advance_clock(crate::parser::REPARSE_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(
            painted(&pane, &mut cx).style_for(2, 0).fill,
            Some(CellColor::Red)
        );

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(
            painted(&pane, &mut cx).style_for(2, 0).fill,
            Some(CellColor::Red)
        );
    }

    #[gpui::test]
    async fn test_inserting_and_deleting_columns_carry_the_colors(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\nd,e,f", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_column(AnyColumn(2), false, cx);
        });
        cx.dispatch_action(SetTextColor {
            color: CellColor::Purple,
        });
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.select_column(AnyColumn(0), false, cx);
        });
        cx.dispatch_action(InsertColumnLeft);
        cx.run_until_parked();
        let styles = painted(&pane, &mut cx);
        assert_eq!(styles.style_for(1, 3).text, Some(CellColor::Purple));
        assert_eq!(styles.style_for(1, 2).text, None);

        pane.update(&mut cx, |pane, cx| {
            pane.select_column(AnyColumn(0), false, cx);
        });
        cx.dispatch_action(DeleteColumns);
        cx.run_until_parked();
        assert_eq!(
            painted(&pane, &mut cx).style_for(1, 2).text,
            Some(CellColor::Purple)
        );
    }

    /// The shift is worked out before the edit is attempted, and the edit
    /// engine hands back no transaction when nothing reached the buffer. The
    /// cells never moved, so neither may their colors - and there would be no
    /// undo step left to take them back if they did.
    #[gpui::test]
    async fn test_a_shift_with_no_transaction_behind_it_leaves_the_colors_alone(
        cx: &mut TestAppContext,
    ) {
        let (pane, _editor, mut cx) = open_grid(
            "a,b,c
d,e,f",
            cx,
        )
        .await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_column(AnyColumn(2), false, cx);
        });
        cx.dispatch_action(SetTextColor {
            color: CellColor::Purple,
        });
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.record_style_shift(None, StyleShift::InsertColumns { at: 0, count: 1 }, cx);
        });
        cx.run_until_parked();

        let styles = painted(&pane, &mut cx);
        assert_eq!(
            styles.style_for(1, 2).text,
            Some(CellColor::Purple),
            "the color stays on the cell that never moved"
        );
        assert_eq!(styles.style_for(1, 3).text, None);

        // And the same for a deletion, which would otherwise pull every color
        // past it one column to the left.
        pane.update(&mut cx, |pane, cx| {
            pane.record_style_shift(None, StyleShift::delete_columns(vec![0]), cx);
        });
        cx.run_until_parked();

        let styles = painted(&pane, &mut cx);
        assert_eq!(styles.style_for(1, 2).text, Some(CellColor::Purple));
        assert_eq!(styles.style_for(1, 1).text, None);
    }

    #[gpui::test]
    async fn test_deleting_the_header_row_pulls_the_colors_up(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(1)), false, cx);
        });
        cx.dispatch_action(SetFillColor {
            color: CellColor::Orange,
        });
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Header, false, cx);
        });
        cx.dispatch_action(DeleteRows);
        cx.run_until_parked();

        assert_eq!(
            editor.read_with(&cx, |editor, cx| editor.text(cx)),
            "c,d\ne,f"
        );
        assert_eq!(
            painted(&pane, &mut cx).style_for(1, 0).fill,
            Some(CellColor::Orange)
        );
    }

    #[gpui::test]
    async fn test_toggling_the_header_row_leaves_the_colors_in_place(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(body_cell(0, 0), false, cx);
        });
        cx.dispatch_action(SetFillColor {
            color: CellColor::Red,
        });
        cx.run_until_parked();

        cx.dispatch_action(ToggleHeaderRow);
        cx.run_until_parked();

        // Line 1 is an ordinary row now, and it is still the one painted red.
        pane.read_with(&cx, |pane, cx| {
            assert!(!pane.has_header_row());
            assert!(
                pane.cell_paint(body_cell(1, 0), false, false, cx)
                    .background
                    .is_some()
            );
            assert!(
                pane.cell_paint(body_cell(0, 0), false, false, cx)
                    .background
                    .is_none()
            );
        });
    }

    #[gpui::test]
    async fn test_two_grids_on_one_file_share_their_colors(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;
        let second = cx.update(|window, cx| TabularDataPreviewPane::new(&editor, window, cx));
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(body_cell(0, 0), false, cx);
            pane.set_selection_color(StyleField::Fill, CellColor::Blue, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            painted(&second, &mut cx).style_for(1, 0).fill,
            Some(CellColor::Blue),
            "the second grid should be looking at the same colors"
        );

        // And one row is written, rather than the two overwriting each other.
        settle_save(&mut cx);
        let key = styles_key(&pane, &mut cx);
        assert_eq!(
            stored(&key, &mut cx)
                .expect("the colors should reach the store")
                .style_for(1, 0)
                .fill,
            Some(CellColor::Blue)
        );
    }

    #[gpui::test]
    async fn test_the_colors_are_stored_and_read_back(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d", cx).await;
        let key = styles_key(&pane, &mut cx);

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(body_cell(0, 0), false, cx);
        });
        cx.dispatch_action(SetFillColor {
            color: CellColor::Blue,
        });
        settle_save(&mut cx);

        assert_eq!(
            stored(&key, &mut cx)
                .expect("the colors should reach the store")
                .style_for(1, 0)
                .fill,
            Some(CellColor::Blue)
        );

        // Clearing them deletes the row rather than leaving an empty payload.
        cx.dispatch_action(ClearCellColors);
        settle_save(&mut cx);
        assert_eq!(stored(&key, &mut cx), None);
    }

    #[gpui::test]
    async fn test_closing_the_tab_inside_the_debounce_still_stores_the_colors(
        cx: &mut TestAppContext,
    ) {
        let (workspace, pane, _editor, mut cx) = open_grid_in_workspace("a,b\nc,d", cx).await;
        let key = styles_key(&pane, &mut cx);
        let item_id = pane.entity_id();

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(body_cell(0, 0), false, cx);
            pane.set_selection_color(StyleField::Fill, CellColor::Red, cx);
        });
        cx.run_until_parked();
        drop(pane);

        // The tab goes away well inside the 500 ms the write is waiting out.
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
                })
            })
            .await
            .expect("closing the grid tab should succeed");
        cx.run_until_parked();

        settle_save(&mut cx);
        assert_eq!(
            stored(&key, &mut cx)
                .expect("a closed tab's last colors should still be stored")
                .style_for(1, 0)
                .fill,
            Some(CellColor::Red)
        );
    }

    #[gpui::test]
    async fn test_a_payload_naming_an_unknown_color_keeps_the_rest(cx: &mut TestAppContext) {
        let (workspace, pane, editor, mut cx) = open_grid_in_workspace("a,b\nc,d", cx).await;
        let key = styles_key(&pane, &mut cx);
        let item_id = pane.entity_id();

        let payload = r#"{"rows":{"1":{"fill":"red"},"2":{"fill":"chartreuse"}}}"#;
        let store = cx.update(|_window, cx| KeyValueStore::global(cx));
        store
            .write_kvp(key, payload.to_owned())
            .await
            .expect("writing the fixture payload should succeed");

        // The shared model has to go before a fresh grid reads the row back.
        drop(pane);
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
                })
            })
            .await
            .expect("closing the grid tab should succeed");
        cx.run_until_parked();

        let reopened = cx.update(|window, cx| TabularDataPreviewPane::new(&editor, window, cx));
        cx.run_until_parked();
        let styles = painted(&reopened, &mut cx);
        assert_eq!(styles.style_for(1, 0).fill, Some(CellColor::Red));
        assert_eq!(styles.style_for(2, 0).fill, None);
    }

    #[gpui::test]
    async fn test_saving_under_a_new_name_moves_the_colors(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;
        let old_key = styles_key(&pane, &mut cx);

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(body_cell(0, 0), false, cx);
        });
        cx.dispatch_action(SetFillColor {
            color: CellColor::Gray,
        });
        settle_save(&mut cx);
        assert!(stored(&old_key, &mut cx).is_some());

        let (project, worktree_id) = cx.update(|_window, cx| {
            let project = editor
                .read(cx)
                .project()
                .cloned()
                .expect("the fixture editor should belong to a project");
            let worktree_id = project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("the fixture project should have a worktree")
                .read(cx)
                .id();
            (project, worktree_id)
        });

        pane.update_in(&mut cx, |pane, window, cx| {
            Item::save_as(
                pane,
                project,
                ProjectPath {
                    worktree_id,
                    path: rel_path("copy.csv").into(),
                },
                window,
                cx,
            )
        })
        .await
        .expect("saving the fixture under a new name should succeed");
        settle_save(&mut cx);

        let new_key = styles_key(&pane, &mut cx);
        assert!(
            new_key.ends_with("copy.csv"),
            "the colors should follow the file to {new_key}"
        );
        assert_eq!(
            stored(&new_key, &mut cx)
                .expect("the new path should hold the colors")
                .style_for(1, 0)
                .fill,
            Some(CellColor::Gray)
        );
        assert_eq!(
            stored(&old_key, &mut cx),
            None,
            "the row of the path the file left should be gone"
        );
        assert_eq!(
            old_key,
            format!("{STYLE_KEY_PREFIX}{}", path!("/project/data.csv"))
        );
    }

    #[gpui::test]
    async fn test_a_buffer_with_no_file_says_its_colors_are_not_stored(cx: &mut TestAppContext) {
        let (_pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;
        let buffer = cx.update(|_window, cx| cx.new(|cx| language::Buffer::local("k,v\n1,2", cx)));
        let project = cx
            .update(|_window, cx| editor.read(cx).project().cloned())
            .expect("the fixture editor should belong to a project");
        let unsaved = cx.update(|window, cx| {
            cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx))
        });
        let pane = cx.update(|window, cx| TabularDataPreviewPane::new(&unsaved, window, cx));
        cx.run_until_parked();

        assert_eq!(
            cx.update(|_window, cx| styles_key_for_editor(&unsaved, cx)),
            None
        );

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(body_cell(0, 0), false, cx);
            pane.set_selection_color(StyleField::Fill, CellColor::Red, cx);
        });
        cx.run_until_parked();

        pane.read_with(&cx, |pane, cx| {
            // The color still shows for as long as the tab is open.
            assert_eq!(
                pane.styles.read(cx).styles().style_for(1, 0).fill,
                Some(CellColor::Red)
            );
            assert_eq!(
                pane.grid.status_message.as_deref(),
                Some("Colors aren't saved for this file")
            );
        });

        // Only once: painting again does not keep nagging.
        pane.update(&mut cx, |pane, cx| {
            pane.set_selection_color(StyleField::Fill, CellColor::Blue, cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert!(pane.grid.status_message.is_none()));
    }

    #[gpui::test]
    async fn test_an_oversized_selection_is_refused(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.grid.cursor = GridCell::new(GridRow::Header, AnyColumn(0));
            pane.grid.anchor = Some(body_cell(400_000, 0));
            pane.set_selection_color(StyleField::Fill, CellColor::Red, cx);
        });
        cx.run_until_parked();

        pane.read_with(&cx, |pane, cx| {
            assert!(pane.styles.read(cx).styles().is_empty());
            assert!(pane.grid.status_message.is_some());
        });
    }
}
