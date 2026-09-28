//! This module defines where the text should be displayed in an [`Editor`][Editor].
//!
//! Not literally though - rendering, layout and all that jazz is a responsibility of [`EditorElement`][EditorElement].
//! Instead, [`DisplayMap`] decides where Inlays/Inlay hints are displayed, when
//! to apply a soft wrap, where to add fold indicators, whether there are any tabs in the buffer that
//! we display as spaces and where to display custom blocks (like diagnostics).
//! Seems like a lot? That's because it is. [`DisplayMap`] is conceptually made up
//! of several smaller structures that form a hierarchy (starting at the bottom):
//! - [`InlayMap`] that decides where the [`Inlay`]s should be displayed.
//! - [`FoldMap`] that decides where the fold indicators should be; it also tracks parts of a source file that are currently folded.
//! - [`TabMap`] that keeps track of hard tabs in a buffer.
//! - [`WrapMap`] that handles soft wrapping.
//! - [`BlockMap`] that tracks custom blocks such as diagnostics that should be displayed within buffer.
//! - [`DisplayMap`] that adds background highlights to the regions of text.
//!   Each one of those builds on top of preceding map.
//!
//! ## Structure of the display map layers
//!
//! Each layer in the map (and the multibuffer itself to some extent) has a few
//! structures that are used to implement the public API available to the layer
//! above:
//! - a `Transform` type - this represents a region of text that the layer in
//!   question is "managing", that it transforms into a more "processed" text
//!   for the layer above. For example, the inlay map has an `enum Transform`
//!   that has two variants:
//!     - `Isomorphic`, representing a region of text that has no inlay hints (i.e.
//!       is passed through the map transparently)
//!     - `Inlay`, representing a location where an inlay hint is to be inserted.
//! - a `TransformSummary` type, which is usually a struct with two fields:
//!   [`input: TextSummary`][`TextSummary`] and [`output: TextSummary`][`TextSummary`]. Here,
//!   `input` corresponds to "text in the layer below", and `output` corresponds to the text
//!   exposed to the layer above. So in the inlay map case, a `Transform::Isomorphic`'s summary is
//!   just `input = output = summary`, where `summary` is the [`TextSummary`] stored in that
//!   variant. Conversely, a `Transform::Inlay` always has an empty `input` summary, because it's
//!   not "replacing" any text that exists on disk. The `output` is the summary of the inlay text
//!   to be injected. - Various newtype wrappers for co-ordinate spaces (e.g. [`WrapRow`]
//!   represents a row index, after soft-wrapping (and all lower layers)).
//! - A `Snapshot` type (e.g. [`InlaySnapshot`]) that captures the state of a layer at a specific
//!   point in time.
//! - various APIs which drill through the layers below to work with the underlying text. Notably:
//!   - `fn text_summary_for_offset()` returns a [`TextSummary`] for the range in the co-ordinate
//!     space that the map in question is responsible for.
//!   - `fn <A>_point_to_<B>_point()` converts a point in co-ordinate space `A` into co-ordinate
//!     space `B`.
//!   - A [`RowInfo`] iterator (e.g. [`InlayBufferRows`]) and a [`Chunk`] iterator
//!     (e.g. [`InlayChunks`])
//!   - A `sync` function (e.g. [`InlayMap::sync`]) that takes a snapshot and list of [`Edit<T>`]s,
//!     and returns a new snapshot and a list of transformed [`Edit<S>`]s. Note that the generic
//!     parameter on `Edit` changes, since these methods take in edits in the co-ordinate space of
//!     the lower layer, and return edits in their own co-ordinate space. The term "edit" is
//!     slightly misleading, since an [`Edit<T>`] doesn't tell you what changed - rather it can be
//!     thought of as a "region to invalidate". In theory, it would be correct to always use a
//!     single edit that covers the entire range. However, this would lead to lots of unnecessary
//!     recalculation.
//!
//! See the docs for the [`inlay_map`] module for a more in-depth explanation of how a single layer
//! works.
//!
//! [Editor]: crate::Editor
//! [EditorElement]: crate::element::EditorElement
//! [`TextSummary`]: multi_buffer::MBTextSummary
//! [`WrapRow`]: wrap_map::WrapRow
//! [`InlayBufferRows`]: inlay_map::InlayBufferRows
//! [`InlayChunks`]: inlay_map::InlayChunks
//! [`Edit<T>`]: text::Edit
//! [`Edit<S>`]: text::Edit
//! [`Chunk`]: language::Chunk

#[macro_use]
mod dimensions;

mod block_map;
mod crease_map;
mod custom_highlights;
mod fold_map;
mod inlay_map;
mod invisibles;
mod tab_map;
mod wrap_map;

pub use crate::display_map::{fold_map::FoldMap, inlay_map::InlayMap, tab_map::TabMap};
pub use block_map::{
    Block, BlockChunks as DisplayChunks, BlockContext, BlockId, BlockMap, BlockPlacement,
    BlockPoint, BlockProperties, BlockRows, BlockStyle, CompanionView, CompanionViewMut,
    CustomBlockId, EditorMargins, RenderBlock, StickyHeaderExcerpt,
};
pub use crease_map::*;
pub use fold_map::{
    ChunkRenderer, ChunkRendererContext, ChunkRendererId, Fold, FoldId, FoldPlaceholder, FoldPoint,
};
pub use inlay_map::{InlayOffset, InlayPoint};
use invisibles::is_standalone_grapheme;
pub use invisibles::{is_invisible, replacement};
pub use wrap_map::{WrapPoint, WrapRow, WrapSnapshot};

use crate::markdown_actions::is_closing_fence;
use collections::{HashMap, HashSet, IndexSet};
use gpui::{
    App, Context, Entity, EntityId, Font, HighlightStyle, Hsla, LineLayout, Pixels, UnderlineStyle,
    WeakEntity,
};
use language::{
    LanguageAwareStyling, Point, Subscription as BufferSubscription, TextObject, TreeSitterOptions,
    language_settings::{AllLanguageSettings, LanguageSettings},
};

use multi_buffer::{
    Anchor, AnchorRangeExt, MultiBuffer, MultiBufferOffset, MultiBufferOffsetUtf16,
    MultiBufferPoint, MultiBufferRow, MultiBufferSnapshot, RowInfo, ToOffset, ToPoint,
};
use parking_lot::Mutex;
use project::project_settings::DiagnosticSeverity;
use project::{InlayId, lsp_store::LspFoldingRange, lsp_store::TokenType};
use serde::Deserialize;
use settings::Settings;
use smallvec::SmallVec;
use sum_tree::{Bias, TreeMap};
use text::{BufferId, LineIndent, Patch};
use theme::StatusColors;
use ui::{SharedString, px};
use unicode_segmentation::UnicodeSegmentation;
use ztracing::instrument;

use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::{
    any::TypeId,
    borrow::Cow,
    fmt::Debug,
    iter,
    num::NonZeroU32,
    ops::{self, Add, Range, Sub},
    sync::Arc,
};

use crate::{
    EditorStyle, RowExt, hover_links::InlayHighlight, inlays::Inlay, movement::TextLayoutDetails,
};
use block_map::{BlockPointCursor, BlockRow, BlockSnapshot};
use fold_map::{FoldPointCursor, FoldSnapshot};
use inlay_map::{BufferOffsetToInlayPointCursor, InlaySnapshot};
use tab_map::{TabPoint, TabPointCursor, TabSnapshot};
use wrap_map::{WrapMap, WrapPatch, WrapPointCursor};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FoldStatus {
    Folded,
    Foldable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NavigationOverlayKey(TypeId);

impl NavigationOverlayKey {
    pub const fn unique<T: 'static>() -> Self {
        Self(TypeId::of::<T>())
    }
}

/// Keys for tagging text highlights.
///
/// Note the order is important as it determines the priority of the highlights, lower means higher priority
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HighlightKey {
    // Note we want semantic tokens > colorized brackets
    // to allow language server highlights to work over brackets.
    ColorizeBracket(usize),
    SemanticToken(u32),
    // below is sorted lexicographically, as there is no relevant ordering for these aside from coming after the above
    BufferSearchHighlights,
    ConsoleAnsiHighlight(usize),
    DebugStackFrameLine,
    DocumentHighlightRead,
    DocumentHighlightWrite,
    EditPredictionHighlight,
    Editor,
    HighlightOnYank,
    HighlightsTreeView(usize),
    HoverState,
    HoveredLinkState,
    InlineAssist,
    InputComposition,
    MatchingBracket,
    NavigationOverlay(NavigationOverlayKey),
    PendingInput,
    PickerPreview,
    ProjectSearchView,
    Rename,
    SearchWithinRange,
    SelectedTextHighlight,
    SyntaxTreeView(usize),
    VimExchange,
}

pub trait ToDisplayPoint {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint;
}

type TextHighlights = Arc<HashMap<HighlightKey, Arc<(HighlightStyle, Vec<Range<Anchor>>)>>>;
type SemanticTokensHighlights =
    Arc<HashMap<BufferId, (Arc<[SemanticTokenHighlight]>, Arc<HighlightStyleInterner>)>>;
type InlayHighlights = TreeMap<HighlightKey, TreeMap<InlayId, (HighlightStyle, InlayHighlight)>>;

#[derive(Debug)]
pub struct CompanionExcerptPatch {
    pub patch: Patch<MultiBufferPoint>,
    pub edited_range: Range<MultiBufferPoint>,
    pub source_excerpt_range: Range<MultiBufferPoint>,
    pub target_excerpt_range: Range<MultiBufferPoint>,
}

/// Decides how text in a [`MultiBuffer`] should be displayed in a buffer, handling inlay hints,
/// folding, hard tabs, soft wrapping, custom blocks (like diagnostics), and highlighting.
///
/// See the [module level documentation](self) for more information.
pub struct DisplayMap {
    entity_id: EntityId,
    /// The buffer that we are displaying.
    buffer: Entity<MultiBuffer>,
    buffer_subscription: BufferSubscription<MultiBufferOffset>,
    /// Decides where the [`Inlay`]s should be displayed.
    inlay_map: InlayMap,
    /// Decides where the fold indicators should be and tracks parts of a source file that are currently folded.
    fold_map: FoldMap,
    /// Keeps track of hard tabs in a buffer.
    tab_map: TabMap,
    /// Handles soft wrapping.
    wrap_map: Entity<WrapMap>,
    /// Tracks custom blocks such as diagnostics that should be displayed within buffer.
    block_map: BlockMap,
    /// Regions of text that should be highlighted.
    text_highlights: TextHighlights,
    /// Regions of inlays that should be highlighted.
    inlay_highlights: InlayHighlights,
    /// The semantic tokens from the language server.
    pub semantic_token_highlights: SemanticTokensHighlights,
    /// A container for explicitly foldable ranges, which supersede indentation based fold range suggestions.
    crease_map: CreaseMap,
    pub(crate) fold_placeholder: FoldPlaceholder,
    pub clip_at_line_ends: bool,
    pub(crate) masked: bool,
    pub(crate) diagnostics_max_severity: DiagnosticSeverity,
    pub(crate) companion: Option<(WeakEntity<DisplayMap>, Entity<Companion>)>,
    lsp_folding_crease_ids: HashMap<BufferId, Vec<CreaseId>>,
    /// The one walk of a Markdown document every snapshot of this map answers
    /// its `<details>` and setext heading folds from.
    markdown_scan_cache: MarkdownScanCache,
}

pub(crate) struct Companion {
    rhs_display_map_id: EntityId,
    rhs_custom_block_to_balancing_block: RefCell<HashMap<CustomBlockId, CustomBlockId>>,
    lhs_custom_block_to_balancing_block: RefCell<HashMap<CustomBlockId, CustomBlockId>>,
}

impl Companion {
    pub(crate) fn new(rhs_display_map_id: EntityId) -> Self {
        Self {
            rhs_display_map_id,
            rhs_custom_block_to_balancing_block: Default::default(),
            lhs_custom_block_to_balancing_block: Default::default(),
        }
    }

    pub(crate) fn is_rhs(&self, display_map_id: EntityId) -> bool {
        self.rhs_display_map_id == display_map_id
    }

    pub(crate) fn custom_block_to_balancing_block(
        &self,
        display_map_id: EntityId,
    ) -> &RefCell<HashMap<CustomBlockId, CustomBlockId>> {
        if self.is_rhs(display_map_id) {
            &self.rhs_custom_block_to_balancing_block
        } else {
            &self.lhs_custom_block_to_balancing_block
        }
    }

    pub(crate) fn convert_rows_to_companion(
        &self,
        display_map_id: EntityId,
        companion_snapshot: &MultiBufferSnapshot,
        our_snapshot: &MultiBufferSnapshot,
        bounds: Range<MultiBufferPoint>,
    ) -> Vec<CompanionExcerptPatch> {
        if self.is_rhs(display_map_id) {
            crate::split::patches_for_rhs_range(companion_snapshot, our_snapshot, bounds)
        } else {
            crate::split::patches_for_lhs_range(companion_snapshot, our_snapshot, bounds)
        }
    }

    pub(crate) fn convert_point_from_companion(
        &self,
        display_map_id: EntityId,
        our_snapshot: &MultiBufferSnapshot,
        companion_snapshot: &MultiBufferSnapshot,
        point: MultiBufferPoint,
    ) -> Range<MultiBufferPoint> {
        let patches = if self.is_rhs(display_map_id) {
            crate::split::patches_for_lhs_range(our_snapshot, companion_snapshot, point..point)
        } else {
            crate::split::patches_for_rhs_range(our_snapshot, companion_snapshot, point..point)
        };

        let Some(excerpt) = patches.into_iter().next() else {
            if cfg!(any(test, debug_assertions)) {
                assert!(
                    our_snapshot.max_point() == Point::zero(),
                    "`patches_for_*_in_range` is only allowed to return an empty vec if the multibuffer is empty"
                );
            }
            return Point::zero()..our_snapshot.max_point();
        };
        excerpt.patch.edit_for_old_position(point).new
    }

    pub(crate) fn convert_point_to_companion(
        &self,
        display_map_id: EntityId,
        our_snapshot: &MultiBufferSnapshot,
        companion_snapshot: &MultiBufferSnapshot,
        point: MultiBufferPoint,
    ) -> Range<MultiBufferPoint> {
        let patches = if self.is_rhs(display_map_id) {
            crate::split::patches_for_rhs_range(companion_snapshot, our_snapshot, point..point)
        } else {
            crate::split::patches_for_lhs_range(companion_snapshot, our_snapshot, point..point)
        };

        let Some(excerpt) = patches.into_iter().next() else {
            return Point::zero()..companion_snapshot.max_point();
        };
        excerpt.patch.edit_for_old_position(point).new
    }
}

#[derive(Default, Debug)]
pub struct HighlightStyleInterner {
    styles: IndexSet<HighlightStyle>,
}

impl HighlightStyleInterner {
    pub(crate) fn intern(&mut self, style: HighlightStyle) -> HighlightStyleId {
        HighlightStyleId(self.styles.insert_full(style).0 as u32)
    }
}

impl ops::Index<HighlightStyleId> for HighlightStyleInterner {
    type Output = HighlightStyle;

    fn index(&self, index: HighlightStyleId) -> &Self::Output {
        &self.styles[index.0 as usize]
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct HighlightStyleId(u32);

/// A `SemanticToken`, but positioned to an offset in a buffer, and stylized.
#[derive(Debug, Clone)]
pub struct SemanticTokenHighlight {
    pub range: Range<Anchor>,
    pub style: HighlightStyleId,
    pub token_type: TokenType,
    pub token_modifiers: u32,
    pub server_id: lsp::LanguageServerId,
    pub precedence: u32,
}

impl DisplayMap {
    pub fn new(
        buffer: Entity<MultiBuffer>,
        font: Font,
        font_size: Pixels,
        wrap_width: Option<Pixels>,
        buffer_header_height: u32,
        excerpt_header_height: u32,
        fold_placeholder: FoldPlaceholder,
        diagnostics_max_severity: DiagnosticSeverity,
        cx: &mut Context<Self>,
    ) -> Self {
        let tab_size = Self::tab_size(&buffer, cx);
        // Important: obtain the snapshot BEFORE creating the subscription.
        // snapshot() may call sync() which publishes edits. If we subscribe first,
        // those edits would be captured but the InlayMap would already be at the
        // post-edit state, causing a desync.
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let buffer_subscription = buffer.update(cx, |buffer, _| buffer.subscribe());
        let crease_map = CreaseMap::new(&buffer_snapshot);
        let (inlay_map, snapshot) = InlayMap::new(buffer_snapshot);
        let (fold_map, snapshot) = FoldMap::new(snapshot);
        let (tab_map, snapshot) = TabMap::new(snapshot, tab_size);
        let (wrap_map, snapshot) = WrapMap::new(snapshot, font, font_size, wrap_width, cx);
        let block_map = BlockMap::new(snapshot, buffer_header_height, excerpt_header_height);

        cx.observe(&wrap_map, |_, _, cx| cx.notify()).detach();

        DisplayMap {
            entity_id: cx.entity_id(),
            buffer,
            buffer_subscription,
            fold_map,
            inlay_map,
            tab_map,
            wrap_map,
            block_map,
            crease_map,
            fold_placeholder,
            diagnostics_max_severity,
            text_highlights: Default::default(),
            inlay_highlights: Default::default(),
            semantic_token_highlights: Default::default(),
            clip_at_line_ends: false,
            masked: false,
            companion: None,
            lsp_folding_crease_ids: HashMap::default(),
            markdown_scan_cache: MarkdownScanCache::default(),
        }
    }

    pub(crate) fn set_companion(
        &mut self,
        companion: Option<(Entity<DisplayMap>, Entity<Companion>)>,
        cx: &mut Context<Self>,
    ) {
        let this = cx.weak_entity();
        // Reverting to no companion, recompute the block map to clear spacers
        // and balancing blocks.
        let Some((companion_display_map, companion)) = companion else {
            let Some((_, companion)) = self.companion.take() else {
                return;
            };
            assert_eq!(self.entity_id, companion.read(cx).rhs_display_map_id);
            let (snapshot, _edits) = self.sync_through_wrap(cx);
            let edits = Patch::new(vec![text::Edit {
                old: WrapRow(0)
                    ..self.block_map.wrap_snapshot.borrow().max_point().row() + WrapRow(1),
                new: WrapRow(0)..snapshot.max_point().row() + WrapRow(1),
            }]);
            self.block_map.deferred_edits.set(edits);
            self.block_map.retain_blocks_raw(&mut |block| {
                if companion
                    .read(cx)
                    .lhs_custom_block_to_balancing_block
                    .borrow()
                    .values()
                    .any(|id| *id == block.id)
                {
                    return false;
                }
                true
            });
            return;
        };
        assert_eq!(self.entity_id, companion.read(cx).rhs_display_map_id);

        // Note, throwing away the wrap edits because we defer spacer computation to the first render.
        let snapshot = {
            let edits = self.buffer_subscription.consume();
            let snapshot = self.buffer.read(cx).snapshot(cx);
            let tab_size = Self::tab_size(&self.buffer, cx);
            let (snapshot, edits) = self.inlay_map.sync(snapshot, edits.into_inner());
            let (mut writer, snapshot, edits) = self.fold_map.write(snapshot, edits);
            let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
            let (_snapshot, _edits) = self
                .wrap_map
                .update(cx, |wrap_map, cx| wrap_map.sync(snapshot, edits, cx));

            let (snapshot, edits) = writer.unfold_intersecting([Anchor::Min..Anchor::Max], true);
            let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
            let (snapshot, _edits) = self
                .wrap_map
                .update(cx, |wrap_map, cx| wrap_map.sync(snapshot, edits, cx));

            self.block_map.retain_blocks_raw(&mut |block| {
                !matches!(block.placement, BlockPlacement::Replace(_))
            });
            snapshot
        };

        let (companion_wrap_snapshot, _companion_wrap_edits) =
            companion_display_map.update(cx, |dm, cx| dm.sync_through_wrap(cx));

        let edits = Patch::new(vec![text::Edit {
            old: WrapRow(0)..self.block_map.wrap_snapshot.borrow().max_point().row() + WrapRow(1),
            new: WrapRow(0)..snapshot.max_point().row() + WrapRow(1),
        }]);
        self.block_map.deferred_edits.set(edits);

        let all_blocks: Vec<_> = self.block_map.blocks_raw().map(Clone::clone).collect();

        companion_display_map.update(cx, |companion_display_map, cx| {
            // Sync folded buffers from RHS to LHS. Also clean up stale
            // entries: the block map doesn't remove buffers from
            // `folded_buffers` when they leave the multibuffer, so we
            // unfold any RHS buffers whose companion mapping is missing.
            let rhs_snapshot = self.buffer.read(cx).snapshot(cx);
            let mut buffers_to_unfold = Vec::new();
            for my_buffer in self.folded_buffers() {
                let their_buffer = rhs_snapshot
                    .diff_for_buffer_id(*my_buffer)
                    .map(|diff| diff.base_text().remote_id());

                let Some(their_buffer) = their_buffer else {
                    buffers_to_unfold.push(*my_buffer);
                    continue;
                };

                companion_display_map
                    .block_map
                    .folded_buffers
                    .insert(their_buffer);
            }
            for buffer_id in buffers_to_unfold {
                self.block_map.folded_buffers.remove(&buffer_id);
            }

            for block in all_blocks {
                let Some(their_block) = block_map::balancing_block(
                    &block.properties(),
                    snapshot.buffer(),
                    companion_wrap_snapshot.buffer(),
                    self.entity_id,
                    companion.read(cx),
                ) else {
                    continue;
                };
                let their_id = companion_display_map
                    .block_map
                    .insert_block_raw(their_block, companion_wrap_snapshot.buffer());
                companion.update(cx, |companion, _cx| {
                    companion
                        .custom_block_to_balancing_block(self.entity_id)
                        .borrow_mut()
                        .insert(block.id, their_id);
                });
            }
            let companion_edits = Patch::new(vec![text::Edit {
                old: WrapRow(0)
                    ..companion_display_map
                        .block_map
                        .wrap_snapshot
                        .borrow()
                        .max_point()
                        .row()
                        + WrapRow(1),
                new: WrapRow(0)..companion_wrap_snapshot.max_point().row() + WrapRow(1),
            }]);
            companion_display_map
                .block_map
                .deferred_edits
                .set(companion_edits);
            companion_display_map.companion = Some((this, companion.clone()));
        });

        self.companion = Some((companion_display_map.downgrade(), companion));
    }

    pub(crate) fn companion(&self) -> Option<&Entity<Companion>> {
        self.companion.as_ref().map(|(_, c)| c)
    }

    fn sync_through_wrap(&mut self, cx: &mut App) -> (WrapSnapshot, WrapPatch) {
        let tab_size = Self::tab_size(&self.buffer, cx);
        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();

        let (snapshot, edits) = self.inlay_map.sync(buffer_snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        self.wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx))
    }

    fn with_synced_companion_mut<R>(
        display_map_id: EntityId,
        companion: &Option<(WeakEntity<DisplayMap>, Entity<Companion>)>,
        cx: &mut App,
        callback: impl FnOnce(Option<CompanionViewMut<'_>>, &mut App) -> R,
    ) -> R {
        let Some((companion_display_map, companion)) = companion else {
            return callback(None, cx);
        };
        let Some(companion_display_map) = companion_display_map.upgrade() else {
            return callback(None, cx);
        };
        companion_display_map.update(cx, |companion_display_map, cx| {
            let (companion_wrap_snapshot, companion_wrap_edits) =
                companion_display_map.sync_through_wrap(cx);
            companion_display_map
                .buffer
                .update(cx, |companion_multibuffer, cx| {
                    companion.update(cx, |companion, cx| {
                        let companion_view = CompanionViewMut::new(
                            display_map_id,
                            companion_display_map.entity_id,
                            &companion_wrap_snapshot,
                            &companion_wrap_edits,
                            companion_multibuffer,
                            companion,
                            &mut companion_display_map.block_map,
                        );
                        callback(Some(companion_view), cx)
                    })
                })
        })
    }

    #[instrument(skip_all)]
    pub fn snapshot(&mut self, cx: &mut Context<Self>) -> DisplaySnapshot {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);
        let companion_wrap_data = self.companion.as_ref().and_then(|(companion_dm, _)| {
            companion_dm
                .update(cx, |dm, cx| dm.sync_through_wrap(cx))
                .ok()
        });
        let companion_ref = self.companion.as_ref().map(|(_, c)| c.read(cx));
        let companion_view = companion_wrap_data.as_ref().zip(companion_ref).map(
            |((snapshot, edits), companion)| {
                CompanionView::new(self.entity_id, snapshot, edits, companion)
            },
        );

        let block_snapshot = self
            .block_map
            .read(
                self_wrap_snapshot.clone(),
                self_wrap_edits.clone(),
                companion_view,
            )
            .snapshot;

        if let Some((companion_dm, _)) = &self.companion {
            let _ = companion_dm.update(cx, |dm, _cx| {
                if let Some((companion_snapshot, companion_edits)) = companion_wrap_data {
                    let their_companion_ref = dm.companion.as_ref().map(|(_, c)| c.read(_cx));
                    dm.block_map.read(
                        companion_snapshot,
                        companion_edits,
                        their_companion_ref.map(|c| {
                            CompanionView::new(
                                dm.entity_id,
                                &self_wrap_snapshot,
                                &self_wrap_edits,
                                c,
                            )
                        }),
                    );
                }
            });
        }

        let companion_display_snapshot = self.companion.as_ref().and_then(|(companion_dm, _)| {
            companion_dm
                .update(cx, |dm, cx| Arc::new(dm.snapshot_simple(cx)))
                .ok()
        });

        DisplaySnapshot {
            display_map_id: self.entity_id,
            companion_display_snapshot,
            block_snapshot,
            diagnostics_max_severity: self.diagnostics_max_severity,
            crease_snapshot: self.crease_map.snapshot(),
            text_highlights: self.text_highlights.clone(),
            inlay_highlights: self.inlay_highlights.clone(),
            semantic_token_highlights: self.semantic_token_highlights.clone(),
            clip_at_line_ends: self.clip_at_line_ends,
            masked: self.masked,
            use_lsp_folding_ranges: !self.lsp_folding_crease_ids.is_empty(),
            fold_placeholder: self.fold_placeholder.clone(),
            markdown_scan_cache: self.markdown_scan_cache.clone(),
        }
    }

    fn snapshot_simple(&mut self, cx: &mut Context<Self>) -> DisplaySnapshot {
        let (wrap_snapshot, wrap_edits) = self.sync_through_wrap(cx);

        let block_snapshot = self
            .block_map
            .read(wrap_snapshot, wrap_edits, None)
            .snapshot;

        DisplaySnapshot {
            display_map_id: self.entity_id,
            companion_display_snapshot: None,
            block_snapshot,
            diagnostics_max_severity: self.diagnostics_max_severity,
            crease_snapshot: self.crease_map.snapshot(),
            text_highlights: self.text_highlights.clone(),
            inlay_highlights: self.inlay_highlights.clone(),
            semantic_token_highlights: self.semantic_token_highlights.clone(),
            clip_at_line_ends: self.clip_at_line_ends,
            masked: self.masked,
            use_lsp_folding_ranges: !self.lsp_folding_crease_ids.is_empty(),
            fold_placeholder: self.fold_placeholder.clone(),
            markdown_scan_cache: self.markdown_scan_cache.clone(),
        }
    }

    pub fn crease_snapshot(&self) -> CreaseSnapshot {
        self.crease_map.snapshot()
    }

    #[instrument(skip_all)]
    pub fn set_state(&mut self, other: &DisplaySnapshot, cx: &mut Context<Self>) {
        self.fold(
            other
                .folds_in_range(MultiBufferOffset(0)..other.buffer_snapshot().len())
                .map(|fold| {
                    Crease::simple(
                        fold.range.to_offset(other.buffer_snapshot()),
                        fold.placeholder.clone(),
                    )
                })
                .collect(),
            cx,
        );
        for buffer_id in &other.block_snapshot.buffers_with_disabled_headers {
            self.disable_header_for_buffer(*buffer_id, cx);
        }
    }

    /// Creates folds for the given creases.
    #[instrument(skip_all)]
    pub fn fold<T: Clone + ToOffset>(&mut self, creases: Vec<Crease<T>>, cx: &mut Context<Self>) {
        if self.companion().is_some() {
            return;
        }

        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);

        let (snapshot, edits) = self.inlay_map.sync(buffer_snapshot.clone(), edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits, None);

        let inline = creases.iter().filter_map(|crease| {
            if let Crease::Inline {
                range, placeholder, ..
            } = crease
            {
                Some((range.clone(), placeholder.clone()))
            } else {
                None
            }
        });
        let (snapshot, edits) = fold_map.fold(inline);

        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        let blocks = creases
            .into_iter()
            .filter_map(|crease| {
                if let Crease::Block {
                    range,
                    block_height,
                    render_block,
                    block_style,
                    block_priority,
                    ..
                } = crease
                {
                    Some((
                        range,
                        render_block,
                        block_height,
                        block_style,
                        block_priority,
                    ))
                } else {
                    None
                }
            })
            .map(|(range, render, height, style, priority)| {
                let start = buffer_snapshot.anchor_before(range.start);
                let end = buffer_snapshot.anchor_after(range.end);
                BlockProperties {
                    placement: BlockPlacement::Replace(start..=end),
                    render,
                    height: Some(height),
                    style,
                    priority,
                }
            });

        self.block_map.write(snapshot, edits, None).insert(blocks);
    }

    /// Removes any folds with the given ranges.
    #[instrument(skip_all)]
    pub fn remove_folds_with_type<T: ToOffset>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        type_id: TypeId,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);

        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits, None);

        let (snapshot, edits) = fold_map.remove_folds(ranges, type_id);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (self_new_wrap_snapshot, self_new_wrap_edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        self.block_map
            .write(self_new_wrap_snapshot, self_new_wrap_edits, None);
    }

    /// Removes any folds whose ranges intersect any of the given ranges.
    #[instrument(skip_all)]
    pub fn unfold_intersecting<T: ToOffset>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        inclusive: bool,
        cx: &mut Context<Self>,
    ) -> WrapSnapshot {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let offset_ranges = ranges
            .into_iter()
            .map(|range| range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot))
            .collect::<Vec<_>>();
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);

        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits, None);

        let (snapshot, edits) =
            fold_map.unfold_intersecting(offset_ranges.iter().cloned(), inclusive);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (self_new_wrap_snapshot, self_new_wrap_edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        self.block_map
            .write(self_new_wrap_snapshot.clone(), self_new_wrap_edits, None)
            .remove_intersecting_replace_blocks(offset_ranges, inclusive);

        self_new_wrap_snapshot
    }

    #[instrument(skip_all)]
    pub fn disable_header_for_buffer(&mut self, buffer_id: BufferId, cx: &mut Context<Self>) {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);
        self.block_map
            .write(self_wrap_snapshot, self_wrap_edits, None)
            .disable_header_for_buffer(buffer_id);
    }

    #[instrument(skip_all)]
    pub fn fold_buffers(
        &mut self,
        buffer_ids: impl IntoIterator<Item = language::BufferId>,
        cx: &mut App,
    ) {
        let buffer_ids: Vec<_> = buffer_ids.into_iter().collect();

        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);

        Self::with_synced_companion_mut(
            self.entity_id,
            &self.companion,
            cx,
            |companion_view, cx| {
                self.block_map
                    .write(
                        self_wrap_snapshot.clone(),
                        self_wrap_edits.clone(),
                        companion_view,
                    )
                    .fold_buffers(buffer_ids.iter().copied(), self.buffer.read(cx), cx);
            },
        )
    }

    #[instrument(skip_all)]
    pub fn unfold_buffers(
        &mut self,
        buffer_ids: impl IntoIterator<Item = language::BufferId>,
        cx: &mut Context<Self>,
    ) {
        let buffer_ids: Vec<_> = buffer_ids.into_iter().collect();

        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);

        Self::with_synced_companion_mut(
            self.entity_id,
            &self.companion,
            cx,
            |companion_view, cx| {
                self.block_map
                    .write(
                        self_wrap_snapshot.clone(),
                        self_wrap_edits.clone(),
                        companion_view,
                    )
                    .unfold_buffers(buffer_ids.iter().copied(), self.buffer.read(cx), cx);
            },
        )
    }

    #[instrument(skip_all)]
    pub(crate) fn is_buffer_folded(&self, buffer_id: language::BufferId) -> bool {
        self.block_map.folded_buffers.contains(&buffer_id)
    }

    #[instrument(skip_all)]
    pub(crate) fn folded_buffers(&self) -> &HashSet<BufferId> {
        &self.block_map.folded_buffers
    }

    #[instrument(skip_all)]
    pub fn insert_creases(
        &mut self,
        creases: impl IntoIterator<Item = Crease<Anchor>>,
        cx: &mut Context<Self>,
    ) -> Vec<CreaseId> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        self.crease_map.insert(creases, &snapshot)
    }

    #[instrument(skip_all)]
    pub fn remove_creases(
        &mut self,
        crease_ids: impl IntoIterator<Item = CreaseId>,
        cx: &mut Context<Self>,
    ) -> Vec<(CreaseId, Range<Anchor>)> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        self.crease_map.remove(crease_ids, &snapshot)
    }

    /// Replaces the LSP folding-range creases for a single buffer.
    /// Converts the supplied buffer-anchor ranges into multi-buffer creases
    /// by mapping them through the appropriate excerpts.
    pub(super) fn set_lsp_folding_ranges(
        &mut self,
        buffer_id: BufferId,
        ranges: Vec<LspFoldingRange>,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);

        let old_ids = self
            .lsp_folding_crease_ids
            .remove(&buffer_id)
            .unwrap_or_default();
        if !old_ids.is_empty() {
            self.crease_map.remove(old_ids, &snapshot);
        }

        if ranges.is_empty() {
            return;
        }

        let base_placeholder = self.fold_placeholder.clone();
        let creases = ranges.into_iter().filter_map(|folding_range| {
            let mb_range =
                snapshot.buffer_anchor_range_to_anchor_range(folding_range.range.clone())?;
            let placeholder = if let Some(collapsed_text) = folding_range.collapsed_text {
                FoldPlaceholder {
                    render: Arc::new({
                        let collapsed_text = collapsed_text.clone();
                        move |fold_id, _fold_range, cx: &mut gpui::App| {
                            use gpui::{Element as _, ParentElement as _};
                            FoldPlaceholder::fold_element(fold_id, cx)
                                .child(collapsed_text.clone())
                                .into_any()
                        }
                    }),
                    constrain_width: false,
                    merge_adjacent: base_placeholder.merge_adjacent,
                    type_tag: base_placeholder.type_tag,
                    collapsed_text: Some(collapsed_text),
                }
            } else {
                base_placeholder.clone()
            };
            Some(Crease::simple(mb_range, placeholder))
        });

        let new_ids = self.crease_map.insert(creases, &snapshot);
        if !new_ids.is_empty() {
            self.lsp_folding_crease_ids.insert(buffer_id, new_ids);
        }
    }

    /// Removes all LSP folding-range creases for a single buffer.
    pub(super) fn clear_lsp_folding_ranges(&mut self, buffer_id: BufferId, cx: &mut Context<Self>) {
        if let Some(old_ids) = self.lsp_folding_crease_ids.remove(&buffer_id) {
            let snapshot = self.buffer.read(cx).snapshot(cx);
            self.crease_map.remove(old_ids, &snapshot);
        }
    }

    /// Returns `true` when at least one buffer has LSP folding-range creases.
    pub(super) fn has_lsp_folding_ranges(&self) -> bool {
        !self.lsp_folding_crease_ids.is_empty()
    }

    #[instrument(skip_all)]
    pub fn insert_blocks(
        &mut self,
        blocks: impl IntoIterator<Item = BlockProperties<Anchor>>,
        cx: &mut Context<Self>,
    ) -> Vec<CustomBlockId> {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);
        Self::with_synced_companion_mut(
            self.entity_id,
            &self.companion,
            cx,
            |companion_view, _cx| {
                self.block_map
                    .write(
                        self_wrap_snapshot.clone(),
                        self_wrap_edits.clone(),
                        companion_view,
                    )
                    .insert(blocks)
            },
        )
    }

    #[instrument(skip_all)]
    pub fn resize_blocks(&mut self, heights: HashMap<CustomBlockId, u32>, cx: &mut Context<Self>) {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);

        Self::with_synced_companion_mut(
            self.entity_id,
            &self.companion,
            cx,
            |companion_view, _cx| {
                self.block_map
                    .write(
                        self_wrap_snapshot.clone(),
                        self_wrap_edits.clone(),
                        companion_view,
                    )
                    .resize(heights);
            },
        )
    }

    #[instrument(skip_all)]
    pub fn replace_blocks(&mut self, renderers: HashMap<CustomBlockId, RenderBlock>) {
        self.block_map.replace_blocks(renderers);
    }

    #[instrument(skip_all)]
    pub fn remove_blocks(&mut self, ids: HashSet<CustomBlockId>, cx: &mut Context<Self>) {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);

        Self::with_synced_companion_mut(
            self.entity_id,
            &self.companion,
            cx,
            |companion_view, _cx| {
                self.block_map
                    .write(
                        self_wrap_snapshot.clone(),
                        self_wrap_edits.clone(),
                        companion_view,
                    )
                    .remove(ids);
            },
        )
    }

    #[instrument(skip_all)]
    pub fn row_for_block(
        &mut self,
        block_id: CustomBlockId,
        cx: &mut Context<Self>,
    ) -> Option<DisplayRow> {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);

        let companion_wrap_data = self.companion.as_ref().and_then(|(companion_dm, _)| {
            companion_dm
                .update(cx, |dm, cx| dm.sync_through_wrap(cx))
                .ok()
        });

        let companion_ref = self.companion.as_ref().map(|(_, c)| c.read(cx));
        let companion_view = companion_wrap_data.as_ref().zip(companion_ref).map(
            |((snapshot, edits), companion)| {
                CompanionView::new(self.entity_id, snapshot, edits, companion)
            },
        );

        let block_map = self.block_map.read(
            self_wrap_snapshot.clone(),
            self_wrap_edits.clone(),
            companion_view,
        );
        let block_row = block_map.row_for_block(block_id)?;

        if let Some((companion_dm, _)) = &self.companion {
            let _ = companion_dm.update(cx, |dm, cx| {
                if let Some((companion_snapshot, companion_edits)) = companion_wrap_data {
                    let their_companion_ref = dm.companion.as_ref().map(|(_, c)| c.read(cx));
                    dm.block_map.read(
                        companion_snapshot,
                        companion_edits,
                        their_companion_ref.map(|c| {
                            CompanionView::new(
                                dm.entity_id,
                                &self_wrap_snapshot,
                                &self_wrap_edits,
                                c,
                            )
                        }),
                    );
                }
            });
        }

        Some(DisplayRow(block_row.0))
    }

    #[instrument(skip_all)]
    pub fn highlight_text(
        &mut self,
        key: HighlightKey,
        mut ranges: Vec<Range<Anchor>>,
        style: HighlightStyle,
        merge: bool,
        cx: &App,
    ) {
        let multi_buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        match Arc::make_mut(&mut self.text_highlights).entry(key) {
            Entry::Occupied(mut slot) => match Arc::get_mut(slot.get_mut()) {
                Some((_, previous_ranges)) if merge => {
                    previous_ranges.extend(ranges);
                    previous_ranges.sort_by(|a, b| a.start.cmp(&b.start, &multi_buffer_snapshot));
                }
                Some((previous_style, previous_ranges)) => {
                    *previous_style = style;
                    *previous_ranges = ranges;
                    previous_ranges.sort_by(|a, b| a.start.cmp(&b.start, &multi_buffer_snapshot));
                }
                None if merge => {
                    ranges.extend(slot.get().1.iter().cloned());
                    ranges.sort_by(|a, b| a.start.cmp(&b.start, &multi_buffer_snapshot));
                    slot.insert(Arc::new((style, ranges)));
                }
                None => {
                    ranges.sort_by(|a, b| a.start.cmp(&b.start, &multi_buffer_snapshot));
                    slot.insert(Arc::new((style, ranges)));
                }
            },
            Entry::Vacant(slot) => {
                ranges.sort_by(|a, b| a.start.cmp(&b.start, &multi_buffer_snapshot));
                slot.insert(Arc::new((style, ranges)));
            }
        }
    }

    #[instrument(skip_all)]
    pub(crate) fn highlight_inlays(
        &mut self,
        key: HighlightKey,
        highlights: Vec<InlayHighlight>,
        style: HighlightStyle,
    ) {
        for highlight in highlights {
            let update = self.inlay_highlights.update(&key, |highlights| {
                highlights.insert(highlight.inlay, (style, highlight.clone()))
            });
            if update.is_none() {
                self.inlay_highlights.insert(
                    key,
                    TreeMap::from_ordered_entries([(highlight.inlay, (style, highlight))]),
                );
            }
        }
    }

    #[instrument(skip_all)]
    pub fn text_highlights(&self, key: HighlightKey) -> Option<(HighlightStyle, &[Range<Anchor>])> {
        let highlights = self.text_highlights.get(&key)?;
        Some((highlights.0, &highlights.1))
    }

    pub fn all_text_highlights(
        &self,
    ) -> impl Iterator<Item = (&HighlightKey, &Arc<(HighlightStyle, Vec<Range<Anchor>>)>)> {
        self.text_highlights.iter()
    }

    pub fn all_semantic_token_highlights(
        &self,
    ) -> impl Iterator<
        Item = (
            &BufferId,
            &(Arc<[SemanticTokenHighlight]>, Arc<HighlightStyleInterner>),
        ),
    > {
        self.semantic_token_highlights.iter()
    }

    pub fn clear_highlights(&mut self, key: HighlightKey) -> bool {
        let mut cleared = Arc::make_mut(&mut self.text_highlights)
            .remove(&key)
            .is_some();
        cleared |= self.inlay_highlights.remove(&key).is_some();
        cleared
    }

    pub fn clear_highlights_with(&mut self, f: &mut dyn FnMut(&HighlightKey) -> bool) -> bool {
        let mut cleared = false;
        Arc::make_mut(&mut self.text_highlights).retain(|k, _| {
            let b = !f(k);
            cleared |= b;
            b
        });
        self.inlay_highlights.retain(|k, _| {
            let b = !f(k);
            cleared |= b;
            b
        });
        cleared
    }

    pub fn set_font(&self, font: Font, font_size: Pixels, cx: &mut Context<Self>) -> bool {
        self.wrap_map
            .update(cx, |map, cx| map.set_font_with_size(font, font_size, cx))
    }

    pub fn set_wrap_width(&self, width: Option<Pixels>, cx: &mut Context<Self>) -> bool {
        self.wrap_map
            .update(cx, |map, cx| map.set_wrap_width(width, cx))
    }

    #[instrument(skip_all)]
    pub fn update_fold_widths(
        &mut self,
        widths: impl IntoIterator<Item = (ChunkRendererId, Pixels)>,
        cx: &mut Context<Self>,
    ) -> bool {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);

        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits, None);

        let (snapshot, edits) = fold_map.update_fold_widths(widths);
        let widths_changed = !edits.is_empty();
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (self_new_wrap_snapshot, self_new_wrap_edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        self.block_map
            .read(self_new_wrap_snapshot, self_new_wrap_edits, None);

        widths_changed
    }

    pub(crate) fn current_inlays(&self) -> impl Iterator<Item = &Inlay> + Default {
        self.inlay_map.current_inlays()
    }

    #[instrument(skip_all)]
    pub(crate) fn splice_inlays(
        &mut self,
        to_remove: &[InlayId],
        to_insert: Vec<Inlay>,
        cx: &mut Context<Self>,
    ) {
        if to_remove.is_empty() && to_insert.is_empty() {
            return;
        }
        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);

        let companion_wrap_data = self.companion.as_ref().and_then(|(companion_dm, _)| {
            companion_dm
                .update(cx, |dm, cx| dm.sync_through_wrap(cx))
                .ok()
        });

        let (snapshot, edits) = self.inlay_map.sync(buffer_snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        {
            let companion_ref = self.companion.as_ref().map(|(_, c)| c.read(cx));
            let companion_view = companion_wrap_data.as_ref().zip(companion_ref).map(
                |((snapshot, edits), companion)| {
                    CompanionView::new(self.entity_id, snapshot, edits, companion)
                },
            );
            self.block_map.read(snapshot, edits, companion_view);
        }

        let (snapshot, edits) = self.inlay_map.splice(to_remove, to_insert);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (self_new_wrap_snapshot, self_new_wrap_edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        let (self_wrap_snapshot, self_wrap_edits) =
            (self_new_wrap_snapshot.clone(), self_new_wrap_edits.clone());

        {
            let companion_ref = self.companion.as_ref().map(|(_, c)| c.read(cx));
            let companion_view = companion_wrap_data.as_ref().zip(companion_ref).map(
                |((snapshot, edits), companion)| {
                    CompanionView::new(self.entity_id, snapshot, edits, companion)
                },
            );
            self.block_map
                .read(self_new_wrap_snapshot, self_new_wrap_edits, companion_view);
        }

        if let Some((companion_dm, _)) = &self.companion {
            let _ = companion_dm.update(cx, |dm, cx| {
                if let Some((companion_snapshot, companion_edits)) = companion_wrap_data {
                    let their_companion_ref = dm.companion.as_ref().map(|(_, c)| c.read(cx));
                    dm.block_map.read(
                        companion_snapshot,
                        companion_edits,
                        their_companion_ref.map(|c| {
                            CompanionView::new(
                                dm.entity_id,
                                &self_wrap_snapshot,
                                &self_wrap_edits,
                                c,
                            )
                        }),
                    );
                }
            });
        }
    }

    #[instrument(skip_all)]
    fn tab_size(buffer: &Entity<MultiBuffer>, cx: &App) -> NonZeroU32 {
        if let Some(buffer) = buffer.read(cx).as_singleton().map(|buffer| buffer.read(cx)) {
            LanguageSettings::for_buffer(buffer, cx).tab_size
        } else {
            AllLanguageSettings::get_global(cx).defaults.tab_size
        }
    }

    pub fn is_rewrapping(&self, cx: &gpui::App) -> bool {
        self.wrap_map.read(cx).is_rewrapping()
    }

    pub fn invalidate_semantic_highlights(&mut self, buffer_id: BufferId) {
        Arc::make_mut(&mut self.semantic_token_highlights).remove(&buffer_id);
    }
}

#[derive(Debug, Default)]
pub(crate) struct Highlights<'a> {
    pub text_highlights: Option<&'a TextHighlights>,
    pub inlay_highlights: Option<&'a InlayHighlights>,
    pub semantic_token_highlights: Option<&'a SemanticTokensHighlights>,
    pub styles: HighlightStyles,
}

#[derive(Clone, Copy, Debug)]
pub struct EditPredictionStyles {
    pub insertion: HighlightStyle,
    pub whitespace: HighlightStyle,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct HighlightStyles {
    pub inlay_hint: Option<HighlightStyle>,
    pub edit_prediction: Option<EditPredictionStyles>,
}

#[derive(Clone)]
pub enum ChunkReplacement {
    Renderer(ChunkRenderer),
    Str(SharedString),
}

pub struct HighlightedChunk<'a> {
    pub text: &'a str,
    pub style: Option<HighlightStyle>,
    pub is_tab: bool,
    pub is_inlay: bool,
    pub replacement: Option<ChunkReplacement>,
}

impl<'a> HighlightedChunk<'a> {
    #[instrument(skip_all)]
    fn highlight_invisibles(
        self,
        editor_style: &'a EditorStyle,
    ) -> impl Iterator<Item = Self> + 'a {
        let mut text = self.text;
        let style = self.style;
        let is_tab = self.is_tab;
        let renderer = self.replacement;
        let is_inlay = self.is_inlay;
        iter::from_fn(move || {
            if text.is_empty() {
                return None;
            }
            for (offset, ch) in text.char_indices() {
                if !is_invisible(ch) {
                    continue;
                }
                let ch_end = offset + ch.len_utf8();
                if !is_standalone_grapheme(text, offset, ch_end) {
                    continue;
                }
                if offset > 0 {
                    let (prefix, suffix) = text.split_at(offset);
                    text = suffix;
                    return Some(HighlightedChunk {
                        text: prefix,
                        style,
                        is_tab,
                        is_inlay,
                        replacement: renderer.clone(),
                    });
                }
                let (invisible_text, suffix) = text.split_at(ch_end);
                text = suffix;
                let invisible_highlight = HighlightStyle {
                    background_color: Some(editor_style.status.hint_background),
                    underline: Some(UnderlineStyle {
                        color: Some(editor_style.status.hint),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..Default::default()
                };
                let invisible_style = if let Some(style) = style {
                    style.highlight(invisible_highlight)
                } else {
                    invisible_highlight
                };
                return Some(HighlightedChunk {
                    text: invisible_text,
                    style: Some(invisible_style),
                    is_tab: false,
                    is_inlay,
                    replacement: match replacement(ch) {
                        Some(replacement) => {
                            Some(ChunkReplacement::Str(SharedString::from(replacement)))
                        }
                        None => renderer.clone(),
                    },
                });
            }
            let remainder = text;
            text = "";
            Some(HighlightedChunk {
                text: remainder,
                style,
                is_tab,
                is_inlay,
                replacement: renderer.clone(),
            })
        })
    }
}

/// The name `crates/grammars/src/markdown/config.toml` gives the Markdown
/// language; also what `MarkdownPreviewView::is_markdown_file` matches on.
const MARKDOWN_LANGUAGE_NAME: &str = "Markdown";

/// How many characters of a line are read while deciding what Markdown block it
/// starts. Headings, code fences and the `<details>` tag all settle the answer
/// within the first few characters, so a single very long line cannot make the
/// gutter walk it in full. The one thing this truncates is a `</details>`
/// written more than this many characters into a line, which CommonMark would
/// not read as a block of its own anyway.
const MARKDOWN_LINE_LIMIT: usize = 256;

/// How far ahead the scan that pairs `<details>` with `</details>` looks. The
/// gutter asks for a crease once per visible row and `editor::FoldAll` asks for
/// every row in the buffer, so a `<details>` that is never closed must not walk
/// a large file to its end each time it is drawn.
const MARKDOWN_DETAILS_SCAN_LIMIT: u32 = 2_000;

/// How many characters that same scan reads before it gives up, so that a
/// region of very long lines costs no more than one of ordinary ones. At two
/// hundred characters per row it is well past any prose the row limit allows,
/// and a scan cut short by it offers no fold rather than a wrong one.
const MARKDOWN_DETAILS_SCAN_CHAR_LIMIT: usize = 400_000;

/// How many characters the whole-document walk behind [`MarkdownScan`] reads
/// before it gives up.
///
/// The walk is redone once for every version of the buffer, so this is what a
/// keystroke in a Markdown file costs at worst — where the limit above is what
/// one row costs on every frame. A million characters is twice the largest
/// document this fork's own notes run to and a few milliseconds of walking; a
/// file past it keeps the bounded per-row behaviour rather than paying for a
/// walk on every edit.
const MARKDOWN_DOCUMENT_SCAN_CHAR_LIMIT: usize = 1_000_000;

/// The fold a Markdown heading row offers: the heading line stays visible and
/// its body — nested headings included — is hidden.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownHeadingFold {
    /// The `#` count, 1 to 6.
    pub level: u32,
    pub range: Range<MultiBufferPoint>,
}

/// One walk of a Markdown document, kept on the [`DisplayMap`] and shared with
/// every [`DisplaySnapshot`] taken from it.
///
/// The gutter asks for a crease once per visible row and `editor::FoldAll` once
/// per row of the buffer, so neither pairing `<details>` tags nor working out
/// where a setext heading's body ends can afford a walk per row. Walking once
/// per version of the buffer and answering every row from the result is what
/// lets the `<details>` fold drop [`MARKDOWN_DETAILS_SCAN_LIMIT`] rather than
/// raise it.
pub(crate) type MarkdownScanCache = Arc<Mutex<MarkdownScanCacheState>>;

#[derive(Default)]
pub(crate) struct MarkdownScanCacheState {
    /// What [`Self::scan`] was taken from, kept even when the walk gave up, so
    /// that a document too large to walk is not walked again for every row.
    key: Option<MarkdownScanKey>,
    scan: Option<MarkdownScan>,
    /// How many walks have been run. Only the tests read this, to pin down that
    /// drawing a screenful of gutter costs one walk and not one per row.
    walks: usize,
    /// How many times the rows have been handed out by
    /// [`DisplaySnapshot::markdown_rows_from`]. Only the tests read this, to
    /// pin down that a walk reads the document once — a second reading is what
    /// looking for the end of the frontmatter separately would cost on a
    /// document that opens with a delimiter and never closes it.
    #[cfg(any(test, feature = "test-support"))]
    row_passes: usize,
}

struct MarkdownScan {
    /// Each `<details>` row with the row closing it, sorted. A disclosure that
    /// is never closed is paired with the last row of the buffer, which is how
    /// far it folds.
    details_pairs: Vec<(u32, u32)>,
    /// The fold of each setext heading, in document order, anchored on the
    /// heading's underline row.
    setext_headings: Vec<MarkdownHeadingFold>,
}

/// What a [`MarkdownScan`] was taken from.
///
/// Only a singleton buffer is walked: the rows of a multi-excerpt multibuffer
/// move when excerpts come and go and nothing in the snapshot counts that, so
/// there would be no way to tell a stale walk from a current one. The
/// `<details>` fold falls back to its bounded per-row scan there, and the
/// setext heading fold — which has no bounded form — is not offered at all.
#[derive(PartialEq, Eq)]
struct MarkdownScanKey {
    buffer_id: BufferId,
    version: clock::Global,
    /// Diff hunks expanded into a singleton multibuffer move its rows without
    /// touching the buffer's own version, and this is what counts those.
    edit_count: usize,
}

/// A heading the lexical walk found, whatever its syntax.
struct MarkdownHeadingRow {
    /// The first row of the heading: its `#` line, or a setext heading's text.
    /// The section it opens starts here, so this is one row past the end of the
    /// body of the section before it.
    start_row: u32,
    /// The row the fold hangs off, which stays visible: the same row for an ATX
    /// heading, and the underline for a setext one (ADR §5.10 decision 6).
    anchor_row: u32,
    level: u32,
    is_setext: bool,
}

impl MarkdownScan {
    /// The row the `<details>` opened on `open_row` is closed on, or `None`
    /// when the walk pairs no disclosure with that row.
    fn details_close_row(&self, open_row: u32) -> Option<u32> {
        // A row carrying two opening tags is paired twice, and the fold the row
        // offers is the outer one's — the last of the two, since it closes
        // later.
        let index = self
            .details_pairs
            .partition_point(|(row, _)| *row <= open_row)
            .checked_sub(1)?;
        match self.details_pairs.get(index) {
            Some((row, close_row)) if *row == open_row => Some(*close_row),
            _ => None,
        }
    }
}

/// `Some(level)` when `line` is an ATX heading line, following the CommonMark
/// rule: up to three spaces of indent, one to six `#`, then a space or the end
/// of the line.
fn markdown_atx_heading_level(line: &str) -> Option<u32> {
    let after_indent = line.trim_start_matches(' ');
    if line.len() - after_indent.len() > 3 {
        return None;
    }
    let hashes = after_indent
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = after_indent.get(hashes..)?;
    (rest.is_empty() || rest.starts_with([' ', '\t'])).then_some(hashes as u32)
}

/// `Some((marker, length))` when `line` is a CommonMark code fence delimiter:
/// up to three spaces of indent and then at least three backticks or tildes.
/// Whether it *opens* a block cannot be told from the line alone, which is what
/// [`DisplaySnapshot::markdown_fence_fold`] asks tree-sitter for.
fn markdown_fence_delimiter(line: &str) -> Option<(char, usize)> {
    let after_indent = line.trim_start_matches(' ');
    if line.len() - after_indent.len() > 3 {
        return None;
    }
    let marker = after_indent
        .chars()
        .next()
        .filter(|character| *character == '`' || *character == '~')?;
    let length = after_indent
        .chars()
        .take_while(|character| *character == marker)
        .count();
    if length < 3 {
        return None;
    }
    // A backtick fence cannot carry a backtick after its delimiter, which is
    // what keeps an inline code span from reading as a fence.
    if marker == '`' && after_indent.get(length..)?.contains('`') {
        return None;
    }
    Some((marker, length))
}

/// `Some(level)` when `line` is a setext heading underline: up to three spaces
/// of indent and then nothing but `=` (level 1) or `-` (level 2), trailing
/// whitespace aside.
///
/// A line of dashes is also how a thematic break is written, and CommonMark
/// resolves that in favour of the heading whenever the line above is a
/// paragraph — which is what [`markdown_setext_heading_text`] decides.
fn markdown_setext_underline_level(line: &str) -> Option<u32> {
    let after_indent = line.trim_start_matches(' ');
    if line.len() - after_indent.len() > 3 {
        return None;
    }
    let marker = after_indent.chars().next()?;
    let level = match marker {
        '=' => 1,
        '-' => 2,
        _ => return None,
    };
    let length = after_indent
        .chars()
        .take_while(|character| *character == marker)
        .count();
    after_indent
        .get(length..)
        .is_some_and(|rest| rest.trim().is_empty())
        .then_some(level)
}

/// Whether `line` can be the text of a setext heading, which is what turns the
/// `=` or `-` line under it into an underline rather than a thematic break or a
/// list.
///
/// CommonMark arrives at a paragraph by elimination — it is what is left once
/// every other kind of block has been ruled out — so this rules the same ones
/// out. A line that is itself an underline is rejected because the paragraph
/// above it already claimed it.
fn markdown_setext_heading_text(line: &str) -> bool {
    let after_indent = line.trim_start_matches(' ');
    // Four columns of indent make the line indented code instead, and a leading
    // tab is four columns on its own.
    if line.len() - after_indent.len() > 3 || after_indent.starts_with('\t') {
        return false;
    }
    if after_indent.trim().is_empty() {
        return false;
    }
    if markdown_atx_heading_level(line).is_some()
        || markdown_fence_delimiter(line).is_some()
        || markdown_setext_underline_level(line).is_some()
    {
        return false;
    }
    // A block quote or an HTML block interrupts a paragraph, so a line of
    // dashes under one of them is a thematic break.
    if after_indent.starts_with(['>', '<']) {
        return false;
    }
    !markdown_starts_list_item(after_indent)
}

/// Whether `after_indent` — a line whose leading spaces have already been
/// removed — starts with a bullet or an ordered list marker.
fn markdown_starts_list_item(after_indent: &str) -> bool {
    let mut characters = after_indent.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if matches!(first, '-' | '*' | '+') {
        return matches!(characters.next(), None | Some(' ') | Some('\t'));
    }
    // CommonMark allows at most nine digits in an ordered list marker.
    let digits = after_indent
        .chars()
        .take_while(char::is_ascii_digit)
        .count();
    if !(1..=9).contains(&digits) {
        return false;
    }
    let mut rest = after_indent.get(digits..).unwrap_or_default().chars();
    matches!(rest.next(), Some('.') | Some(')'))
        && matches!(rest.next(), None | Some(' ') | Some('\t'))
}

/// Whether `line` opens an HTML block with a `<details>` tag: up to three spaces
/// of indent and then the tag. CommonMark needs the tag at the start of the line
/// for the disclosure to be a block of its own; a `<details>` in the middle of a
/// paragraph is inline HTML, which no renderer folds.
fn markdown_details_block_open(line: &str) -> bool {
    const OPEN_TAG: &str = "<details";

    let after_indent = line.trim_start_matches(' ');
    if line.len() - after_indent.len() > 3 {
        return false;
    }
    // Compared in place rather than through `to_ascii_lowercase`, because this
    // runs on every row of every Markdown buffer and has nothing to allocate.
    after_indent
        .get(..OPEN_TAG.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(OPEN_TAG))
        && after_indent
            .get(OPEN_TAG.len()..)
            .is_some_and(markdown_tag_name_ends)
}

/// Hands the `<details>` and `</details>` tags of `line` to `visit` in document
/// order, `true` for an opening tag, and stops once `visit` returns `true`.
/// Order matters because one line can close one disclosure and open the next.
fn markdown_details_tags_in(line: &str, mut visit: impl FnMut(bool) -> bool) {
    // Almost every row the scan walks is prose, and this runs on all of them,
    // so the copy below is only made once a tag could possibly be there.
    if !line.contains('<') {
        return;
    }
    // Lowercasing ASCII keeps every byte offset, so the slices below stay on
    // character boundaries.
    let lowered = line.to_ascii_lowercase();
    let mut skip_to = 0;
    for (index, character) in lowered.char_indices() {
        if index < skip_to || character != '<' {
            continue;
        }
        let Some(rest) = lowered.get(index..) else {
            continue;
        };
        if rest.starts_with("<!--") {
            skip_to = match rest.find("-->") {
                Some(end) => index + end + "-->".len(),
                None => lowered.len(),
            };
            continue;
        }
        if let Some(after) = rest.strip_prefix("</details") {
            if markdown_tag_name_ends(after) && visit(false) {
                return;
            }
        } else if let Some(after) = rest.strip_prefix("<details")
            && markdown_tag_name_ends(after)
            && visit(true)
        {
            return;
        }
    }
}

/// Applies the `<details>` and `</details>` tags of `line` to `depth`, and
/// returns `true` once the disclosure the scan started from is closed.
fn markdown_details_tags(line: &str, depth: &mut usize) -> bool {
    let mut closed = false;
    markdown_details_tags_in(line, |opens| {
        if opens {
            *depth += 1;
        } else if *depth > 0 {
            *depth -= 1;
            closed = *depth == 0;
        }
        closed
    });
    closed
}

fn markdown_tag_name_ends(after_name: &str) -> bool {
    match after_name.chars().next() {
        None => true,
        Some(character) => character == '>' || character == '/' || character.is_whitespace(),
    }
}

#[derive(Clone)]
pub struct DisplaySnapshot {
    pub display_map_id: EntityId,
    pub companion_display_snapshot: Option<Arc<DisplaySnapshot>>,
    pub crease_snapshot: CreaseSnapshot,
    block_snapshot: BlockSnapshot,
    text_highlights: TextHighlights,
    inlay_highlights: InlayHighlights,
    semantic_token_highlights: SemanticTokensHighlights,
    clip_at_line_ends: bool,
    masked: bool,
    diagnostics_max_severity: DiagnosticSeverity,
    pub(crate) fold_placeholder: FoldPlaceholder,
    /// When true, LSP folding ranges are used via the crease map and the
    /// indent-based fallback in `crease_for_buffer_row` is skipped.
    pub(crate) use_lsp_folding_ranges: bool,
    /// Shared with the [`DisplayMap`] this snapshot was taken from, so that the
    /// document is walked once per version of the buffer rather than once per
    /// snapshot.
    markdown_scan_cache: MarkdownScanCache,
}

impl DisplaySnapshot {
    pub fn companion_snapshot(&self) -> Option<&DisplaySnapshot> {
        self.companion_display_snapshot.as_deref()
    }

    pub fn wrap_snapshot(&self) -> &WrapSnapshot {
        &self.block_snapshot.wrap_snapshot
    }
    pub fn tab_snapshot(&self) -> &TabSnapshot {
        &self.block_snapshot.wrap_snapshot.tab_snapshot
    }

    /// The column `point` sits at once tabs are expanded, which is where it appears
    /// on screen when the line isn't soft-wrapped. Unlike a display column this is
    /// counted from the start of the buffer row rather than the wrapped segment.
    pub(crate) fn tab_expanded_column(&self, point: Point) -> u32 {
        self.tab_snapshot()
            .point_to_tab_point(point, Bias::Left)
            .0
            .column
    }

    /// Inverse of [`Self::tab_expanded_column`], clamped to the end of the row.
    pub(crate) fn point_for_tab_expanded_column(
        &self,
        buffer_row: MultiBufferRow,
        column: u32,
    ) -> Point {
        let tab_snapshot = self.tab_snapshot();
        let tab_row = tab_snapshot.buffer_row_to_tab_row(buffer_row);
        let column = column.min(tab_snapshot.line_len(tab_row));
        tab_snapshot.tab_point_to_point(TabPoint(Point::new(tab_row, column)), Bias::Left)
    }

    /// The length of `buffer_row` once tabs are expanded.
    pub(crate) fn tab_expanded_line_len(&self, buffer_row: MultiBufferRow) -> u32 {
        let tab_snapshot = self.tab_snapshot();
        tab_snapshot.line_len(tab_snapshot.buffer_row_to_tab_row(buffer_row))
    }

    pub fn fold_snapshot(&self) -> &FoldSnapshot {
        &self.block_snapshot.wrap_snapshot.tab_snapshot.fold_snapshot
    }

    #[inline(always)]
    pub fn has_collapsed_content(&self) -> bool {
        self.fold_snapshot().has_folds() || self.block_snapshot.has_replacement_blocks()
    }

    pub fn inlay_snapshot(&self) -> &InlaySnapshot {
        &self
            .block_snapshot
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
    }

    pub fn buffer_snapshot(&self) -> &MultiBufferSnapshot {
        &self
            .block_snapshot
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer
    }

    #[cfg(test)]
    pub fn fold_count(&self) -> usize {
        self.fold_snapshot().fold_count()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer_snapshot().len() == MultiBufferOffset(0)
    }

    /// Returns whether tree-sitter syntax highlighting should be used.
    /// Returns `false` if any buffer with semantic token highlights has the "full" mode setting,
    /// meaning LSP semantic tokens should replace tree-sitter highlighting.
    pub fn use_tree_sitter_for_syntax(&self, position: DisplayRow, cx: &App) -> bool {
        let position = DisplayPoint::new(position, 0);
        let Some((buffer_snapshot, ..)) = self.point_to_buffer_point(position.to_point(self))
        else {
            return false;
        };
        let settings = LanguageSettings::for_buffer_snapshot(&buffer_snapshot, None, cx);
        settings.semantic_tokens.use_tree_sitter()
    }

    pub fn row_infos(&self, start_row: DisplayRow) -> impl Iterator<Item = RowInfo> + '_ {
        self.block_snapshot.row_infos(BlockRow(start_row.0))
    }

    pub fn widest_line_number(&self) -> u32 {
        self.buffer_snapshot().widest_line_number()
    }

    #[instrument(skip_all)]
    pub fn prev_line_boundary(&self, mut point: MultiBufferPoint) -> (Point, DisplayPoint) {
        loop {
            let mut inlay_point = self.inlay_snapshot().to_inlay_point(point);
            let mut fold_point = self.fold_snapshot().to_fold_point(inlay_point, Bias::Left);
            fold_point.0.column = 0;
            inlay_point = fold_point.to_inlay_point(self.fold_snapshot());
            point = self.inlay_snapshot().to_buffer_point(inlay_point);

            let mut display_point = self.point_to_display_point(point, Bias::Left);
            *display_point.column_mut() = 0;
            let next_point = self.display_point_to_point(display_point, Bias::Left);
            if next_point == point {
                return (point, display_point);
            }
            point = next_point;
        }
    }

    #[instrument(skip_all)]
    pub fn next_line_boundary(
        &self,
        mut point: MultiBufferPoint,
    ) -> (MultiBufferPoint, DisplayPoint) {
        let original_point = point;
        loop {
            let mut inlay_point = self.inlay_snapshot().to_inlay_point(point);
            let mut fold_point = self.fold_snapshot().to_fold_point(inlay_point, Bias::Right);
            fold_point.0.column = self.fold_snapshot().line_len(fold_point.row());
            inlay_point = fold_point.to_inlay_point(self.fold_snapshot());
            point = self.inlay_snapshot().to_buffer_point(inlay_point);

            let mut display_point = self.point_to_display_point(point, Bias::Right);
            *display_point.column_mut() = self.line_len(display_point.row());
            let next_point = self.display_point_to_point(display_point, Bias::Right);
            if next_point == point || original_point == point || original_point == next_point {
                return (point, display_point);
            }
            point = next_point;
        }
    }

    // used by line_mode selections and tries to match vim behavior
    pub fn expand_to_line(&self, range: Range<Point>) -> Range<Point> {
        let new_start = MultiBufferPoint::new(range.start.row, 0);
        let new_end = if range.end.column > 0 {
            MultiBufferPoint::new(
                range.end.row,
                self.buffer_snapshot()
                    .line_len(MultiBufferRow(range.end.row)),
            )
        } else {
            range.end
        };

        new_start..new_end
    }

    #[instrument(skip_all)]
    pub fn point_to_display_point(&self, point: MultiBufferPoint, bias: Bias) -> DisplayPoint {
        let inlay_point = self.inlay_snapshot().to_inlay_point(point);
        let fold_point = self.fold_snapshot().to_fold_point(inlay_point, bias);
        let tab_point = self.tab_snapshot().fold_point_to_tab_point(fold_point);
        let wrap_point = self.wrap_snapshot().tab_point_to_wrap_point(tab_point);
        let block_point = self.block_snapshot.to_block_point(wrap_point);
        DisplayPoint(block_point)
    }

    /// Converts a buffer offset range into one or more `DisplayPoint` ranges
    /// that cover only actual buffer text, excluding any inlay hint text that
    /// falls within the range.
    pub fn isomorphic_display_point_ranges_for_buffer_range(
        &self,
        range: Range<MultiBufferOffset>,
    ) -> SmallVec<[Range<DisplayPoint>; 1]> {
        self.display_point_converter().map(range)
    }

    /// Converts a non-empty buffer range into one contiguous display range.
    /// Inlays at either boundary are excluded, while inlays between selected
    /// buffer characters are included.
    pub fn contiguous_display_point_range_for_buffer_range(
        &self,
        range: Range<MultiBufferOffset>,
    ) -> Option<Range<DisplayPoint>> {
        if range.is_empty() {
            return None;
        }

        let buffer = self.buffer_snapshot();
        let first_character_end =
            buffer.clip_offset((range.start + 1usize).min(range.end), Bias::Right);
        let last_character_start = buffer.clip_offset(
            range.end.saturating_sub_usize(1).max(range.start),
            Bias::Left,
        );

        let mut converter = self.display_point_converter();
        let first_ranges = converter.map(range.start..first_character_end);
        let start = first_ranges.first()?.start;
        if first_character_end == range.end {
            return Some(start..first_ranges.last()?.end);
        }

        let last_ranges = converter.map(last_character_start..range.end);
        Some(start..last_ranges.last()?.end)
    }

    /// Returns a converter that maps buffer offset ranges to `DisplayPoint`
    /// ranges (as in [`Self::isomorphic_display_point_ranges_for_buffer_range`])
    /// while reusing cursor state across calls. Use this when converting many
    /// ranges in a single pass; the inputs must be supplied with non-decreasing
    /// offsets so the underlying cursors only advance forward.
    pub fn display_point_converter(&self) -> DisplayPointConverter<'_> {
        DisplayPointConverter {
            inlay_cursor: self.inlay_snapshot().buffer_offset_to_inlay_point_cursor(),
            fold_point_cursor: self.fold_snapshot().fold_point_cursor(),
            tab_point_cursor: self.tab_snapshot().tab_point_cursor(),
            wrap_point_cursor: self.wrap_snapshot().wrap_point_cursor(),
            block_point_cursor: self.block_snapshot.block_point_cursor(),
            prev_end: None,
        }
    }

    pub fn display_point_to_point(&self, point: DisplayPoint, bias: Bias) -> Point {
        self.inlay_snapshot()
            .to_buffer_point(self.display_point_to_inlay_point(point, bias))
    }

    pub fn display_point_to_inlay_offset(&self, point: DisplayPoint, bias: Bias) -> InlayOffset {
        self.inlay_snapshot()
            .to_offset(self.display_point_to_inlay_point(point, bias))
    }

    pub fn anchor_to_inlay_offset(&self, anchor: Anchor) -> InlayOffset {
        self.inlay_snapshot()
            .to_inlay_offset(anchor.to_offset(self.buffer_snapshot()))
    }

    pub fn display_point_to_anchor(&self, point: DisplayPoint, bias: Bias) -> Anchor {
        self.buffer_snapshot()
            .anchor_at(point.to_offset(self, bias), bias)
    }

    #[instrument(skip_all)]
    fn display_point_to_inlay_point(&self, point: DisplayPoint, bias: Bias) -> InlayPoint {
        let block_point = point.0;
        let wrap_point = self.block_snapshot.to_wrap_point(block_point, bias);
        let tab_point = self.wrap_snapshot().to_tab_point(wrap_point);
        let fold_point = self
            .tab_snapshot()
            .tab_point_to_fold_point(tab_point, bias)
            .0;
        fold_point.to_inlay_point(self.fold_snapshot())
    }

    #[instrument(skip_all)]
    pub fn display_point_to_fold_point(&self, point: DisplayPoint, bias: Bias) -> FoldPoint {
        let block_point = point.0;
        let wrap_point = self.block_snapshot.to_wrap_point(block_point, bias);
        let tab_point = self.wrap_snapshot().to_tab_point(wrap_point);
        self.tab_snapshot()
            .tab_point_to_fold_point(tab_point, bias)
            .0
    }

    #[instrument(skip_all)]
    pub fn fold_point_to_display_point(&self, fold_point: FoldPoint) -> DisplayPoint {
        let tab_point = self.tab_snapshot().fold_point_to_tab_point(fold_point);
        let wrap_point = self.wrap_snapshot().tab_point_to_wrap_point(tab_point);
        let block_point = self.block_snapshot.to_block_point(wrap_point);
        DisplayPoint(block_point)
    }

    pub fn max_point(&self) -> DisplayPoint {
        DisplayPoint(self.block_snapshot.max_point())
    }

    /// Returns text chunks starting at the given display row until the end of the file
    #[instrument(skip_all)]
    pub fn text_chunks(&self, display_row: DisplayRow) -> impl Iterator<Item = &str> {
        self.block_snapshot
            .chunks(
                BlockRow(display_row.0)..BlockRow(self.max_point().row().next_row().0),
                LanguageAwareStyling {
                    tree_sitter: false,
                    diagnostics: false,
                },
                self.masked,
                Highlights::default(),
            )
            .map(|h| h.text)
    }

    /// Returns text chunks starting at the end of the given display row in reverse until the start of the file
    #[instrument(skip_all)]
    pub fn reverse_text_chunks(&self, display_row: DisplayRow) -> impl Iterator<Item = &str> {
        (0..=display_row.0).rev().flat_map(move |row| {
            self.block_snapshot
                .chunks(
                    BlockRow(row)..BlockRow(row + 1),
                    LanguageAwareStyling {
                        tree_sitter: false,
                        diagnostics: false,
                    },
                    self.masked,
                    Highlights::default(),
                )
                .map(|h| h.text)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
        })
    }

    #[instrument(skip_all)]
    pub fn chunks(
        &self,
        display_rows: Range<DisplayRow>,
        language_aware: LanguageAwareStyling,
        highlight_styles: HighlightStyles,
    ) -> DisplayChunks<'_> {
        self.block_snapshot.chunks(
            BlockRow(display_rows.start.0)..BlockRow(display_rows.end.0),
            language_aware,
            self.masked,
            Highlights {
                text_highlights: Some(&self.text_highlights),
                inlay_highlights: Some(&self.inlay_highlights),
                semantic_token_highlights: Some(&self.semantic_token_highlights),
                styles: highlight_styles,
            },
        )
    }

    #[instrument(skip_all)]
    pub fn highlighted_chunks<'a>(
        &'a self,
        display_rows: Range<DisplayRow>,
        language_aware: LanguageAwareStyling,
        editor_style: &'a EditorStyle,
    ) -> impl Iterator<Item = HighlightedChunk<'a>> {
        self.chunks(
            display_rows,
            language_aware,
            HighlightStyles {
                inlay_hint: Some(editor_style.inlay_hints_style),
                edit_prediction: Some(editor_style.edit_prediction_styles),
            },
        )
        .flat_map({
            // track the current underline style so that we can apply it to
            // inlay hints within the diagnostic's span
            let mut current_diagnostic_underline: Option<UnderlineStyle> = None;

            move |chunk| {
                let syntax_highlight_style = chunk
                    .syntax_highlight_id
                    .and_then(|id| editor_style.syntax.get(id).cloned());

                let chunk_highlight = chunk.highlight_style.map(|chunk_highlight| {
                    HighlightStyle {
                        // For color inlays, blend the color with the editor background
                        // if the color has transparency (alpha < 1.0)
                        color: chunk_highlight.color.map(|color| {
                            if chunk.is_inlay && !color.is_opaque() {
                                editor_style.background.blend(color)
                            } else {
                                color
                            }
                        }),
                        underline: chunk_highlight
                            .underline
                            .filter(|_| editor_style.show_underlines),
                        ..chunk_highlight
                    }
                });

                let diagnostic_highlight = if chunk.is_inlay {
                    current_diagnostic_underline.map(|underline| HighlightStyle {
                        underline: Some(underline),
                        ..Default::default()
                    })
                } else {
                    let highlight = chunk
                        .diagnostic_severity
                        .filter(|severity| {
                            self.diagnostics_max_severity
                                .into_lsp()
                                .is_some_and(|max_severity| severity <= &max_severity)
                        })
                        .map(|severity| HighlightStyle {
                            fade_out: chunk
                                .is_unnecessary
                                .then_some(editor_style.unnecessary_code_fade),
                            underline: (chunk.underline
                                && editor_style.show_underlines
                                && !(chunk.is_unnecessary
                                    && severity > lsp::DiagnosticSeverity::WARNING))
                                .then(|| {
                                    let diagnostic_color =
                                        diagnostic_style(severity, &editor_style.status);
                                    UnderlineStyle {
                                        color: Some(diagnostic_color),
                                        thickness: 1.0.into(),
                                        wavy: true,
                                    }
                                }),
                            ..Default::default()
                        });

                    current_diagnostic_underline = highlight.as_ref().and_then(|h| h.underline);
                    highlight
                };

                let style = [
                    syntax_highlight_style,
                    chunk_highlight,
                    diagnostic_highlight,
                ]
                .into_iter()
                .flatten()
                .reduce(|acc, highlight| acc.highlight(highlight));

                HighlightedChunk {
                    text: chunk.text,
                    style,
                    is_tab: chunk.is_tab,
                    is_inlay: chunk.is_inlay,
                    replacement: chunk.renderer.map(ChunkReplacement::Renderer),
                }
                .highlight_invisibles(editor_style)
            }
        })
    }

    /// Returns combined highlight styles (tree-sitter syntax + semantic tokens)
    /// for a byte range within the specified buffer.
    /// Returned ranges are 0-based relative to `buffer_range.start`.
    pub(super) fn combined_highlights(
        &self,
        multibuffer_range: Range<MultiBufferOffset>,
        syntax_theme: &theme::SyntaxTheme,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let multibuffer = self.buffer_snapshot();

        let chunks = custom_highlights::CustomHighlightsChunks::new(
            multibuffer_range,
            LanguageAwareStyling {
                tree_sitter: true,
                diagnostics: true,
            },
            None,
            Some(&self.semantic_token_highlights),
            multibuffer,
        );

        let mut highlights = Vec::new();
        let mut offset = 0usize;
        for chunk in chunks {
            let chunk_len = chunk.text.len();
            if chunk_len == 0 {
                continue;
            }

            let syntax_style = chunk
                .syntax_highlight_id
                .and_then(|id| syntax_theme.get(id).cloned());

            let overlay_style = chunk.highlight_style;

            let combined = match (syntax_style, overlay_style) {
                (Some(syntax), Some(overlay)) => Some(syntax.highlight(overlay)),
                (some @ Some(_), None) | (None, some @ Some(_)) => some,
                (None, None) => None,
            };

            if let Some(style) = combined {
                highlights.push((offset..offset + chunk_len, style));
            }
            offset += chunk_len;
        }
        highlights
    }

    #[instrument(skip_all)]
    pub fn layout_row(
        &self,
        display_row: DisplayRow,
        TextLayoutDetails {
            text_system,
            editor_style,
            rem_size,
            scroll_anchor: _,
            visible_rows: _,
            vertical_scroll_margin: _,
        }: &TextLayoutDetails,
    ) -> Arc<LineLayout> {
        let mut runs = Vec::new();
        let mut line = String::new();

        let range = display_row..display_row.next_row();
        for chunk in self.highlighted_chunks(
            range,
            LanguageAwareStyling {
                tree_sitter: false,
                diagnostics: false,
            },
            editor_style,
        ) {
            line.push_str(chunk.text);

            let text_style = if let Some(style) = chunk.style {
                Cow::Owned(editor_style.text.clone().highlight(style))
            } else {
                Cow::Borrowed(&editor_style.text)
            };

            runs.push(text_style.to_run(chunk.text.len()))
        }

        if line.ends_with('\n') {
            line.pop();
            if let Some(last_run) = runs.last_mut() {
                last_run.len -= 1;
                if last_run.len == 0 {
                    runs.pop();
                }
            }
        }

        let font_size = editor_style.text.font_size.to_pixels(*rem_size);
        text_system.layout_line(&line, font_size, &runs, None)
    }

    pub fn x_for_display_point(
        &self,
        display_point: DisplayPoint,
        text_layout_details: &TextLayoutDetails,
    ) -> Pixels {
        let line = self.layout_row(display_point.row(), text_layout_details);
        line.x_for_index(display_point.column() as usize)
    }

    pub fn display_column_for_x(
        &self,
        display_row: DisplayRow,
        x: Pixels,
        details: &TextLayoutDetails,
    ) -> u32 {
        let layout_line = self.layout_row(display_row, details);
        layout_line.closest_index_for_x(x) as u32
    }

    #[instrument(skip_all)]
    pub fn grapheme_at(&self, mut point: DisplayPoint) -> Option<SharedString> {
        point = DisplayPoint(self.block_snapshot.clip_point(point.0, Bias::Left));
        let chars = self
            .text_chunks(point.row())
            .flat_map(str::chars)
            .skip_while({
                let mut column = 0;
                move |char| {
                    let at_point = column >= point.column();
                    column += char.len_utf8() as u32;
                    !at_point
                }
            })
            .take_while({
                let mut prev = false;
                move |char| {
                    let now = char.is_ascii();
                    let end = char.is_ascii() && (char.is_ascii_whitespace() || prev);
                    prev = now;
                    !end
                }
            });
        chars.collect::<String>().graphemes(true).next().map(|s| {
            if let Some(invisible) = s.chars().next().filter(|&c| is_invisible(c)) {
                replacement(invisible).map_or_else(|| s.to_owned().into(), SharedString::from)
            } else if s == "\n" {
                " ".into()
            } else {
                s.to_owned().into()
            }
        })
    }

    pub fn buffer_chars_at(
        &self,
        mut offset: MultiBufferOffset,
    ) -> impl Iterator<Item = (char, MultiBufferOffset)> + '_ {
        self.buffer_snapshot().chars_at(offset).map(move |ch| {
            let ret = (ch, offset);
            offset += ch.len_utf8();
            ret
        })
    }

    pub fn reverse_buffer_chars_at(
        &self,
        mut offset: MultiBufferOffset,
    ) -> impl Iterator<Item = (char, MultiBufferOffset)> + '_ {
        self.buffer_snapshot()
            .reversed_chars_at(offset)
            .map(move |ch| {
                offset -= ch.len_utf8();
                (ch, offset)
            })
    }

    pub fn clip_point(&self, point: DisplayPoint, bias: Bias) -> DisplayPoint {
        let mut clipped = self.block_snapshot.clip_point(point.0, bias);
        if self.clip_at_line_ends {
            clipped = self.clip_at_line_end(DisplayPoint(clipped)).0
        }
        DisplayPoint(clipped)
    }

    pub fn clip_ignoring_line_ends(&self, point: DisplayPoint, bias: Bias) -> DisplayPoint {
        DisplayPoint(self.block_snapshot.clip_point(point.0, bias))
    }

    pub fn inlay_bias_at(&self, point: DisplayPoint) -> Option<Bias> {
        let wrap_point = self.block_snapshot.to_wrap_point(point.0, Bias::Left);
        let tab_point = self.block_snapshot.to_tab_point(wrap_point);
        let (fold_point, _, _) = self
            .block_snapshot
            .tab_snapshot
            .tab_point_to_fold_point(tab_point, Bias::Left);
        let inlay_point =
            fold_point.to_inlay_point(&self.block_snapshot.tab_snapshot.fold_snapshot);
        self.block_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_bias_at_point(inlay_point)
    }

    pub fn clip_at_line_end(&self, display_point: DisplayPoint) -> DisplayPoint {
        let mut point = self.display_point_to_point(display_point, Bias::Left);

        if point.column != self.buffer_snapshot().line_len(MultiBufferRow(point.row)) {
            return display_point;
        }
        point.column = point.column.saturating_sub(1);
        point = self.buffer_snapshot().clip_point(point, Bias::Left);
        self.point_to_display_point(point, Bias::Left)
    }

    pub fn folds_in_range<T>(&self, range: Range<T>) -> impl Iterator<Item = &Fold>
    where
        T: ToOffset,
    {
        self.fold_snapshot().folds_in_range(range)
    }

    pub fn blocks_in_range(
        &self,
        rows: Range<DisplayRow>,
    ) -> impl Iterator<Item = (DisplayRow, &Block)> {
        self.block_snapshot
            .blocks_in_range(BlockRow(rows.start.0)..BlockRow(rows.end.0))
            .map(|(row, block)| (DisplayRow(row.0), block))
    }

    pub fn sticky_header_excerpt(&self, row: f64) -> Option<StickyHeaderExcerpt<'_>> {
        self.block_snapshot.sticky_header_excerpt(row)
    }

    pub fn block_for_id(&self, id: BlockId) -> Option<Block> {
        self.block_snapshot.block_for_id(id)
    }

    pub fn intersects_fold<T: ToOffset>(&self, offset: T) -> bool {
        self.fold_snapshot().intersects_fold(offset)
    }

    pub fn is_line_folded(&self, buffer_row: MultiBufferRow) -> bool {
        self.block_snapshot.is_line_replaced(buffer_row)
            || self.fold_snapshot().is_line_folded(buffer_row)
    }

    pub fn is_block_line(&self, display_row: DisplayRow) -> bool {
        self.block_snapshot.is_block_line(BlockRow(display_row.0))
    }

    pub fn is_folded_buffer_header(&self, display_row: DisplayRow) -> bool {
        self.block_snapshot
            .is_folded_buffer_header(BlockRow(display_row.0))
    }

    pub fn soft_wrap_indent(&self, display_row: DisplayRow) -> Option<u32> {
        let wrap_row = self
            .block_snapshot
            .to_wrap_point(BlockPoint::new(BlockRow(display_row.0), 0), Bias::Left)
            .row();
        self.wrap_snapshot().soft_wrap_indent(wrap_row)
    }

    pub fn text(&self) -> String {
        self.text_chunks(DisplayRow(0)).collect()
    }

    pub fn line(&self, display_row: DisplayRow) -> String {
        let mut result = String::new();
        for chunk in self.text_chunks(display_row) {
            if let Some(ix) = chunk.find('\n') {
                result.push_str(&chunk[0..ix]);
                break;
            } else {
                result.push_str(chunk);
            }
        }
        result
    }

    pub fn line_indent_for_buffer_row(&self, buffer_row: MultiBufferRow) -> LineIndent {
        self.buffer_snapshot().line_indent_for_row(buffer_row)
    }

    pub fn line_len(&self, row: DisplayRow) -> u32 {
        self.block_snapshot.line_len(BlockRow(row.0))
    }

    pub fn longest_row(&self) -> DisplayRow {
        DisplayRow(self.block_snapshot.longest_row().0)
    }

    pub fn longest_row_in_range(&self, range: Range<DisplayRow>) -> DisplayRow {
        let block_range = BlockRow(range.start.0)..BlockRow(range.end.0);
        let longest_row = self.block_snapshot.longest_row_in_range(block_range);
        DisplayRow(longest_row.0)
    }

    pub fn starts_indent(&self, buffer_row: MultiBufferRow) -> bool {
        let max_row = self.buffer_snapshot().max_row();
        if buffer_row >= max_row {
            return false;
        }

        let line_indent = self.line_indent_for_buffer_row(buffer_row);
        if line_indent.is_line_blank() {
            return false;
        }

        (buffer_row.0 + 1..=max_row.0)
            .find_map(|next_row| {
                let next_line_indent = self.line_indent_for_buffer_row(MultiBufferRow(next_row));
                if next_line_indent.raw_len() > line_indent.raw_len() {
                    Some(true)
                } else if !next_line_indent.is_line_blank() {
                    Some(false)
                } else {
                    None
                }
            })
            .unwrap_or(false)
    }

    fn markdown_line(&self, row: u32) -> String {
        self.buffer_snapshot()
            .chars_at(Point::new(row, 0))
            .take_while(|character| *character != '\n')
            .take(MARKDOWN_LINE_LIMIT)
            .collect()
    }

    /// Hands rows `start_row..=end_row` to `visit` in one walk of the buffer,
    /// each truncated to [`MARKDOWN_LINE_LIMIT`] characters. `visit` returning
    /// `true` stops the walk.
    ///
    /// Seeking the rope once rather than once per row is what keeps the
    /// `<details>` scan off the gutter's critical path: asking
    /// [`Self::markdown_line`] for two thousand rows in a row costs two
    /// thousand seeks and as many allocations, which is enough to be seen in a
    /// frame.
    ///
    /// Returns whether every row through `end_row` was reached. A walk stopped
    /// by `char_budget` has seen no further than it got, so its caller cannot
    /// conclude anything about the rows beyond.
    fn markdown_rows_from(
        &self,
        start_row: u32,
        end_row: u32,
        char_budget: usize,
        mut visit: impl FnMut(u32, &str) -> bool,
    ) -> bool {
        if start_row > end_row {
            return true;
        }
        #[cfg(any(test, feature = "test-support"))]
        {
            self.markdown_scan_cache.lock().row_passes += 1;
        }

        let mut row = start_row;
        let mut line = String::new();
        let mut line_length = 0;
        let mut budget = char_budget;
        for character in self.buffer_snapshot().chars_at(Point::new(start_row, 0)) {
            if budget == 0 {
                return false;
            }
            budget -= 1;
            if character == '\n' {
                if visit(row, &line) || row == end_row {
                    return true;
                }
                row += 1;
                line.clear();
                line_length = 0;
            } else if line_length < MARKDOWN_LINE_LIMIT {
                line.push(character);
                line_length += 1;
            }
        }
        // The last row of a buffer that does not end in a newline never reaches
        // the branch above.
        visit(row, &line);
        true
    }

    /// The key a [`MarkdownScan`] of this snapshot would be filed under, or
    /// `None` when the buffer is one that must not be walked at all — see
    /// [`MarkdownScanKey`].
    fn markdown_scan_key(&self) -> Option<MarkdownScanKey> {
        let snapshot = self.buffer_snapshot();
        let buffer = snapshot.as_singleton()?;
        if buffer
            .language()
            .is_none_or(|language| language.name() != MARKDOWN_LANGUAGE_NAME)
        {
            return None;
        }
        Some(MarkdownScanKey {
            buffer_id: buffer.remote_id(),
            version: buffer.version().clone(),
            edit_count: snapshot.edit_count(),
        })
    }

    /// Hands the walk of this Markdown document to `read`, running it first when
    /// the one in hand was taken from another version of the buffer. `None` when
    /// there is no walk to be had: another language, a multi-excerpt
    /// multibuffer, or a document past
    /// [`MARKDOWN_DOCUMENT_SCAN_CHAR_LIMIT`].
    ///
    /// `read` takes what it needs out of the walk rather than the walk being
    /// handed back, because the cache is shared and is held locked while it is
    /// read.
    fn with_markdown_scan<T>(&self, read: impl FnOnce(&MarkdownScan) -> T) -> Option<T> {
        let key = self.markdown_scan_key()?;
        {
            let cache = self.markdown_scan_cache.lock();
            if cache.key.as_ref() == Some(&key) {
                return cache.scan.as_ref().map(read);
            }
        }
        // The walk runs with the cache unlocked. `parking_lot::Mutex` is not
        // reentrant, so anything the walk came to ask a fold for would hang
        // rather than misbehave, and walking under the lock is what would make
        // that a hang. The cost is that two snapshots of the same version that
        // miss at the same time both walk and store the same answer; the folds
        // are asked for from the foreground thread a row at a time, so the
        // second walk is a possibility rather than something that happens.
        let scan = self.markdown_document_scan();
        let mut cache = self.markdown_scan_cache.lock();
        cache.walks += 1;
        cache.scan = scan;
        cache.key = Some(key);
        cache.scan.as_ref().map(read)
    }

    /// How many times this snapshot's document has been walked, which is what
    /// the tests weigh a screenful of gutter against.
    #[cfg(any(test, feature = "test-support"))]
    pub fn markdown_document_walks(&self) -> usize {
        self.markdown_scan_cache.lock().walks
    }

    /// How many times the rows of this snapshot's document have been handed out
    /// by [`Self::markdown_rows_from`], which is what the tests weigh a walk
    /// against: one walk is meant to be one reading of the document.
    #[cfg(any(test, feature = "test-support"))]
    pub fn markdown_row_passes(&self) -> usize {
        self.markdown_scan_cache.lock().row_passes
    }

    /// Walks the document once and works out everything the Markdown folds need
    /// to answer row by row: where each `<details>` is closed, and what each
    /// setext heading's underline folds away.
    ///
    /// `None` when the walk ran past [`MARKDOWN_DOCUMENT_SCAN_CHAR_LIMIT`]: it
    /// has seen nothing of the rows beyond, and a half-walked document would
    /// pair a disclosure with the wrong row rather than with none.
    fn markdown_document_scan(&self) -> Option<MarkdownScan> {
        let snapshot = self.buffer_snapshot();
        let max_row = snapshot.max_row().0;
        // `tree-sitter-md` reads a metadata block only when the very first line
        // is the delimiter and a *later* line closes it — an unclosed `---`
        // parses as a thematic break — and the walk applies the same rule.
        // Without it the closing `---` reads as the underline of a setext
        // heading whose text is the last metadata line, and offers to fold the
        // whole document away.
        //
        // Whether the block is closed is only known once the close turns up, so
        // the walk takes the rows that may be metadata as ordinary ones and
        // throws away what it made of them if it finds the close. Looking for
        // the close first would read every row of a document that opens with a
        // delimiter and never closes it a second time.
        let mut frontmatter_delimiter = match self.markdown_line(0).trim_end() {
            "---" => Some("---"),
            "+++" => Some("+++"),
            _ => None,
        };

        let mut open_fence: Option<(char, usize)> = None;
        let mut open_details_rows: Vec<u32> = Vec::new();
        let mut details_pairs: Vec<(u32, u32)> = Vec::new();
        let mut headings: Vec<MarkdownHeadingRow> = Vec::new();
        // Where the run of paragraph rows ending on the row before the one
        // being visited began. CommonMark makes the whole paragraph the text of
        // the setext heading its underline closes, so that is where the heading
        // starts; taking the row above the underline instead would leave the
        // section above folding all but the last line of the heading away.
        let mut heading_text_start_row: Option<u32> = None;

        let complete = self.markdown_rows_from(
            0,
            max_row,
            MARKDOWN_DOCUMENT_SCAN_CHAR_LIMIT,
            |row, line| {
                // The opening delimiter is on row zero, so only a row below it
                // can be the close that turns those rows into metadata.
                if let Some(delimiter) = frontmatter_delimiter
                    && row > 0
                    && line.trim_end() == delimiter
                {
                    frontmatter_delimiter = None;
                    open_fence = None;
                    open_details_rows.clear();
                    details_pairs.clear();
                    headings.clear();
                    heading_text_start_row = None;
                    return false;
                }

                // Inside a fenced code block every line is an example: a `#` is
                // not a heading, a row of dashes is not an underline, and a
                // `</details>` closes nothing.
                if let Some((marker, length)) = open_fence {
                    if is_closing_fence(line, marker, length) {
                        open_fence = None;
                    }
                    heading_text_start_row = None;
                    return false;
                }
                if let Some(fence) = markdown_fence_delimiter(line) {
                    open_fence = Some(fence);
                    heading_text_start_row = None;
                    return false;
                }

                if let Some(level) = markdown_atx_heading_level(line) {
                    headings.push(MarkdownHeadingRow {
                        start_row: row,
                        anchor_row: row,
                        level,
                        is_setext: false,
                    });
                } else if let Some(text_start_row) = heading_text_start_row
                    && let Some(level) = markdown_setext_underline_level(line)
                {
                    headings.push(MarkdownHeadingRow {
                        start_row: text_start_row,
                        anchor_row: row,
                        level,
                        is_setext: true,
                    });
                }

                markdown_details_tags_in(line, |opens| {
                    if opens {
                        open_details_rows.push(row);
                    } else if let Some(open_row) = open_details_rows.pop() {
                        details_pairs.push((open_row, row));
                    }
                    false
                });

                heading_text_start_row = if markdown_setext_heading_text(line) {
                    Some(heading_text_start_row.unwrap_or(row))
                } else {
                    None
                };
                false
            },
        );
        if !complete {
            return None;
        }

        // A disclosure that is never closed still has a body to hide, and the
        // extent it hides runs to the end of the buffer.
        details_pairs.extend(
            open_details_rows
                .into_iter()
                .map(|open_row| (open_row, max_row)),
        );
        details_pairs.sort_unstable();

        let setext_headings = headings
            .iter()
            .enumerate()
            .filter(|(_, heading)| heading.is_setext)
            .filter_map(|(index, heading)| {
                // A section runs until a heading of its own level or shallower,
                // so a deeper one on the way is part of the body and folds with
                // it.
                let last_body_row = headings
                    .get(index + 1..)
                    .unwrap_or_default()
                    .iter()
                    .find(|next| next.level <= heading.level)
                    .map_or(max_row, |next| next.start_row.saturating_sub(1));
                self.markdown_heading_fold_through(heading.anchor_row, heading.level, last_body_row)
            })
            .collect();

        Some(MarkdownScan {
            details_pairs,
            setext_headings,
        })
    }

    /// Whether the syntax layer at the start of `row` is the Markdown layer
    /// itself rather than a language injected into it.
    ///
    /// The layer is checked per row rather than once per buffer because a fenced
    /// code block is an injected layer of its own language: a comment line
    /// inside one can read as a heading, and the text objects of one turn up
    /// alongside Markdown's own.
    fn is_markdown_layer_row(&self, row: u32) -> bool {
        self.buffer_snapshot()
            .language_at(MultiBufferPoint::new(row, 0))
            .is_some_and(|language| language.name() == MARKDOWN_LANGUAGE_NAME)
    }

    /// Whether the buffer `row` belongs to is a Markdown buffer.
    ///
    /// [`Self::is_markdown_layer_row`] is the wrong test for a `<details>` row:
    /// Markdown injects every HTML block into the HTML grammar, so such a row
    /// reports HTML rather than Markdown wherever an HTML grammar is installed.
    fn is_markdown_buffer_row(&self, row: u32) -> bool {
        self.buffer_snapshot()
            .point_to_buffer_offset(MultiBufferPoint::new(row, 0))
            .and_then(|(buffer, _)| buffer.language())
            .is_some_and(|language| language.name() == MARKDOWN_LANGUAGE_NAME)
    }

    /// `Some(level)` when `row` of a Markdown buffer is an ATX heading row.
    fn markdown_heading_level_at(&self, row: u32) -> Option<u32> {
        if !self.is_markdown_layer_row(row) {
            return None;
        }
        markdown_atx_heading_level(&self.markdown_line(row))
    }

    /// Turns the extent of a `section` text object into the fold its heading
    /// row offers.
    fn markdown_heading_fold_extent(
        &self,
        heading_row: u32,
        level: u32,
        section_end: MultiBufferPoint,
    ) -> Option<MarkdownHeadingFold> {
        // A section ends where the next one begins, so its end lands at the
        // start of a row that is no longer part of it.
        let last_body_row = if section_end.column == 0 {
            section_end.row.saturating_sub(1)
        } else {
            section_end.row
        };
        self.markdown_heading_fold_through(heading_row, level, last_body_row)
    }

    /// The fold a heading offers once the last row of its body is known.
    ///
    /// `heading_row` is the row that stays visible — the `#` line of an ATX
    /// heading, the underline of a setext one — so the fold starts at its end.
    /// The blank lines before the next heading read as separating the two
    /// rather than as the tail of the body, so none of them is folded away.
    fn markdown_heading_fold_through(
        &self,
        heading_row: u32,
        level: u32,
        last_body_row: u32,
    ) -> Option<MarkdownHeadingFold> {
        let snapshot = self.buffer_snapshot();

        let mut end_row = last_body_row.min(snapshot.max_row().0);
        while end_row > heading_row && snapshot.is_line_blank(MultiBufferRow(end_row)) {
            end_row -= 1;
        }
        if end_row <= heading_row {
            return None;
        }

        Some(MarkdownHeadingFold {
            level,
            range: MultiBufferPoint::new(
                heading_row,
                snapshot.line_len(MultiBufferRow(heading_row)),
            )
                ..MultiBufferPoint::new(end_row, snapshot.line_len(MultiBufferRow(end_row))),
        })
    }

    /// The fold an ATX Markdown heading row offers, if any.
    ///
    /// Markdown has no fold query in this tree, and `starts_indent` never
    /// matches a heading whose body is not indented (which is every ordinary
    /// Markdown document), so the extent is taken from the `section` text
    /// object instead. The lexical test comes first only to keep the
    /// tree-sitter query off every non-heading row: the query is what decides,
    /// which is why a `#` line inside a fenced code block is rejected.
    ///
    /// Setext headings (`===` / `---` underlines) are left out: `tree-sitter-md`
    /// does not open a `section` for them, so there is no extent to fold here.
    /// [`Self::markdown_setext_heading_fold`] answers for those.
    pub fn markdown_heading_fold(&self, buffer_row: MultiBufferRow) -> Option<MarkdownHeadingFold> {
        let level = self.markdown_heading_level_at(buffer_row.0)?;

        let snapshot = self.buffer_snapshot();
        let row_start_offset = MultiBufferPoint::new(buffer_row.0, 0).to_offset(snapshot);
        let row_end_offset =
            MultiBufferPoint::new(buffer_row.0, snapshot.line_len(buffer_row)).to_offset(snapshot);
        let section_end = snapshot
            .text_object_ranges(
                row_start_offset..row_end_offset,
                TreeSitterOptions::default(),
            )
            .find_map(|(range, text_object)| {
                (text_object == TextObject::AroundClass && range.start == row_start_offset)
                    .then_some(range.end)
            })?
            .to_point(snapshot);

        self.markdown_heading_fold_extent(buffer_row.0, level, section_end)
    }

    /// The fold a setext Markdown heading offers, asked on its underline row.
    ///
    /// `tree-sitter-md` opens a `section` from an `atx_heading` only — its
    /// `_section1`..`_section6` rules all start with one — so the extent
    /// [`Self::markdown_heading_fold`] reads off the tree does not exist for a
    /// setext heading, and the whole-document walk works it out lexically
    /// instead.
    ///
    /// ADR §5.10 decision 6 keeps the heading itself visible, and a setext
    /// heading is two lines: the fold starts at the end of the underline, which
    /// is therefore also the row the gutter offers it on.
    pub fn markdown_setext_heading_fold(
        &self,
        buffer_row: MultiBufferRow,
    ) -> Option<MarkdownHeadingFold> {
        // The gutter asks for a crease per visible row and `editor::FoldAll`
        // for every row in the buffer, of whatever language, but nothing here
        // reads the row: [`Self::markdown_scan_key`] turns away every buffer
        // that is not a Markdown singleton before that.
        self.with_markdown_scan(|scan| {
            scan.setext_headings
                .binary_search_by_key(&buffer_row.0, |heading| heading.range.start.row)
                .ok()
                .and_then(|index| scan.setext_headings.get(index).cloned())
        })
        .flatten()
    }

    /// Every Markdown heading fold in the buffer, in document order.
    ///
    /// Folding a whole file needs every heading at once, and asking
    /// [`Self::markdown_heading_fold`] row by row would set up one tree-sitter
    /// query per heading; this walks the tree once instead, so a document with
    /// thousands of headings stays a single pass. The setext headings come from
    /// the whole-document walk, which is where they are worked out in either
    /// case, so the two forms cannot disagree.
    pub fn markdown_heading_folds(&self) -> Vec<MarkdownHeadingFold> {
        let snapshot = self.buffer_snapshot();
        let mut folds = snapshot
            .text_object_ranges(
                MultiBufferOffset(0)..snapshot.len(),
                TreeSitterOptions::default(),
            )
            .filter(|(_, text_object)| *text_object == TextObject::AroundClass)
            .filter_map(|(range, _)| {
                let section_start = range.start.to_point(snapshot);
                // Sections of an injected language inside a fenced code block
                // reach this too; a heading always starts its own line, and
                // the level test below rejects the rest.
                if section_start.column != 0 {
                    return None;
                }
                let level = self.markdown_heading_level_at(section_start.row)?;
                self.markdown_heading_fold_extent(
                    section_start.row,
                    level,
                    range.end.to_point(snapshot),
                )
            })
            .collect::<Vec<_>>();
        folds.extend(
            self.with_markdown_scan(|scan| scan.setext_headings.clone())
                .unwrap_or_default(),
        );
        // A nested section's match completes before the section enclosing it,
        // so the query hands these back child-first rather than in the order
        // the headings appear.
        folds.sort_by_key(|heading| heading.range.start);
        folds
    }

    /// The fold a Markdown row offers besides the heading folds: a fenced code
    /// block or a `<details>` disclosure, each leaving its own first line
    /// visible the way a folded function leaves its signature.
    pub fn markdown_block_fold(
        &self,
        buffer_row: MultiBufferRow,
    ) -> Option<Range<MultiBufferPoint>> {
        // The gutter asks for a crease per visible row and `editor::FoldAll`
        // asks for every row in the buffer, of whatever language, so the
        // language test comes before the row is even read, and one read of the
        // row then serves both of the tests below.
        if !self.is_markdown_buffer_row(buffer_row.0) {
            return None;
        }
        let line = self.markdown_line(buffer_row.0);
        if markdown_fence_delimiter(&line).is_some() {
            self.markdown_fence_fold(buffer_row)
        } else if markdown_details_block_open(&line) {
            self.markdown_details_fold(buffer_row, &line)
        } else {
            None
        }
    }

    /// The range the opening fence of a fenced code block folds away: the fence
    /// line stays visible and everything through the closing fence is hidden.
    /// A fence that is never closed runs to the end of the buffer, which is the
    /// extent the grammar gives it.
    ///
    /// Markdown has no fold query in this tree, so the extent comes from the
    /// `fenced_code_block` text object in `textobjects.scm`.
    fn markdown_fence_fold(&self, buffer_row: MultiBufferRow) -> Option<Range<MultiBufferPoint>> {
        // `text_object_ranges` reports the captures of every layer, so the
        // functions of an injected language inside a ```rust block arrive here
        // too. Only the Markdown layer captures a fence row.
        if !self.is_markdown_layer_row(buffer_row.0) {
            return None;
        }

        let snapshot = self.buffer_snapshot();
        let row_start = MultiBufferPoint::new(buffer_row.0, 0).to_offset(snapshot);
        let row_end =
            MultiBufferPoint::new(buffer_row.0, snapshot.line_len(buffer_row)).to_offset(snapshot);
        let block_end = snapshot
            .text_object_ranges(row_start..row_end, TreeSitterOptions::default())
            .find_map(|(range, text_object)| {
                // A closing fence has the same block as its ancestor, so the
                // capture has to start on this row for it to be the opening
                // one. An indented fence starts past column 0, which is why
                // this compares rows rather than offsets.
                (text_object == TextObject::AroundFunction
                    && range.start.to_point(snapshot).row == buffer_row.0)
                    .then_some(range.end)
            })?
            .to_point(snapshot);

        // The block takes in the newline that ends its closing fence, which
        // puts its end at the start of the row after the block.
        let end_row = if block_end.column == 0 {
            block_end.row.saturating_sub(1)
        } else {
            block_end.row
        }
        .min(snapshot.max_row().0);
        if end_row <= buffer_row.0 {
            return None;
        }

        Some(
            MultiBufferPoint::new(buffer_row.0, snapshot.line_len(buffer_row))
                ..MultiBufferPoint::new(end_row, snapshot.line_len(MultiBufferRow(end_row))),
        )
    }

    /// The range a `<details>` row folds away: the opening tag's line — which is
    /// where `markdown::InsertDetails` puts the `<summary>` — stays visible and
    /// everything through the matching `</details>` is hidden.
    ///
    /// A CommonMark HTML block ends at the first blank line, so the opening tag,
    /// the body and `</details>` are separate blocks and no tree-sitter node
    /// spans the pair. The tags are matched by walking the rows instead,
    /// skipping fenced code blocks so that a `</details>` quoted in an example
    /// closes nothing.
    fn markdown_details_fold(
        &self,
        buffer_row: MultiBufferRow,
        line: &str,
    ) -> Option<Range<MultiBufferPoint>> {
        let mut depth = 0;
        if markdown_details_tags(line, &mut depth) || depth == 0 {
            // The disclosure is closed on its own line, so it has no body rows
            // to hide.
            return None;
        }
        if self.markdown_row_is_inside_fence(buffer_row.0) {
            return None;
        }

        let end_row = match self.with_markdown_scan(|scan| scan.details_close_row(buffer_row.0)) {
            Some(close_row) => close_row?,
            None => self.markdown_details_close_row_nearby(buffer_row.0, depth)?,
        };
        if end_row <= buffer_row.0 {
            return None;
        }

        let snapshot = self.buffer_snapshot();
        Some(
            MultiBufferPoint::new(buffer_row.0, snapshot.line_len(buffer_row))
                ..MultiBufferPoint::new(end_row, snapshot.line_len(MultiBufferRow(end_row))),
        )
    }

    /// Where the disclosure opened on `open_row` at `depth` is closed, found by
    /// walking forward at most [`MARKDOWN_DETAILS_SCAN_LIMIT`] rows.
    ///
    /// This is what answers where no whole-document walk is kept — a
    /// multi-excerpt multibuffer, or a document too large to walk on every edit.
    /// The gutter asks once per visible row and `editor::FoldAll` once per row
    /// of the buffer, so a disclosure that is never closed must not walk a large
    /// file to its end each time it is drawn; past the limit it offers no fold
    /// rather than a wrong one.
    fn markdown_details_close_row_nearby(&self, open_row: u32, mut depth: usize) -> Option<u32> {
        let max_row = self.buffer_snapshot().max_row().0;
        let scan_end = max_row.min(open_row.saturating_add(MARKDOWN_DETAILS_SCAN_LIMIT));
        let mut open_fence = None;
        let mut close_row = None;
        let reached_scan_end = self.markdown_rows_from(
            open_row + 1,
            scan_end,
            MARKDOWN_DETAILS_SCAN_CHAR_LIMIT,
            |row, line| {
                match open_fence {
                    Some((marker, length)) => {
                        if is_closing_fence(line, marker, length) {
                            open_fence = None;
                        }
                    }
                    None => {
                        if let Some(fence) = markdown_fence_delimiter(line) {
                            open_fence = Some(fence);
                        } else if markdown_details_tags(line, &mut depth) {
                            close_row = Some(row);
                            return true;
                        }
                    }
                }
                false
            },
        );

        match close_row {
            Some(row) => Some(row),
            // An unclosed `<details>` still has a body to hide, but only a walk
            // that reached the end of the buffer rather than one of its own
            // limits knows that there is no closing tag further down.
            None if reached_scan_end && scan_end == max_row => Some(max_row),
            None => None,
        }
    }

    /// Whether `row` sits inside a fenced code block, where a `<details>` tag is
    /// an example rather than a disclosure.
    fn markdown_row_is_inside_fence(&self, row: u32) -> bool {
        let snapshot = self.buffer_snapshot();
        let row_start = MultiBufferPoint::new(row, 0).to_offset(snapshot);
        let row_end =
            MultiBufferPoint::new(row, snapshot.line_len(MultiBufferRow(row))).to_offset(snapshot);
        snapshot
            .text_object_ranges(row_start..row_end, TreeSitterOptions::default())
            .any(|(range, text_object)| {
                text_object == TextObject::AroundFunction
                    && range.start < row_start
                    && self.is_markdown_layer_row(range.start.to_point(snapshot).row)
            })
    }

    /// Returns the indent length of `row` if it starts with a closing bracket.
    fn closing_bracket_indent_len(&self, row: u32) -> Option<u32> {
        let snapshot = self.buffer_snapshot();
        let indent_len = self
            .line_indent_for_buffer_row(MultiBufferRow(row))
            .raw_len();
        let content_start = Point::new(row, indent_len);
        let line_text: String = snapshot
            .chars_at(content_start)
            .take_while(|ch| *ch != '\n')
            .collect();

        let scope = snapshot.language_scope_at(Point::new(row, 0))?;
        if scope
            .brackets()
            .any(|(pair, _)| line_text.starts_with(&pair.end))
        {
            return Some(indent_len);
        }

        None
    }

    #[instrument(skip_all)]
    pub fn crease_for_buffer_row(&self, buffer_row: MultiBufferRow) -> Option<Crease<Point>> {
        let start =
            MultiBufferPoint::new(buffer_row.0, self.buffer_snapshot().line_len(buffer_row));
        if let Some(crease) = self
            .crease_snapshot
            .query_row(buffer_row, self.buffer_snapshot())
        {
            match crease {
                Crease::Inline {
                    range,
                    placeholder,
                    render_toggle,
                    render_trailer,
                    metadata,
                } => Some(Crease::Inline {
                    range: range.to_point(self.buffer_snapshot()),
                    placeholder: placeholder.clone(),
                    render_toggle: render_toggle.clone(),
                    render_trailer: render_trailer.clone(),
                    metadata: metadata.clone(),
                }),
                Crease::Block {
                    range,
                    block_height,
                    block_style,
                    render_block,
                    block_priority,
                    render_toggle,
                } => Some(Crease::Block {
                    range: range.to_point(self.buffer_snapshot()),
                    block_height: *block_height,
                    block_style: *block_style,
                    render_block: render_block.clone(),
                    block_priority: *block_priority,
                    render_toggle: render_toggle.clone(),
                }),
            }
        } else if !self.is_line_folded(MultiBufferRow(start.row))
            && let Some(heading) = self.markdown_heading_fold(buffer_row)
        {
            Some(Crease::Inline {
                range: heading.range,
                placeholder: self.fold_placeholder.clone(),
                render_toggle: None,
                render_trailer: None,
                metadata: None,
            })
        } else if !self.is_line_folded(MultiBufferRow(start.row))
            && let Some(heading) = self.markdown_setext_heading_fold(buffer_row)
        {
            Some(Crease::Inline {
                range: heading.range,
                placeholder: self.fold_placeholder.clone(),
                render_toggle: None,
                render_trailer: None,
                metadata: None,
            })
        } else if !self.is_line_folded(MultiBufferRow(start.row))
            && let Some(range) = self.markdown_block_fold(buffer_row)
        {
            Some(Crease::Inline {
                range,
                placeholder: self.fold_placeholder.clone(),
                render_toggle: None,
                render_trailer: None,
                metadata: None,
            })
        } else if !self.use_lsp_folding_ranges
            && self.starts_indent(MultiBufferRow(start.row))
            && !self.is_line_folded(MultiBufferRow(start.row))
        {
            let start_line_indent = self.line_indent_for_buffer_row(buffer_row);
            let snapshot = self.buffer_snapshot();
            let max_point = snapshot.max_point();
            let mut closing_row = None;

            // End byte of the smallest syntactic node enclosing `buffer_row`.
            // Used to tell standalone top-level comments (which terminate the
            // fold) apart from unindented content inside a multi-line string
            // or block comment belonging to the folded node (which does not).
            let foldable_node_end = {
                let row_start = Point::new(buffer_row.0, 0);
                let row_end = Point::new(buffer_row.0, snapshot.line_len(buffer_row));
                snapshot
                    .syntax_ancestor(row_start..row_end)
                    .map(|(_, range)| range.end)
            };

            for row in (buffer_row.0 + 1)..=max_point.row {
                let line_indent = self.line_indent_for_buffer_row(MultiBufferRow(row));
                if !line_indent.is_line_blank()
                    && line_indent.raw_len() <= start_line_indent.raw_len()
                {
                    let in_string_or_comment_scope = snapshot
                        .language_scope_at(Point::new(row, 0))
                        .is_some_and(|scope| {
                            matches!(
                                scope.override_name(),
                                Some("string") | Some("comment") | Some("comment.inclusive")
                            )
                        });
                    if in_string_or_comment_scope
                        && let Some(end) = foldable_node_end
                        && Point::new(row, 0).to_offset(snapshot) < end
                    {
                        continue;
                    }

                    closing_row = Some(row);
                    break;
                }
            }

            let last_non_blank_row = |from_row: u32| -> Point {
                let mut row = from_row;
                while row > start.row && self.buffer_snapshot().is_line_blank(MultiBufferRow(row)) {
                    row -= 1;
                }
                Point::new(row, self.buffer_snapshot().line_len(MultiBufferRow(row)))
            };

            let end = if let Some(row) = closing_row {
                if let Some(indent_len) = self.closing_bracket_indent_len(row) {
                    // Include newline and whitespace before closing delimiter,
                    // so it appears on the same display line as the fold placeholder
                    Point::new(row, indent_len)
                } else {
                    last_non_blank_row(row - 1)
                }
            } else {
                last_non_blank_row(max_point.row)
            };

            Some(Crease::Inline {
                range: start..end,
                placeholder: self.fold_placeholder.clone(),
                render_toggle: None,
                render_trailer: None,
                metadata: None,
            })
        } else {
            None
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[instrument(skip_all)]
    pub fn text_highlight_ranges(
        &self,
        key: HighlightKey,
    ) -> Option<Arc<(HighlightStyle, Vec<Range<Anchor>>)>> {
        self.text_highlights.get(&key).cloned()
    }

    #[cfg(any(test, feature = "test-support"))]
    #[instrument(skip_all)]
    pub fn all_text_highlight_ranges(
        &self,
        f: &dyn Fn(&HighlightKey) -> bool,
    ) -> Vec<(gpui::Hsla, Range<Point>)> {
        use itertools::Itertools;

        self.text_highlights
            .iter()
            .filter(|(key, _)| f(key))
            .map(|(_, value)| value.clone())
            .flat_map(|ranges| {
                ranges
                    .1
                    .iter()
                    .flat_map(|range| {
                        Some((ranges.0.color?, range.to_point(self.buffer_snapshot())))
                    })
                    .collect::<Vec<_>>()
            })
            .sorted_by_key(|(_, range)| range.start)
            .collect()
    }

    #[allow(unused)]
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn inlay_highlights(
        &self,
        key: HighlightKey,
    ) -> Option<&TreeMap<InlayId, (HighlightStyle, InlayHighlight)>> {
        self.inlay_highlights.get(&key)
    }

    pub fn buffer_header_height(&self) -> u32 {
        self.block_snapshot.buffer_header_height
    }

    pub fn excerpt_header_height(&self) -> u32 {
        self.block_snapshot.excerpt_header_height
    }

    /// Given a `DisplayPoint`, returns another `DisplayPoint` corresponding to
    /// the start of the buffer row that is a given number of buffer rows away
    /// from the provided point.
    ///
    /// This moves by buffer rows instead of display rows, a distinction that is
    /// important when soft wrapping is enabled.
    #[instrument(skip_all)]
    pub fn start_of_relative_buffer_row(&self, point: DisplayPoint, times: isize) -> DisplayPoint {
        let start = self.display_point_to_fold_point(point, Bias::Left);
        let target = start.row() as isize + times;
        let new_row = (target.max(0) as u32).min(self.fold_snapshot().max_point().row());

        self.clip_point(
            self.fold_point_to_display_point(
                self.fold_snapshot()
                    .clip_point(FoldPoint::new(new_row, 0), Bias::Right),
            ),
            Bias::Right,
        )
    }
}

fn diagnostic_style(severity: lsp::DiagnosticSeverity, colors: &StatusColors) -> Hsla {
    match severity {
        lsp::DiagnosticSeverity::ERROR => colors.error,
        lsp::DiagnosticSeverity::WARNING => colors.warning,
        lsp::DiagnosticSeverity::INFORMATION => colors.info,
        lsp::DiagnosticSeverity::HINT => colors.hint,
        _ => colors.ignored,
    }
}

impl std::ops::Deref for DisplaySnapshot {
    type Target = BlockSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.block_snapshot
    }
}

/// A zero-indexed point in a text buffer consisting of a row and column adjusted for inserted blocks.
#[derive(Copy, Clone, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct DisplayPoint(BlockPoint);

impl Debug for DisplayPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "DisplayPoint({}, {})",
            self.row().0,
            self.column()
        ))
    }
}

impl Add for DisplayPoint {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        DisplayPoint(BlockPoint(self.0.0 + other.0.0))
    }
}

impl Sub for DisplayPoint {
    type Output = Self;

    fn sub(self, other: Self) -> Self::Output {
        DisplayPoint(BlockPoint(self.0.0 - other.0.0))
    }
}

#[derive(Debug, Copy, Clone, Default, Eq, Ord, PartialOrd, PartialEq, Deserialize, Hash)]
#[serde(transparent)]
pub struct DisplayRow(pub u32);

impl DisplayRow {
    pub(crate) fn as_display_point(&self) -> DisplayPoint {
        DisplayPoint::new(*self, 0)
    }
}

impl Add<DisplayRow> for DisplayRow {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        DisplayRow(self.0 + other.0)
    }
}

impl Add<u32> for DisplayRow {
    type Output = Self;

    fn add(self, other: u32) -> Self::Output {
        DisplayRow(self.0 + other)
    }
}

impl Sub<DisplayRow> for DisplayRow {
    type Output = Self;

    fn sub(self, other: Self) -> Self::Output {
        DisplayRow(self.0 - other.0)
    }
}

impl Sub<u32> for DisplayRow {
    type Output = Self;

    fn sub(self, other: u32) -> Self::Output {
        DisplayRow(self.0 - other)
    }
}

impl DisplayPoint {
    pub fn new(row: DisplayRow, column: u32) -> Self {
        Self(BlockPoint(Point::new(row.0, column)))
    }

    pub fn zero() -> Self {
        Self::new(DisplayRow(0), 0)
    }

    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    pub fn row(self) -> DisplayRow {
        DisplayRow(self.0.row)
    }

    pub fn column(self) -> u32 {
        self.0.column
    }

    pub fn row_mut(&mut self) -> &mut u32 {
        &mut self.0.row
    }

    pub fn column_mut(&mut self) -> &mut u32 {
        &mut self.0.column
    }

    pub fn to_point(self, map: &DisplaySnapshot) -> Point {
        map.display_point_to_point(self, Bias::Left)
    }

    pub fn to_offset(self, map: &DisplaySnapshot, bias: Bias) -> MultiBufferOffset {
        let wrap_point = map.block_snapshot.to_wrap_point(self.0, bias);
        let tab_point = map.wrap_snapshot().to_tab_point(wrap_point);
        let fold_point = map
            .tab_snapshot()
            .tab_point_to_fold_point(tab_point, bias)
            .0;
        let inlay_point = fold_point.to_inlay_point(map.fold_snapshot());
        map.inlay_snapshot()
            .to_buffer_offset(map.inlay_snapshot().to_offset(inlay_point))
    }
}

impl ToDisplayPoint for MultiBufferOffset {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        map.point_to_display_point(self.to_point(map.buffer_snapshot()), Bias::Left)
    }
}

impl ToDisplayPoint for MultiBufferOffsetUtf16 {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        self.to_offset(map.buffer_snapshot()).to_display_point(map)
    }
}

impl ToDisplayPoint for Point {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        map.point_to_display_point(*self, Bias::Left)
    }
}

impl ToDisplayPoint for Anchor {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        self.to_point(map.buffer_snapshot()).to_display_point(map)
    }
}

/// Maps buffer offset ranges to `DisplayPoint` ranges covering only buffer text
/// (excluding inlay text), reusing cursor state across calls.
///
/// Created via [`DisplaySnapshot::display_point_converter`]. Each layer
/// (inlay -> fold -> tab -> wrap -> block) is backed by a forward-only cursor,
/// so it is most efficient when ranges are supplied with non-decreasing
/// offsets. If a range starts before the previous one ended, the cursors are
/// transparently reset so the result stays correct (at the cost of an extra
/// seek), which keeps the converter robust to overlapping inputs such as the
/// base and buffer word diffs of an inline modified hunk.
pub struct DisplayPointConverter<'a> {
    inlay_cursor: BufferOffsetToInlayPointCursor<'a>,
    fold_point_cursor: FoldPointCursor<'a>,
    tab_point_cursor: TabPointCursor<'a>,
    wrap_point_cursor: WrapPointCursor<'a>,
    block_point_cursor: BlockPointCursor<'a>,
    prev_end: Option<MultiBufferOffset>,
}

impl DisplayPointConverter<'_> {
    pub fn map(&mut self, range: Range<MultiBufferOffset>) -> SmallVec<[Range<DisplayPoint>; 1]> {
        if self.prev_end.is_some_and(|prev_end| range.start < prev_end) {
            // The input went backward relative to where the cursors are
            // positioned; reset them so they can seek freely.
            self.inlay_cursor.reset();
            self.fold_point_cursor.reset();
            self.tab_point_cursor.reset();
            self.wrap_point_cursor.reset();
            self.block_point_cursor.reset();
        }
        self.prev_end = Some(range.end);

        let inlay_ranges = self.inlay_cursor.map(range);
        inlay_ranges
            .into_iter()
            .map(|inlay_range| {
                let start = self.inlay_point_to_display_point(inlay_range.start);
                let end = self.inlay_point_to_display_point(inlay_range.end);
                start..end
            })
            .collect()
    }

    fn inlay_point_to_display_point(&mut self, inlay_point: InlayPoint) -> DisplayPoint {
        let fold_point = self.fold_point_cursor.map(inlay_point, Bias::Left);
        let tab_point = self.tab_point_cursor.map(fold_point);
        let wrap_point = self.wrap_point_cursor.map(tab_point);
        DisplayPoint(self.block_point_cursor.map(wrap_point))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::{
        movement,
        test::{marked_display_snapshot, test_font},
    };
    use Bias::*;
    use block_map::BlockPlacement;
    use gpui::{
        App, AppContext as _, BorrowAppContext, Element, Hsla, Rgba, div, font, observe, px,
    };
    use language::{
        Buffer, Diagnostic, DiagnosticEntry, DiagnosticSet, Language, LanguageConfig,
        LanguageMatcher,
    };
    use lsp::LanguageServerId;

    use futures::stream::StreamExt;
    use multi_buffer::PathKey;
    use rand::{Rng, prelude::*};
    use settings::{SettingsContent, SettingsStore};
    use std::{env, sync::Arc};
    use text::PointUtf16;
    use theme::{LoadThemes, SyntaxTheme};
    use unindent::Unindent as _;
    use util::test::{marked_text_ranges, sample_text};

    #[test]
    fn test_markdown_fence_delimiter() {
        assert_eq!(markdown_fence_delimiter("```"), Some(('`', 3)));
        assert_eq!(markdown_fence_delimiter("~~~~"), Some(('~', 4)));
        assert_eq!(markdown_fence_delimiter("```rust"), Some(('`', 3)));
        // A tilde fence may carry backticks in its info string; a backtick one
        // may not, which is what keeps `a ``b`` c` from reading as a fence.
        assert_eq!(markdown_fence_delimiter("~~~ `x`"), Some(('~', 3)));
        assert_eq!(markdown_fence_delimiter("``` `x`"), None);
        assert_eq!(markdown_fence_delimiter("``"), None, "two is too few");
        assert_eq!(markdown_fence_delimiter("   ```"), Some(('`', 3)));
        assert_eq!(
            markdown_fence_delimiter("    ```"),
            None,
            "four spaces make an indented code block, not a fence"
        );
        assert_eq!(markdown_fence_delimiter("\t```"), None);
        assert_eq!(markdown_fence_delimiter("- ```"), None);
        assert_eq!(markdown_fence_delimiter(""), None);
        assert_eq!(markdown_fence_delimiter("日本語```"), None);
    }

    #[test]
    fn test_markdown_details_block_open() {
        assert!(markdown_details_block_open("<details>"));
        assert!(markdown_details_block_open("<DETAILS>"));
        assert!(markdown_details_block_open("<details open>"));
        assert!(markdown_details_block_open("<Details class=\"x\">"));
        assert!(markdown_details_block_open(
            "   <details><summary>a</summary>"
        ));
        assert!(
            markdown_details_block_open("<details"),
            "a tag broken across lines still opens a block"
        );
        assert!(
            !markdown_details_block_open("    <details>"),
            "four spaces make an indented code block"
        );
        assert!(!markdown_details_block_open("<detailsx>"));
        assert!(!markdown_details_block_open("<detail>"));
        assert!(!markdown_details_block_open("text <details>"));
        assert!(!markdown_details_block_open("> <details>"));
        assert!(!markdown_details_block_open("`<details>`"));
        assert!(
            !markdown_details_block_open("日本<details>"),
            "a multi-byte prefix must not be sliced through"
        );
        assert!(!markdown_details_block_open("日"));
        assert!(!markdown_details_block_open(""));
    }

    #[test]
    fn test_markdown_details_tags() {
        let tags = |line: &str, start: usize| {
            let mut depth = start;
            let closed = markdown_details_tags(line, &mut depth);
            (closed, depth)
        };

        assert_eq!(tags("<details>", 0), (false, 1));
        assert_eq!(tags("</details>", 1), (true, 0));
        assert_eq!(tags("</DETAILS >", 1), (true, 0));
        assert_eq!(
            tags("<details><summary>a</summary>本文</details>", 0),
            (true, 0),
            "a disclosure written on one line closes on it"
        );
        assert_eq!(
            tags("</details><details>", 1),
            (true, 0),
            "a line that closes one disclosure and opens the next ends the first \
             one, whatever it goes on to open"
        );
        assert_eq!(
            tags("</details>", 2),
            (false, 1),
            "the scan reports only the disclosure it started from"
        );
        assert_eq!(
            tags("</details>", 0),
            (false, 0),
            "a stray closing tag closes nothing"
        );
        assert_eq!(tags("<detailsx></detailsx>", 1), (false, 1));
        assert_eq!(
            tags("<!-- <details> --></details>", 1),
            (true, 0),
            "a tag inside a comment is not counted"
        );
        assert_eq!(
            tags("<!-- </details>", 1),
            (false, 1),
            "a comment left open swallows the rest of its line"
        );
        assert_eq!(
            tags("日本語 </details> 日本語", 1),
            (true, 0),
            "a multi-byte line is scanned on character boundaries"
        );
        assert_eq!(tags("ただの本文", 1), (false, 1));
    }

    #[gpui::test(iterations = 100)]
    async fn test_random_display_map(cx: &mut gpui::TestAppContext, mut rng: StdRng) {
        cx.background_executor.set_block_on_ticks(0..=50);
        let operations = env::var("OPERATIONS")
            .map(|i| i.parse().expect("invalid `OPERATIONS` variable"))
            .unwrap_or(10);

        let mut tab_size = rng.random_range(1..=4);
        let buffer_start_excerpt_header_height = rng.random_range(1..=5);
        let excerpt_header_height = rng.random_range(1..=5);
        let font_size = px(14.0);
        let max_wrap_width = 300.0;
        let mut wrap_width = if rng.random_bool(0.1) {
            None
        } else {
            Some(px(rng.random_range(0.0..=max_wrap_width)))
        };

        log::info!("tab size: {}", tab_size);
        log::info!("wrap width: {:?}", wrap_width);

        cx.update(|cx| {
            init_test(cx, &|s| {
                s.project.all_languages.defaults.tab_size = NonZeroU32::new(tab_size)
            });
        });

        let buffer = cx.update(|cx| {
            if rng.random() {
                let len = rng.random_range(0..10);
                let text = util::RandomCharIter::new(&mut rng)
                    .take(len)
                    .collect::<String>();
                MultiBuffer::build_simple(&text, cx)
            } else {
                MultiBuffer::build_random(&mut rng, cx)
            }
        });

        let font = test_font();
        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer.clone(),
                font,
                font_size,
                wrap_width,
                buffer_start_excerpt_header_height,
                excerpt_header_height,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });
        let mut notifications = observe(&map, cx);
        let mut fold_count = 0;
        let mut blocks = Vec::new();

        let snapshot = map.update(cx, |map, cx| map.snapshot(cx));
        log::info!("buffer text: {:?}", snapshot.buffer_snapshot().text());
        log::info!("fold text: {:?}", snapshot.fold_snapshot().text());
        log::info!("tab text: {:?}", snapshot.tab_snapshot().text());
        log::info!("wrap text: {:?}", snapshot.wrap_snapshot().text());
        log::info!("block text: {:?}", snapshot.block_snapshot.text());
        log::info!("display text: {:?}", snapshot.text());

        for _i in 0..operations {
            match rng.random_range(0..100) {
                0..=19 => {
                    wrap_width = if rng.random_bool(0.2) {
                        None
                    } else {
                        Some(px(rng.random_range(0.0..=max_wrap_width)))
                    };
                    log::info!("setting wrap width to {:?}", wrap_width);
                    map.update(cx, |map, cx| map.set_wrap_width(wrap_width, cx));
                }
                20..=29 => {
                    let mut tab_sizes = vec![1, 2, 3, 4];
                    tab_sizes.remove((tab_size - 1) as usize);
                    tab_size = *tab_sizes.choose(&mut rng).unwrap();
                    log::info!("setting tab size to {:?}", tab_size);
                    cx.update(|cx| {
                        cx.update_global::<SettingsStore, _>(|store, cx| {
                            store.update_user_settings(cx, |s| {
                                s.project.all_languages.defaults.tab_size =
                                    NonZeroU32::new(tab_size);
                            });
                        });
                    });
                }
                30..=44 => {
                    map.update(cx, |map, cx| {
                        if rng.random() || blocks.is_empty() {
                            let snapshot = map.snapshot(cx);
                            let buffer = snapshot.buffer_snapshot();
                            let block_properties = (0..rng.random_range(1..=1))
                                .map(|_| {
                                    let position = buffer.anchor_after(buffer.clip_offset(
                                        rng.random_range(MultiBufferOffset(0)..=buffer.len()),
                                        Bias::Left,
                                    ));

                                    let placement = if rng.random() {
                                        BlockPlacement::Above(position)
                                    } else {
                                        BlockPlacement::Below(position)
                                    };
                                    let height = rng.random_range(1..5);
                                    log::info!(
                                        "inserting block {:?} with height {}",
                                        placement.as_ref().map(|p| p.to_point(&buffer)),
                                        height
                                    );
                                    let priority = rng.random_range(1..100);
                                    BlockProperties {
                                        placement,
                                        style: BlockStyle::Fixed,
                                        height: Some(height),
                                        render: Arc::new(|_| div().into_any()),
                                        priority,
                                    }
                                })
                                .collect::<Vec<_>>();
                            blocks.extend(map.insert_blocks(block_properties, cx));
                        } else {
                            blocks.shuffle(&mut rng);
                            let remove_count = rng.random_range(1..=4.min(blocks.len()));
                            let block_ids_to_remove = (0..remove_count)
                                .map(|_| blocks.remove(rng.random_range(0..blocks.len())))
                                .collect();
                            log::info!("removing block ids {:?}", block_ids_to_remove);
                            map.remove_blocks(block_ids_to_remove, cx);
                        }
                    });
                }
                45..=79 => {
                    let mut ranges = Vec::new();
                    for _ in 0..rng.random_range(1..=3) {
                        buffer.read_with(cx, |buffer, cx| {
                            let buffer = buffer.read(cx);
                            let end = buffer.clip_offset(
                                rng.random_range(MultiBufferOffset(0)..=buffer.len()),
                                Right,
                            );
                            let start = buffer
                                .clip_offset(rng.random_range(MultiBufferOffset(0)..=end), Left);
                            ranges.push(start..end);
                        });
                    }

                    if rng.random() && fold_count > 0 {
                        log::info!("unfolding ranges: {:?}", ranges);
                        map.update(cx, |map, cx| {
                            map.unfold_intersecting(ranges, true, cx);
                        });
                    } else {
                        log::info!("folding ranges: {:?}", ranges);
                        map.update(cx, |map, cx| {
                            map.fold(
                                ranges
                                    .into_iter()
                                    .map(|range| Crease::simple(range, FoldPlaceholder::test()))
                                    .collect(),
                                cx,
                            );
                        });
                    }
                }
                _ => {
                    buffer.update(cx, |buffer, cx| buffer.randomly_mutate(&mut rng, 5, cx));
                }
            }

            if map.read_with(cx, |map, cx| map.is_rewrapping(cx)) {
                notifications.next().await.unwrap();
            }

            let snapshot = map.update(cx, |map, cx| map.snapshot(cx));
            fold_count = snapshot.fold_count();
            log::info!("buffer text: {:?}", snapshot.buffer_snapshot().text());
            log::info!("fold text: {:?}", snapshot.fold_snapshot().text());
            log::info!("tab text: {:?}", snapshot.tab_snapshot().text());
            log::info!("wrap text: {:?}", snapshot.wrap_snapshot().text());
            log::info!("block text: {:?}", snapshot.block_snapshot.text());
            log::info!("display text: {:?}", snapshot.text());

            // Line boundaries
            let buffer = snapshot.buffer_snapshot();
            for _ in 0..5 {
                let row = rng.random_range(0..=buffer.max_point().row);
                let column = rng.random_range(0..=buffer.line_len(MultiBufferRow(row)));
                let point = buffer.clip_point(Point::new(row, column), Left);

                let (prev_buffer_bound, prev_display_bound) = snapshot.prev_line_boundary(point);
                let (next_buffer_bound, next_display_bound) = snapshot.next_line_boundary(point);

                assert!(prev_buffer_bound <= point);
                assert!(next_buffer_bound >= point);
                assert_eq!(prev_buffer_bound.column, 0);
                assert_eq!(prev_display_bound.column(), 0);
                if next_buffer_bound < buffer.max_point() {
                    assert_eq!(buffer.chars_at(next_buffer_bound).next(), Some('\n'));
                }

                assert_eq!(
                    prev_display_bound,
                    prev_buffer_bound.to_display_point(&snapshot),
                    "row boundary before {:?}. reported buffer row boundary: {:?}",
                    point,
                    prev_buffer_bound
                );
                assert_eq!(
                    next_display_bound,
                    next_buffer_bound.to_display_point(&snapshot),
                    "display row boundary after {:?}. reported buffer row boundary: {:?}",
                    point,
                    next_buffer_bound
                );
                assert_eq!(
                    prev_buffer_bound,
                    prev_display_bound.to_point(&snapshot),
                    "row boundary before {:?}. reported display row boundary: {:?}",
                    point,
                    prev_display_bound
                );
                assert_eq!(
                    next_buffer_bound,
                    next_display_bound.to_point(&snapshot),
                    "row boundary after {:?}. reported display row boundary: {:?}",
                    point,
                    next_display_bound
                );
            }

            // Movement
            let min_point = snapshot.clip_point(DisplayPoint::new(DisplayRow(0), 0), Left);
            let max_point = snapshot.clip_point(snapshot.max_point(), Right);
            for _ in 0..5 {
                let row = rng.random_range(0..=snapshot.max_point().row().0);
                let column = rng.random_range(0..=snapshot.line_len(DisplayRow(row)));
                let point = snapshot.clip_point(DisplayPoint::new(DisplayRow(row), column), Left);

                log::info!("Moving from point {:?}", point);

                let moved_right = movement::right(&snapshot, point);
                log::info!("Right {:?}", moved_right);
                if point < max_point {
                    assert!(moved_right > point);
                    if point.column() == snapshot.line_len(point.row())
                        || snapshot.soft_wrap_indent(point.row()).is_some()
                            && point.column() == snapshot.line_len(point.row()) - 1
                    {
                        assert!(moved_right.row() > point.row());
                    }
                } else {
                    assert_eq!(moved_right, point);
                }

                let moved_left = movement::left(&snapshot, point);
                log::info!("Left {:?}", moved_left);
                if point > min_point {
                    assert!(moved_left < point);
                    if point.column() == 0 {
                        assert!(moved_left.row() < point.row());
                    }
                } else {
                    assert_eq!(moved_left, point);
                }
            }
        }
    }

    #[gpui::test(retries = 5)]
    async fn test_soft_wraps(cx: &mut gpui::TestAppContext) {
        cx.background_executor
            .set_block_on_ticks(usize::MAX..=usize::MAX);
        cx.update(|cx| {
            init_test(cx, &|_| {});
        });

        let mut cx = crate::test::editor_test_context::EditorTestContext::new(cx).await;
        let editor = cx.editor.clone();
        let window = cx.window;

        _ = cx.update_window(window, |_, window, cx| {
            let text_layout_details =
                editor.update(cx, |editor, cx| editor.text_layout_details(window, cx));

            let font_size = px(12.0);
            let wrap_width = Some(px(96.));

            let text = "one two three four five\nsix seven eight";
            let buffer = MultiBuffer::build_simple(text, cx);
            let map = cx.new(|cx| {
                DisplayMap::new(
                    buffer.clone(),
                    font("Helvetica"),
                    font_size,
                    wrap_width,
                    1,
                    1,
                    FoldPlaceholder::test(),
                    DiagnosticSeverity::Warning,
                    cx,
                )
            });

            let snapshot = map.update(cx, |map, cx| map.snapshot(cx));
            assert_eq!(
                snapshot.text_chunks(DisplayRow(0)).collect::<String>(),
                "one two \nthree four \nfive\nsix seven \neight"
            );
            assert_eq!(
                snapshot.clip_point(DisplayPoint::new(DisplayRow(0), 8), Bias::Left),
                DisplayPoint::new(DisplayRow(0), 7)
            );
            assert_eq!(
                snapshot.clip_point(DisplayPoint::new(DisplayRow(0), 8), Bias::Right),
                DisplayPoint::new(DisplayRow(1), 0)
            );
            assert_eq!(
                movement::right(&snapshot, DisplayPoint::new(DisplayRow(0), 7)),
                DisplayPoint::new(DisplayRow(1), 0)
            );
            assert_eq!(
                movement::left(&snapshot, DisplayPoint::new(DisplayRow(1), 0)),
                DisplayPoint::new(DisplayRow(0), 7)
            );

            let x = snapshot
                .x_for_display_point(DisplayPoint::new(DisplayRow(1), 10), &text_layout_details);
            assert_eq!(
                movement::up(
                    &snapshot,
                    DisplayPoint::new(DisplayRow(1), 10),
                    language::SelectionGoal::None,
                    false,
                    &text_layout_details,
                ),
                (
                    DisplayPoint::new(DisplayRow(0), 7),
                    language::SelectionGoal::HorizontalPosition(f64::from(x))
                )
            );
            assert_eq!(
                movement::down(
                    &snapshot,
                    DisplayPoint::new(DisplayRow(0), 7),
                    language::SelectionGoal::HorizontalPosition(f64::from(x)),
                    false,
                    &text_layout_details
                ),
                (
                    DisplayPoint::new(DisplayRow(1), 10),
                    language::SelectionGoal::HorizontalPosition(f64::from(x))
                )
            );
            assert_eq!(
                movement::down(
                    &snapshot,
                    DisplayPoint::new(DisplayRow(1), 10),
                    language::SelectionGoal::HorizontalPosition(f64::from(x)),
                    false,
                    &text_layout_details
                ),
                (
                    DisplayPoint::new(DisplayRow(2), 4),
                    language::SelectionGoal::HorizontalPosition(f64::from(x))
                )
            );

            let ix = MultiBufferOffset(snapshot.buffer_snapshot().text().find("seven").unwrap());
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(ix..ix, "and ")], None, cx);
            });

            let snapshot = map.update(cx, |map, cx| map.snapshot(cx));
            assert_eq!(
                snapshot.text_chunks(DisplayRow(1)).collect::<String>(),
                "three four \nfive\nsix and \nseven eight"
            );

            // Re-wrap on font size changes
            map.update(cx, |map, cx| {
                map.set_font(font("Helvetica"), font_size + Pixels::from(3.), cx)
            });

            let snapshot = map.update(cx, |map, cx| map.snapshot(cx));
            assert_eq!(
                snapshot.text_chunks(DisplayRow(1)).collect::<String>(),
                "three \nfour five\nsix and \nseven \neight"
            )
        });
    }

    #[gpui::test]
    fn test_text_chunks(cx: &mut gpui::App) {
        init_test(cx, &|_| {});

        let text = sample_text(6, 6, 'a');
        let buffer = MultiBuffer::build_simple(&text, cx);

        let font_size = px(14.0);
        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer.clone(),
                font("Helvetica"),
                font_size,
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });

        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                vec![
                    (
                        MultiBufferPoint::new(1, 0)..MultiBufferPoint::new(1, 0),
                        "\t",
                    ),
                    (
                        MultiBufferPoint::new(1, 1)..MultiBufferPoint::new(1, 1),
                        "\t",
                    ),
                    (
                        MultiBufferPoint::new(2, 1)..MultiBufferPoint::new(2, 1),
                        "\t",
                    ),
                ],
                None,
                cx,
            )
        });

        assert_eq!(
            map.update(cx, |map, cx| map.snapshot(cx))
                .text_chunks(DisplayRow(1))
                .collect::<String>()
                .lines()
                .next(),
            Some("    b   bbbbb")
        );
        assert_eq!(
            map.update(cx, |map, cx| map.snapshot(cx))
                .text_chunks(DisplayRow(2))
                .collect::<String>()
                .lines()
                .next(),
            Some("c   ccccc")
        );
    }

    #[gpui::test]
    fn test_inlays_with_newlines_after_blocks(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| init_test(cx, &|_| {}));

        let buffer = cx.new(|cx| Buffer::local("a", cx));
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        let buffer_snapshot = buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx));

        let font_size = px(14.0);
        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer.clone(),
                font("Helvetica"),
                font_size,
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });

        map.update(cx, |map, cx| {
            map.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Above(
                        buffer_snapshot.anchor_before(Point::new(0, 0)),
                    ),
                    height: Some(2),
                    style: BlockStyle::Sticky,
                    render: Arc::new(|_| div().into_any()),
                    priority: 0,
                }],
                cx,
            );
        });
        map.update(cx, |m, cx| assert_eq!(m.snapshot(cx).text(), "\n\na"));

        map.update(cx, |map, cx| {
            map.splice_inlays(
                &[],
                vec![Inlay::edit_prediction(
                    0,
                    buffer_snapshot.anchor_after(MultiBufferOffset(0)),
                    "\n",
                )],
                cx,
            );
        });
        map.update(cx, |m, cx| assert_eq!(m.snapshot(cx).text(), "\n\n\na"));

        // Regression test: updating the display map does not crash when a
        // block is immediately followed by a multi-line inlay.
        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [(MultiBufferOffset(1)..MultiBufferOffset(1), "b")],
                None,
                cx,
            );
        });
        map.update(cx, |m, cx| assert_eq!(m.snapshot(cx).text(), "\n\n\nab"));
    }

    #[gpui::test]
    async fn test_chunks(cx: &mut gpui::TestAppContext) {
        let text = r#"
            fn outer() {}

            mod module {
                fn inner() {}
            }"#
        .unindent();

        let theme =
            SyntaxTheme::new_test(vec![("mod.body", Hsla::red()), ("fn.name", Hsla::blue())]);
        let language = Arc::new(
            Language::new(
                LanguageConfig {
                    name: "Test".into(),
                    matcher: (LanguageMatcher {
                        path_suffixes: vec![".test".to_string()],
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                },
                Some(tree_sitter_rust::LANGUAGE.into()),
            )
            .with_highlights_query(
                r#"
                (mod_item name: (identifier) body: _ @mod.body)
                (function_item name: (identifier) @fn.name)
                "#,
            )
            .unwrap(),
        );
        language.set_theme(&theme);

        cx.update(|cx| {
            init_test(cx, &|s| {
                s.project.all_languages.defaults.tab_size = Some(2.try_into().unwrap())
            })
        });

        let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(language, cx));
        cx.condition(&buffer, |buf, _| !buf.is_parsing()).await;
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        let font_size = px(14.0);

        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer,
                font("Helvetica"),
                font_size,
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });
        assert_eq!(
            cx.update(|cx| syntax_chunks(DisplayRow(0)..DisplayRow(5), &map, &theme, cx)),
            vec![
                ("fn ".to_string(), None),
                ("outer".to_string(), Some(Hsla::blue())),
                ("() {}\n\nmod module ".to_string(), None),
                ("{\n    fn ".to_string(), Some(Hsla::red())),
                ("inner".to_string(), Some(Hsla::blue())),
                ("() {}\n}".to_string(), Some(Hsla::red())),
            ]
        );
        assert_eq!(
            cx.update(|cx| syntax_chunks(DisplayRow(3)..DisplayRow(5), &map, &theme, cx)),
            vec![
                ("    fn ".to_string(), Some(Hsla::red())),
                ("inner".to_string(), Some(Hsla::blue())),
                ("() {}\n}".to_string(), Some(Hsla::red())),
            ]
        );

        map.update(cx, |map, cx| {
            map.fold(
                vec![Crease::simple(
                    MultiBufferPoint::new(0, 6)..MultiBufferPoint::new(3, 2),
                    FoldPlaceholder::test(),
                )],
                cx,
            )
        });
        assert_eq!(
            cx.update(|cx| syntax_chunks(DisplayRow(0)..DisplayRow(2), &map, &theme, cx)),
            vec![
                ("fn ".to_string(), None),
                ("out".to_string(), Some(Hsla::blue())),
                ("⋯".to_string(), None),
                ("  fn ".to_string(), Some(Hsla::red())),
                ("inner".to_string(), Some(Hsla::blue())),
                ("() {}\n}".to_string(), Some(Hsla::red())),
            ]
        );
    }

    #[gpui::test]
    async fn test_chunks_with_syntax_highlighting_across_blocks(cx: &mut gpui::TestAppContext) {
        cx.background_executor
            .set_block_on_ticks(usize::MAX..=usize::MAX);

        let text = r#"
            const A: &str = "
                one
                two
                three
            ";
            const B: &str = "four";
        "#
        .unindent();

        let theme = SyntaxTheme::new_test(vec![
            ("string", Hsla::red()),
            ("punctuation", Hsla::blue()),
            ("keyword", Hsla::green()),
        ]);
        let language = Arc::new(
            Language::new(
                LanguageConfig {
                    name: "Rust".into(),
                    ..Default::default()
                },
                Some(tree_sitter_rust::LANGUAGE.into()),
            )
            .with_highlights_query(
                r#"
                (string_literal) @string
                "const" @keyword
                [":" ";"] @punctuation
                "#,
            )
            .unwrap(),
        );
        language.set_theme(&theme);

        cx.update(|cx| init_test(cx, &|_| {}));

        let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(language, cx));
        cx.condition(&buffer, |buf, _| !buf.is_parsing()).await;
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        let buffer_snapshot = buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx));

        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer,
                font("Courier"),
                px(16.0),
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });

        // Insert two blocks in the middle of a multi-line string literal.
        // The second block has zero height.
        map.update(cx, |map, cx| {
            map.insert_blocks(
                [
                    BlockProperties {
                        placement: BlockPlacement::Below(
                            buffer_snapshot.anchor_before(Point::new(1, 0)),
                        ),
                        height: Some(1),
                        style: BlockStyle::Sticky,
                        render: Arc::new(|_| div().into_any()),
                        priority: 0,
                    },
                    BlockProperties {
                        placement: BlockPlacement::Below(
                            buffer_snapshot.anchor_before(Point::new(2, 0)),
                        ),
                        height: None,
                        style: BlockStyle::Sticky,
                        render: Arc::new(|_| div().into_any()),
                        priority: 0,
                    },
                ],
                cx,
            )
        });

        pretty_assertions::assert_eq!(
            cx.update(|cx| syntax_chunks(DisplayRow(0)..DisplayRow(7), &map, &theme, cx)),
            [
                ("const".into(), Some(Hsla::green())),
                (" A".into(), None),
                (":".into(), Some(Hsla::blue())),
                (" &str = ".into(), None),
                ("\"\n    one\n".into(), Some(Hsla::red())),
                ("\n".into(), None),
                ("    two\n    three\n\"".into(), Some(Hsla::red())),
                (";".into(), Some(Hsla::blue())),
                ("\n".into(), None),
                ("const".into(), Some(Hsla::green())),
                (" B".into(), None),
                (":".into(), Some(Hsla::blue())),
                (" &str = ".into(), None),
                ("\"four\"".into(), Some(Hsla::red())),
                (";".into(), Some(Hsla::blue())),
                ("\n".into(), None),
            ]
        );
    }

    #[gpui::test]
    async fn test_chunks_with_diagnostics_across_blocks(cx: &mut gpui::TestAppContext) {
        cx.background_executor
            .set_block_on_ticks(usize::MAX..=usize::MAX);

        let text = r#"
            struct A {
                b: usize;
            }
            const c: usize = 1;
        "#
        .unindent();

        cx.update(|cx| init_test(cx, &|_| {}));

        let buffer = cx.new(|cx| Buffer::local(text, cx));

        buffer.update(cx, |buffer, cx| {
            buffer.update_diagnostics(
                LanguageServerId(0),
                DiagnosticSet::new(
                    [DiagnosticEntry::new(
                        PointUtf16::new(0, 0)..PointUtf16::new(2, 1),
                        Diagnostic {
                            severity: lsp::DiagnosticSeverity::ERROR,
                            group_id: 1,
                            message: "hi".into(),
                            ..Default::default()
                        },
                    )],
                    buffer,
                ),
                cx,
            )
        });

        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        let buffer_snapshot = buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx));

        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer,
                font("Courier"),
                px(16.0),
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });

        let black = gpui::black().to_rgb();
        let red = gpui::red().to_rgb();

        // Insert a block in the middle of a multi-line diagnostic.
        map.update(cx, |map, cx| {
            map.highlight_text(
                HighlightKey::Editor,
                vec![
                    buffer_snapshot.anchor_before(Point::new(3, 9))
                        ..buffer_snapshot.anchor_after(Point::new(3, 14)),
                    buffer_snapshot.anchor_before(Point::new(3, 17))
                        ..buffer_snapshot.anchor_after(Point::new(3, 18)),
                ],
                red.into(),
                false,
                cx,
            );
            map.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Below(
                        buffer_snapshot.anchor_before(Point::new(1, 0)),
                    ),
                    height: Some(1),
                    style: BlockStyle::Sticky,
                    render: Arc::new(|_| div().into_any()),
                    priority: 0,
                }],
                cx,
            )
        });

        let snapshot = map.update(cx, |map, cx| map.snapshot(cx));
        let mut chunks = Vec::<(String, Option<lsp::DiagnosticSeverity>, Rgba)>::new();
        for chunk in snapshot.chunks(
            DisplayRow(0)..DisplayRow(5),
            LanguageAwareStyling {
                tree_sitter: true,
                diagnostics: true,
            },
            Default::default(),
        ) {
            let color = chunk
                .highlight_style
                .and_then(|style| style.color)
                .map_or(black, |color| color.to_rgb());
            if let Some((last_chunk, last_severity, last_color)) = chunks.last_mut()
                && *last_severity == chunk.diagnostic_severity
                && *last_color == color
            {
                last_chunk.push_str(chunk.text);
                continue;
            }

            chunks.push((chunk.text.to_string(), chunk.diagnostic_severity, color));
        }

        assert_eq!(
            chunks,
            [
                (
                    "struct A {\n    b: usize;\n".into(),
                    Some(lsp::DiagnosticSeverity::ERROR),
                    black
                ),
                ("\n".into(), None, black),
                ("}".into(), Some(lsp::DiagnosticSeverity::ERROR), black),
                ("\nconst c: ".into(), None, black),
                ("usize".into(), None, red),
                (" = ".into(), None, black),
                ("1".into(), None, red),
                (";\n".into(), None, black),
            ]
        );
    }

    #[gpui::test]
    async fn test_point_translation_with_replace_blocks(cx: &mut gpui::TestAppContext) {
        cx.background_executor
            .set_block_on_ticks(usize::MAX..=usize::MAX);

        cx.update(|cx| init_test(cx, &|_| {}));

        let buffer = cx.update(|cx| MultiBuffer::build_simple("abcde\nfghij\nklmno\npqrst", cx));
        let buffer_snapshot = buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx));
        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer.clone(),
                font("Courier"),
                px(16.0),
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });

        let snapshot = map.update(cx, |map, cx| {
            map.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Replace(
                        buffer_snapshot.anchor_before(Point::new(1, 2))
                            ..=buffer_snapshot.anchor_after(Point::new(2, 3)),
                    ),
                    height: Some(4),
                    style: BlockStyle::Fixed,
                    render: Arc::new(|_| div().into_any()),
                    priority: 0,
                }],
                cx,
            );
            map.snapshot(cx)
        });

        assert_eq!(snapshot.text(), "abcde\n\n\n\n\npqrst");

        let point_to_display_points = [
            (Point::new(1, 0), DisplayPoint::new(DisplayRow(1), 0)),
            (Point::new(2, 0), DisplayPoint::new(DisplayRow(1), 0)),
            (Point::new(3, 0), DisplayPoint::new(DisplayRow(5), 0)),
        ];
        for (buffer_point, display_point) in point_to_display_points {
            assert_eq!(
                snapshot.point_to_display_point(buffer_point, Bias::Left),
                display_point,
                "point_to_display_point({:?}, Bias::Left)",
                buffer_point
            );
            assert_eq!(
                snapshot.point_to_display_point(buffer_point, Bias::Right),
                display_point,
                "point_to_display_point({:?}, Bias::Right)",
                buffer_point
            );
        }

        let display_points_to_points = [
            (
                DisplayPoint::new(DisplayRow(1), 0),
                Point::new(1, 0),
                Point::new(2, 5),
            ),
            (
                DisplayPoint::new(DisplayRow(2), 0),
                Point::new(1, 0),
                Point::new(2, 5),
            ),
            (
                DisplayPoint::new(DisplayRow(3), 0),
                Point::new(1, 0),
                Point::new(2, 5),
            ),
            (
                DisplayPoint::new(DisplayRow(4), 0),
                Point::new(1, 0),
                Point::new(2, 5),
            ),
            (
                DisplayPoint::new(DisplayRow(5), 0),
                Point::new(3, 0),
                Point::new(3, 0),
            ),
        ];
        for (display_point, left_buffer_point, right_buffer_point) in display_points_to_points {
            assert_eq!(
                snapshot.display_point_to_point(display_point, Bias::Left),
                left_buffer_point,
                "display_point_to_point({:?}, Bias::Left)",
                display_point
            );
            assert_eq!(
                snapshot.display_point_to_point(display_point, Bias::Right),
                right_buffer_point,
                "display_point_to_point({:?}, Bias::Right)",
                display_point
            );
        }
    }

    #[gpui::test]
    async fn test_chunks_with_soft_wrapping(cx: &mut gpui::TestAppContext) {
        cx.background_executor
            .set_block_on_ticks(usize::MAX..=usize::MAX);

        let text = r#"
            fn outer() {}

            mod module {
                fn inner() {}
            }"#
        .unindent();

        let theme =
            SyntaxTheme::new_test(vec![("mod.body", Hsla::red()), ("fn.name", Hsla::blue())]);
        let language = Arc::new(
            Language::new(
                LanguageConfig {
                    name: "Test".into(),
                    matcher: (LanguageMatcher {
                        path_suffixes: vec![".test".to_string()],
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                },
                Some(tree_sitter_rust::LANGUAGE.into()),
            )
            .with_highlights_query(
                r#"
                (mod_item name: (identifier) body: _ @mod.body)
                (function_item name: (identifier) @fn.name)
                "#,
            )
            .unwrap(),
        );
        language.set_theme(&theme);

        cx.update(|cx| init_test(cx, &|_| {}));

        let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(language, cx));
        cx.condition(&buffer, |buf, _| !buf.is_parsing()).await;
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        let font_size = px(16.0);

        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer,
                font("Courier"),
                font_size,
                Some(px(40.0)),
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });
        assert_eq!(
            cx.update(|cx| syntax_chunks(DisplayRow(0)..DisplayRow(5), &map, &theme, cx)),
            [
                ("fn \n".to_string(), None),
                ("oute".to_string(), Some(Hsla::blue())),
                ("\n".to_string(), None),
                ("r".to_string(), Some(Hsla::blue())),
                ("() \n{}\n\n".to_string(), None),
            ]
        );
        assert_eq!(
            cx.update(|cx| syntax_chunks(DisplayRow(3)..DisplayRow(5), &map, &theme, cx)),
            [("{}\n\n".to_string(), None)]
        );

        map.update(cx, |map, cx| {
            map.fold(
                vec![Crease::simple(
                    MultiBufferPoint::new(0, 6)..MultiBufferPoint::new(3, 2),
                    FoldPlaceholder::test(),
                )],
                cx,
            )
        });
        assert_eq!(
            cx.update(|cx| syntax_chunks(DisplayRow(1)..DisplayRow(4), &map, &theme, cx)),
            [
                ("out".to_string(), Some(Hsla::blue())),
                ("⋯\n".to_string(), None),
                ("  ".to_string(), Some(Hsla::red())),
                ("\n".to_string(), None),
                ("fn ".to_string(), Some(Hsla::red())),
                ("i".to_string(), Some(Hsla::blue())),
                ("\n".to_string(), None)
            ]
        );
    }

    #[gpui::test]
    async fn test_chunks_with_text_highlights(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| init_test(cx, &|_| {}));

        let theme =
            SyntaxTheme::new_test(vec![("operator", Hsla::red()), ("string", Hsla::green())]);
        let language = Arc::new(
            Language::new(
                LanguageConfig {
                    name: "Test".into(),
                    matcher: (LanguageMatcher {
                        path_suffixes: vec![".test".to_string()],
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                },
                Some(tree_sitter_rust::LANGUAGE.into()),
            )
            .with_highlights_query(
                r#"
                ":" @operator
                (string_literal) @string
                "#,
            )
            .unwrap(),
        );
        language.set_theme(&theme);

        let (text, highlighted_ranges) = marked_text_ranges(r#"constˇ «a»«:» B = "c «d»""#, false);

        let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(language, cx));
        cx.condition(&buffer, |buf, _| !buf.is_parsing()).await;

        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        let buffer_snapshot = buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx));

        let font_size = px(16.0);
        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer,
                font("Courier"),
                font_size,
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });

        let style = HighlightStyle {
            color: Some(Hsla::blue()),
            ..Default::default()
        };

        map.update(cx, |map, cx| {
            map.highlight_text(
                HighlightKey::Editor,
                highlighted_ranges
                    .into_iter()
                    .map(|range| MultiBufferOffset(range.start)..MultiBufferOffset(range.end))
                    .map(|range| {
                        buffer_snapshot.anchor_before(range.start)
                            ..buffer_snapshot.anchor_before(range.end)
                    })
                    .collect(),
                style,
                false,
                cx,
            );
        });

        assert_eq!(
            cx.update(|cx| chunks(DisplayRow(0)..DisplayRow(10), &map, &theme, cx)),
            [
                ("const ".to_string(), None, None),
                ("a".to_string(), None, Some(Hsla::blue())),
                (":".to_string(), Some(Hsla::red()), Some(Hsla::blue())),
                (" B = ".to_string(), None, None),
                ("\"c ".to_string(), Some(Hsla::green()), None),
                ("d".to_string(), Some(Hsla::green()), Some(Hsla::blue())),
                ("\"".to_string(), Some(Hsla::green()), None),
            ]
        );
    }

    #[gpui::test]
    fn test_clip_point(cx: &mut gpui::App) {
        init_test(cx, &|_| {});

        fn assert(text: &str, shift_right: bool, bias: Bias, cx: &mut gpui::App) {
            let (unmarked_snapshot, mut markers) = marked_display_snapshot(text, cx);

            match bias {
                Bias::Left => {
                    if shift_right {
                        *markers[1].column_mut() += 1;
                    }

                    assert_eq!(unmarked_snapshot.clip_point(markers[1], bias), markers[0])
                }
                Bias::Right => {
                    if shift_right {
                        *markers[0].column_mut() += 1;
                    }

                    assert_eq!(unmarked_snapshot.clip_point(markers[0], bias), markers[1])
                }
            };
        }

        use Bias::{Left, Right};
        assert("ˇˇα", false, Left, cx);
        assert("ˇˇα", true, Left, cx);
        assert("ˇˇα", false, Right, cx);
        assert("ˇαˇ", true, Right, cx);
        assert("ˇˇ✋", false, Left, cx);
        assert("ˇˇ✋", true, Left, cx);
        assert("ˇˇ✋", false, Right, cx);
        assert("ˇ✋ˇ", true, Right, cx);
        assert("ˇˇ🍐", false, Left, cx);
        assert("ˇˇ🍐", true, Left, cx);
        assert("ˇˇ🍐", false, Right, cx);
        assert("ˇ🍐ˇ", true, Right, cx);
        assert("ˇˇ\t", false, Left, cx);
        assert("ˇˇ\t", true, Left, cx);
        assert("ˇˇ\t", false, Right, cx);
        assert("ˇ\tˇ", true, Right, cx);
        assert(" ˇˇ\t", false, Left, cx);
        assert(" ˇˇ\t", true, Left, cx);
        assert(" ˇˇ\t", false, Right, cx);
        assert(" ˇ\tˇ", true, Right, cx);
        assert("   ˇˇ\t", false, Left, cx);
        assert("   ˇˇ\t", false, Right, cx);
    }

    #[gpui::test]
    fn test_clip_at_line_ends(cx: &mut gpui::App) {
        init_test(cx, &|_| {});

        fn assert(text: &str, cx: &mut gpui::App) {
            let (mut unmarked_snapshot, markers) = marked_display_snapshot(text, cx);
            unmarked_snapshot.clip_at_line_ends = true;
            assert_eq!(
                unmarked_snapshot.clip_point(markers[1], Bias::Left),
                markers[0]
            );
        }

        assert("ˇˇ", cx);
        assert("ˇaˇ", cx);
        assert("aˇbˇ", cx);
        assert("aˇαˇ", cx);
    }

    #[gpui::test]
    fn test_creases(cx: &mut gpui::App) {
        init_test(cx, &|_| {});

        let text = "aaa\nbbb\nccc\nddd\neee\nfff\nggg\nhhh\niii\njjj\nkkk\nlll";
        let buffer = MultiBuffer::build_simple(text, cx);
        let font_size = px(14.0);
        cx.new(|cx| {
            let mut map = DisplayMap::new(
                buffer.clone(),
                font("Helvetica"),
                font_size,
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            );
            let snapshot = map.buffer.read(cx).snapshot(cx);
            let range =
                snapshot.anchor_before(Point::new(2, 0))..snapshot.anchor_after(Point::new(3, 3));

            map.crease_map.insert(
                [Crease::inline(
                    range,
                    FoldPlaceholder::test(),
                    |_row, _status, _toggle, _window, _cx| div(),
                    |_row, _status, _window, _cx| div(),
                )],
                &map.buffer.read(cx).snapshot(cx),
            );

            map
        });
    }

    #[gpui::test]
    fn test_tabs_with_multibyte_chars(cx: &mut gpui::App) {
        init_test(cx, &|_| {});

        let text = "✅\t\tα\nβ\t\n🏀β\t\tγ";
        let buffer = MultiBuffer::build_simple(text, cx);
        let font_size = px(14.0);

        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer.clone(),
                font("Helvetica"),
                font_size,
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });
        let map = map.update(cx, |map, cx| map.snapshot(cx));
        assert_eq!(map.text(), "✅       α\nβ   \n🏀β      γ");
        assert_eq!(
            map.text_chunks(DisplayRow(0)).collect::<String>(),
            "✅       α\nβ   \n🏀β      γ"
        );
        assert_eq!(
            map.text_chunks(DisplayRow(1)).collect::<String>(),
            "β   \n🏀β      γ"
        );
        assert_eq!(
            map.text_chunks(DisplayRow(2)).collect::<String>(),
            "🏀β      γ"
        );

        let point = MultiBufferPoint::new(0, "✅\t\t".len() as u32);
        let display_point = DisplayPoint::new(DisplayRow(0), "✅       ".len() as u32);
        assert_eq!(point.to_display_point(&map), display_point);
        assert_eq!(display_point.to_point(&map), point);

        let point = MultiBufferPoint::new(1, "β\t".len() as u32);
        let display_point = DisplayPoint::new(DisplayRow(1), "β   ".len() as u32);
        assert_eq!(point.to_display_point(&map), display_point);
        assert_eq!(display_point.to_point(&map), point,);

        let point = MultiBufferPoint::new(2, "🏀β\t\t".len() as u32);
        let display_point = DisplayPoint::new(DisplayRow(2), "🏀β      ".len() as u32);
        assert_eq!(point.to_display_point(&map), display_point);
        assert_eq!(display_point.to_point(&map), point,);

        // Display points inside of expanded tabs
        assert_eq!(
            DisplayPoint::new(DisplayRow(0), "✅      ".len() as u32).to_point(&map),
            MultiBufferPoint::new(0, "✅\t".len() as u32),
        );
        assert_eq!(
            DisplayPoint::new(DisplayRow(0), "✅ ".len() as u32).to_point(&map),
            MultiBufferPoint::new(0, "✅".len() as u32),
        );

        // Clipping display points inside of multi-byte characters
        assert_eq!(
            map.clip_point(
                DisplayPoint::new(DisplayRow(0), "✅".len() as u32 - 1),
                Left
            ),
            DisplayPoint::new(DisplayRow(0), 0)
        );
        assert_eq!(
            map.clip_point(
                DisplayPoint::new(DisplayRow(0), "✅".len() as u32 - 1),
                Bias::Right
            ),
            DisplayPoint::new(DisplayRow(0), "✅".len() as u32)
        );
    }

    #[gpui::test]
    fn test_max_point(cx: &mut gpui::App) {
        init_test(cx, &|_| {});

        let buffer = MultiBuffer::build_simple("aaa\n\t\tbbb", cx);
        let font_size = px(14.0);
        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer.clone(),
                font("Helvetica"),
                font_size,
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });
        assert_eq!(
            map.update(cx, |map, cx| map.snapshot(cx)).max_point(),
            DisplayPoint::new(DisplayRow(1), 11)
        )
    }

    fn syntax_chunks(
        rows: Range<DisplayRow>,
        map: &Entity<DisplayMap>,
        theme: &SyntaxTheme,
        cx: &mut App,
    ) -> Vec<(String, Option<Hsla>)> {
        chunks(rows, map, theme, cx)
            .into_iter()
            .map(|(text, color, _)| (text, color))
            .collect()
    }

    fn chunks(
        rows: Range<DisplayRow>,
        map: &Entity<DisplayMap>,
        theme: &SyntaxTheme,
        cx: &mut App,
    ) -> Vec<(String, Option<Hsla>, Option<Hsla>)> {
        let snapshot = map.update(cx, |map, cx| map.snapshot(cx));
        let mut chunks: Vec<(String, Option<Hsla>, Option<Hsla>)> = Vec::new();
        for chunk in snapshot.chunks(
            rows,
            LanguageAwareStyling {
                tree_sitter: true,
                diagnostics: true,
            },
            HighlightStyles::default(),
        ) {
            let syntax_color = chunk
                .syntax_highlight_id
                .and_then(|id| theme.get(id)?.color);

            let highlight_color = chunk.highlight_style.and_then(|style| style.color);
            if let Some((last_chunk, last_syntax_color, last_highlight_color)) = chunks.last_mut()
                && syntax_color == *last_syntax_color
                && highlight_color == *last_highlight_color
            {
                last_chunk.push_str(chunk.text);
                continue;
            }
            chunks.push((chunk.text.to_string(), syntax_color, highlight_color));
        }
        chunks
    }

    /// Asserts that every header-like block in the snapshot references a
    /// buffer that is still present in the multibuffer: the invariant whose
    /// violation panics at render time with "buffer snapshot not found for
    /// excerpt boundary" (ZED-7G6).
    #[track_caller]
    fn assert_headers_resolve(snapshot: &DisplaySnapshot) {
        let end_row = DisplayRow(snapshot.max_point().row().0 + 1);
        for (row, block) in snapshot.blocks_in_range(DisplayRow(0)..end_row) {
            let excerpt = match block {
                Block::BufferHeader { excerpt, .. } | Block::ExcerptBoundary { excerpt, .. } => {
                    excerpt
                }
                Block::FoldedBuffer { first_excerpt, .. } => first_excerpt,
                _ => continue,
            };
            assert!(
                snapshot
                    .buffer_snapshot()
                    .buffer_for_id(excerpt.buffer_id())
                    .is_some(),
                "stale header block {:?} at {row:?} references buffer {:?}, \
                 which is no longer in the multibuffer",
                block.id(),
                excerpt.buffer_id(),
            );
        }
    }

    /// Deterministic end-to-end regression test for ZED-7G6 ("buffer snapshot
    /// not found for excerpt boundary"), driving a real `DisplayMap` with
    /// ordinary operations. In a diff-backed multibuffer with all hunks
    /// expanded, two folds inside an expanded deleted hunk are ordered only by
    /// their diff base anchors. Replacing the diff's base text used to invert
    /// that order (comparison filtered diff base anchors on validity, which
    /// the base edit revoked for one anchor of the pair), silently unsorting
    /// the fold map's persistent fold tree; subsequent syncs walked it with
    /// forward-only cursors and emitted edits that misdescribed the changed
    /// rows, until removing a buffer left its header block referencing a
    /// buffer absent from the snapshot -- the state whose render-time
    /// resolution panics.
    ///
    /// On pre-fix code this fails in the display map layers' internal
    /// checks; in production builds, where those checks don't run, the same
    /// corruption propagated to the stale header instead.
    #[gpui::test]
    async fn test_removing_buffer_removes_header_after_diff_base_changes(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| init_test(cx, &|_| {}));

        fn excerpt_buffer(
            multibuffer: &Entity<MultiBuffer>,
            path: u64,
            buffer: &Entity<Buffer>,
            cx: &mut gpui::TestAppContext,
        ) {
            multibuffer.update(cx, |multibuffer, cx| {
                let max_point = buffer.read(cx).max_point();
                multibuffer.set_excerpts_for_path(
                    PathKey::sorted(path),
                    buffer.clone(),
                    [Point::zero()..max_point],
                    0,
                    cx,
                );
            });
            cx.run_until_parked();
        }

        async fn set_base_text(
            diff: &Entity<buffer_diff::BufferDiff>,
            buffer: &Entity<Buffer>,
            base_text: &str,
            cx: &mut gpui::TestAppContext,
        ) {
            let snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
            diff.update(cx, |diff, cx| {
                diff.set_base_text(Some(base_text.to_string().into()), snapshot, cx)
            })
            .await;
            cx.run_until_parked();
        }

        #[track_caller]
        fn assert_headers(display_map: &Entity<DisplayMap>, cx: &mut gpui::TestAppContext) {
            cx.run_until_parked();
            let snapshot = display_map.update(cx, |display_map, cx| display_map.snapshot(cx));
            assert_headers_resolve(&snapshot);
        }

        let buffer_a = cx.new(|cx| Buffer::local("bbb\nccc\nddd\n", cx));
        let diff_a = cx.new(|cx| {
            buffer_diff::BufferDiff::new_with_base_text(
                "DEL1\nDEL2\nbbb\nccc\nddd\n",
                &buffer_a.read(cx).text_snapshot(),
                cx,
            )
        });
        let buffer_b = cx.new(|cx| Buffer::local("xxx\nyyy\n", cx));
        let buffer_b_id = buffer_b.read_with(cx, |buffer, _| buffer.remote_id());
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(language::Capability::ReadWrite);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        excerpt_buffer(&multibuffer, 0, &buffer_a, cx);
        excerpt_buffer(&multibuffer, 1, &buffer_b, cx);
        multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.add_diff(diff_a.clone(), cx);
        });
        cx.run_until_parked();

        let display_map = cx.new(|cx| {
            DisplayMap::new(
                multibuffer.clone(),
                test_font(),
                px(14.0),
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });
        assert_headers(&display_map, cx);

        // Two folds inside the expanded deleted hunk (rows 0 and 1 are
        // materialized from the base text), sharing a buffer position and
        // ordered only by their diff base anchors: a narrow fold within
        // "DEL1", then a wider fold from "DEL2" into the buffer's own rows.
        display_map.update(cx, |display_map, cx| {
            display_map.fold(
                vec![
                    Crease::simple(Point::new(0, 1)..Point::new(1, 0), FoldPlaceholder::test()),
                    Crease::simple(Point::new(1, 1)..Point::new(2, 2), FoldPlaceholder::test()),
                ],
                cx,
            );
        });
        assert_headers(&display_map, cx);

        // Keep "DEL1" (the first fold's base anchors survive) but delete
        // "DEL2" (the second fold's start anchor is tombstoned): the folds'
        // relative order must not change.
        set_base_text(&diff_a, &buffer_a, "DEL1\nbbb\nccc\nddd\n", cx).await;
        assert_headers(&display_map, cx);

        // Churn the buffers, the diff base, and buffer B's excerpts the way
        // the original fuzz sequence did, syncing the display map after each
        // group of operations.
        multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.remove_excerpts_for_buffer(buffer_b_id, cx);
        });
        buffer_a.update(cx, |buffer, cx| {
            buffer.edit([(4..5, "")], None, cx);
        });
        assert_headers(&display_map, cx);

        excerpt_buffer(&multibuffer, 1, &buffer_b, cx);
        set_base_text(&diff_a, &buffer_a, "DEL1\nbbb\nccc\nddd\n", cx).await;
        buffer_a.update(cx, |buffer, cx| {
            buffer.edit([(1..1, "Q\n")], None, cx);
        });
        assert_headers(&display_map, cx);

        set_base_text(&diff_a, &buffer_a, "DEL1\nbbb\nccc\nddd\n", cx).await;
        assert_headers(&display_map, cx);

        set_base_text(&diff_a, &buffer_a, "DEL2\nbbb\nccc\nddd\n", cx).await;
        buffer_a.update(cx, |buffer, cx| {
            buffer.edit([(2..2, "Q\n")], None, cx);
        });
        assert_headers(&display_map, cx);

        multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.remove_excerpts_for_buffer(buffer_b_id, cx);
        });
        excerpt_buffer(&multibuffer, 1, &buffer_b, cx);
        assert_headers(&display_map, cx);

        // Removing B must remove its header block: with the fold tree
        // corrupted, the removal edit's rows were misdescribed by the time
        // they reached the block map, B's header row went uncovered, and the
        // header survived pointing at a buffer absent from the snapshot.
        multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.remove_excerpts_for_buffer(buffer_b_id, cx);
        });
        assert_headers(&display_map, cx);
    }

    fn init_test(cx: &mut App, f: &dyn Fn(&mut SettingsContent)) {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        crate::init(cx);
        theme_settings::init(LoadThemes::JustBase, cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, f);
        });
    }

    #[gpui::test]
    fn test_isomorphic_display_point_ranges_for_buffer_range(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| init_test(cx, &|_| {}));

        let buffer = cx.new(|cx| Buffer::local("let x = 5;\n", cx));
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        let buffer_snapshot = buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx));

        let font_size = px(14.0);
        let map = cx.new(|cx| {
            DisplayMap::new(
                buffer.clone(),
                font("Helvetica"),
                font_size,
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });

        // Without inlays, a buffer range maps to a single display range.
        let snapshot = map.update(cx, |map, cx| map.snapshot(cx));
        let ranges = snapshot.isomorphic_display_point_ranges_for_buffer_range(
            MultiBufferOffset(4)..MultiBufferOffset(9),
        );
        assert_eq!(ranges.len(), 1);
        // "x = 5" is columns 4..9 with no inlays shifting anything.
        assert_eq!(ranges[0].start, DisplayPoint::new(DisplayRow(0), 4));
        assert_eq!(ranges[0].end, DisplayPoint::new(DisplayRow(0), 9));

        // Insert a 4-char inlay hint ": i32" at buffer offset 5 (after "x").
        map.update(cx, |map, cx| {
            map.splice_inlays(
                &[],
                vec![Inlay::mock_hint(
                    0,
                    buffer_snapshot.anchor_after(MultiBufferOffset(5)),
                    ": i32",
                )],
                cx,
            );
        });
        let snapshot = map.update(cx, |map, cx| map.snapshot(cx));
        assert_eq!(snapshot.text(), "let x: i32 = 5;\n");

        // A buffer range [4..9] ("x = 5") now spans across the inlay.
        // It should be split into two display ranges that skip the inlay text.
        let ranges = snapshot.isomorphic_display_point_ranges_for_buffer_range(
            MultiBufferOffset(4)..MultiBufferOffset(9),
        );
        assert_eq!(
            ranges.len(),
            2,
            "expected the range to be split around the inlay, got: {:?}",
            ranges,
        );
        // First sub-range: buffer [4, 5) → "x" at display columns 4..5
        assert_eq!(ranges[0].start, DisplayPoint::new(DisplayRow(0), 4));
        assert_eq!(ranges[0].end, DisplayPoint::new(DisplayRow(0), 5));
        // Second sub-range: buffer [5, 9) → " = 5" at display columns 10..14
        // (shifted right by the 5-char ": i32" inlay)
        assert_eq!(ranges[1].start, DisplayPoint::new(DisplayRow(0), 10));
        assert_eq!(ranges[1].end, DisplayPoint::new(DisplayRow(0), 14));

        // A range entirely before the inlay is not split.
        let ranges = snapshot.isomorphic_display_point_ranges_for_buffer_range(
            MultiBufferOffset(0)..MultiBufferOffset(5),
        );
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, DisplayPoint::new(DisplayRow(0), 0));
        assert_eq!(ranges[0].end, DisplayPoint::new(DisplayRow(0), 5));

        // A range entirely after the inlay is not split.
        let ranges = snapshot.isomorphic_display_point_ranges_for_buffer_range(
            MultiBufferOffset(5)..MultiBufferOffset(9),
        );
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, DisplayPoint::new(DisplayRow(0), 10));
        assert_eq!(ranges[0].end, DisplayPoint::new(DisplayRow(0), 14));

        map.update(cx, |map, cx| {
            map.splice_inlays(
                &[InlayId::Hint(0)],
                vec![
                    Inlay::mock_hint(1, buffer_snapshot.anchor_after(MultiBufferOffset(5)), "L"),
                    Inlay::mock_hint(2, buffer_snapshot.anchor_before(MultiBufferOffset(5)), "R"),
                    Inlay::mock_hint(3, buffer_snapshot.anchor_after(MultiBufferOffset(7)), "I"),
                ],
                cx,
            );
        });
        let snapshot = map.update(cx, |map, cx| map.snapshot(cx));

        assert_eq!(
            snapshot.contiguous_display_point_range_for_buffer_range(
                MultiBufferOffset(4)..MultiBufferOffset(5),
            ),
            Some(DisplayPoint::new(DisplayRow(0), 4)..DisplayPoint::new(DisplayRow(0), 5)),
        );
        assert_eq!(
            snapshot.contiguous_display_point_range_for_buffer_range(
                MultiBufferOffset(5)..MultiBufferOffset(6),
            ),
            Some(DisplayPoint::new(DisplayRow(0), 7)..DisplayPoint::new(DisplayRow(0), 8)),
        );
        assert_eq!(
            snapshot.contiguous_display_point_range_for_buffer_range(
                MultiBufferOffset(4)..MultiBufferOffset(6),
            ),
            Some(DisplayPoint::new(DisplayRow(0), 4)..DisplayPoint::new(DisplayRow(0), 8)),
        );
        assert_eq!(
            snapshot.contiguous_display_point_range_for_buffer_range(
                MultiBufferOffset(6)..MultiBufferOffset(7),
            ),
            Some(DisplayPoint::new(DisplayRow(0), 8)..DisplayPoint::new(DisplayRow(0), 9)),
        );
        assert_eq!(
            snapshot.contiguous_display_point_range_for_buffer_range(
                MultiBufferOffset(7)..MultiBufferOffset(8),
            ),
            Some(DisplayPoint::new(DisplayRow(0), 10)..DisplayPoint::new(DisplayRow(0), 11)),
        );
        assert_eq!(
            snapshot.contiguous_display_point_range_for_buffer_range(
                MultiBufferOffset(4)..MultiBufferOffset(9),
            ),
            Some(DisplayPoint::new(DisplayRow(0), 4)..DisplayPoint::new(DisplayRow(0), 12)),
        );
        assert_eq!(
            snapshot.contiguous_display_point_range_for_buffer_range(
                MultiBufferOffset(5)..MultiBufferOffset(5),
            ),
            None,
        );
    }

    #[test]
    fn test_highlight_invisibles_preserves_compound_emojis() {
        let editor_style = EditorStyle::default();

        let pilot_emoji = "🧑\u{200d}✈\u{fe0f}";
        let chunk = HighlightedChunk {
            text: pilot_emoji,
            style: None,
            is_tab: false,
            is_inlay: false,
            replacement: None,
        };

        let chunks: Vec<_> = chunk
            .highlight_invisibles(&editor_style)
            .map(|chunk| chunk.text.to_string())
            .collect();

        assert_eq!(
            chunks.concat(),
            pilot_emoji,
            "all text bytes must be preserved"
        );
        assert_eq!(
            chunks.len(),
            1,
            "compound emoji should not be split into multiple chunks, got: {:?}",
            chunks,
        );
    }

    /// Regression test: Creating a DisplayMap when the MultiBuffer has pending
    /// unsynced changes should not cause a desync between the subscription edits
    /// and the InlayMap's buffer state.
    ///
    /// The bug occurred because:
    /// 1. DisplayMap::new created a subscription first
    /// 2. Then called snapshot() which synced and published edits
    /// 3. InlayMap was created with the post-sync snapshot
    /// 4. But the subscription captured the sync edits, leading to double-application
    #[gpui::test]
    fn test_display_map_subscription_ordering(cx: &mut gpui::App) {
        init_test(cx, &|_| {});

        // Create a buffer with some initial text
        let buffer = cx.new(|cx| Buffer::local("initial", cx));
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));

        // Edit the buffer. This sets buffer_changed_since_sync = true.
        // Importantly, do NOT call multibuffer.snapshot() yet.
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "prefix ")], None, cx);
        });

        // Create the DisplayMap. In the buggy code, this would:
        // 1. Create subscription (empty)
        // 2. Call snapshot() which syncs and publishes edits E1
        // 3. Create InlayMap with post-E1 snapshot
        // 4. Subscription now has E1, but InlayMap is already at post-E1 state
        let map = cx.new(|cx| {
            DisplayMap::new(
                multibuffer.clone(),
                font("Helvetica"),
                px(14.0),
                None,
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });

        // Verify initial state is correct
        let snapshot = map.update(cx, |map, cx| map.snapshot(cx));
        assert_eq!(snapshot.text(), "prefix initial");

        // Make another edit
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(7..7, "more ")], None, cx);
        });

        // This would crash in the buggy code because:
        // - InlayMap expects edits from V1 to V2
        // - But subscription has E1 ∘ E2 (from V0 to V2)
        // - The calculation `buffer_edit.new.end + (cursor.end().0 - buffer_edit.old.end)`
        //   would produce an offset exceeding the buffer length
        let snapshot = map.update(cx, |map, cx| map.snapshot(cx));
        assert_eq!(snapshot.text(), "prefix more initial");
    }
}
