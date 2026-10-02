//! Shows an agent's highlights, annotations and code tours in the editor. Registers itself as
//! [`ide_query::GlobalCodeDisplay`] so `ide_server` and `agent` can call it without UI dependencies.
//!
//! Nothing here is saved: every workspace gets a [`WorkspaceAssist`] (which is also the status bar
//! item of the tour) and the displays go away with it.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId,
};
use editor::scroll::Autoscroll;
use editor::{Addon, Editor, RowHighlightOptions, SelectionEffects};
use gpui::{
    AnyElement, App, AsyncApp, Context, Entity, EntityId, Focusable as _, FontWeight, Hsla,
    KeyContext, Subscription, Task, TextStyleRefinement, WeakEntity, Window, actions,
};
use ide_query::{
    AssistSource, CodeAnnotation, CodeDisplay, CodeHighlight, CodeTour, DisplayOutcome,
    GlobalCodeDisplay, LineRange, MAX_ANNOTATIONS, MAX_DISPLAY_MARKDOWN_CHARS, MAX_HIGHLIGHTS,
    MAX_TOUR_STEPS, SkippedDisplay,
};
use language::{Buffer, LanguageRegistry, Point};
use markdown::parser::{MarkdownEvent, MarkdownTag, parse_markdown_events};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownOptions, MarkdownStyle, WrapButtonVisibility,
};
use project::{Project, ProjectPath};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, MultiWorkspace, Pane, StatusItemView, Workspace};

/// The same cap `ide_server` puts on the files a sandboxed agent may have read.
const MAX_DISPLAY_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_NOTE_CHARS: usize = 200;
const MAX_TITLE_CHARS: usize = 200;
const TOUR_KEY_CONTEXT: &str = "ide_assist_tour";

actions!(
    ide_assist,
    [
        /// Moves to the next step of the code tour an agent started.
        NextTourStep,
        /// Moves to the previous step of the code tour an agent started.
        PreviousTourStep,
        /// Ends the code tour an agent started.
        EndTour,
    ]
);

pub fn init(cx: &mut App) {
    cx.set_global(GlobalCodeDisplay(Arc::new(EditorCodeDisplay)));
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let workspace_handle = cx.entity();
        let languages = workspace.project().read(cx).languages().clone();
        let assist = cx.new(|cx| WorkspaceAssist::new(&workspace_handle, languages, cx));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(assist, window, cx);
        });
        workspace
            .register_action(|workspace, _: &NextTourStep, _, cx| {
                if let Some(assist) = workspace_assist(workspace, cx) {
                    assist.update(cx, |assist, cx| assist.step_by(1, cx));
                }
            })
            .register_action(|workspace, _: &PreviousTourStep, _, cx| {
                if let Some(assist) = workspace_assist(workspace, cx) {
                    assist.update(cx, |assist, cx| assist.step_by(-1, cx));
                }
            })
            .register_action(|workspace, _: &EndTour, _, cx| {
                if let Some(assist) = workspace_assist(workspace, cx) {
                    assist.update(cx, |assist, cx| assist.end_tour(cx));
                }
            });
    })
    .detach();
}

/// Orders the requests of one source so that a slow earlier call can't overwrite a later one
/// (or bring back what `clear` removed).
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

fn next_request() -> u64 {
    NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
}

struct EditorCodeDisplay;

impl CodeDisplay for EditorCodeDisplay {
    fn show_highlights(
        &self,
        project: &Entity<Project>,
        source: AssistSource,
        highlights: Vec<CodeHighlight>,
        cx: &mut App,
    ) -> Task<Result<DisplayOutcome>> {
        if highlights.len() > MAX_HIGHLIGHTS {
            return Task::ready(Err(anyhow!(
                "At most {MAX_HIGHLIGHTS} highlights can be shown at once ({} were given)",
                highlights.len()
            )));
        }
        let request = next_request();
        let project = project.clone();
        cx.spawn(async move |cx| {
            let assist = cx.update(|cx| assist_for_project(&project, cx))?;
            let targets = resolve_targets(
                &project,
                highlights
                    .iter()
                    .map(|highlight| (highlight.path.clone(), highlight.lines))
                    .collect(),
                cx,
            )
            .await;
            let mut pending = Vec::new();
            let mut skipped = Vec::new();
            for (highlight, target) in highlights.into_iter().zip(targets) {
                match target {
                    Ok(target) => pending.push(PendingDisplay {
                        path: highlight.path,
                        target,
                        content: PendingContent::Highlight {
                            note: highlight
                                .note
                                .as_deref()
                                .map(|note| single_line(note, MAX_NOTE_CHARS))
                                .filter(|note| !note.is_empty()),
                        },
                    }),
                    Err(reason) => skipped.push(SkippedDisplay {
                        path: highlight.path,
                        reason: reason.for_agent,
                    }),
                }
            }
            let shown = pending.len();
            assist.update(cx, |assist, cx| {
                assist.replace_displays(&source, RequestKind::Highlights, request, pending, cx)
            });
            Ok(DisplayOutcome { shown, skipped })
        })
    }

    fn show_annotations(
        &self,
        project: &Entity<Project>,
        source: AssistSource,
        annotations: Vec<CodeAnnotation>,
        cx: &mut App,
    ) -> Task<Result<DisplayOutcome>> {
        if annotations.len() > MAX_ANNOTATIONS {
            return Task::ready(Err(anyhow!(
                "At most {MAX_ANNOTATIONS} annotations can be shown at once ({} were given)",
                annotations.len()
            )));
        }
        let request = next_request();
        let project = project.clone();
        cx.spawn(async move |cx| {
            let assist = cx.update(|cx| assist_for_project(&project, cx))?;
            let targets = resolve_targets(
                &project,
                annotations
                    .iter()
                    .map(|annotation| (annotation.path.clone(), annotation.lines))
                    .collect(),
                cx,
            )
            .await;
            let mut pending = Vec::new();
            let mut skipped = Vec::new();
            for (annotation, target) in annotations.into_iter().zip(targets) {
                match target {
                    Ok(target) => pending.push(PendingDisplay {
                        path: annotation.path,
                        target,
                        content: PendingContent::Annotation {
                            markdown: DisplayMarkdown::new(&annotation.markdown),
                        },
                    }),
                    Err(reason) => skipped.push(SkippedDisplay {
                        path: annotation.path,
                        reason: reason.for_agent,
                    }),
                }
            }
            let shown = pending.len();
            assist.update(cx, |assist, cx| {
                assist.replace_displays(&source, RequestKind::Annotations, request, pending, cx)
            });
            Ok(DisplayOutcome { shown, skipped })
        })
    }

    fn start_tour(
        &self,
        project: &Entity<Project>,
        source: AssistSource,
        tour: CodeTour,
        cx: &mut App,
    ) -> Task<Result<DisplayOutcome>> {
        if tour.steps.is_empty() {
            return Task::ready(Err(anyhow!("The tour has no steps")));
        }
        if tour.steps.len() > MAX_TOUR_STEPS {
            return Task::ready(Err(anyhow!(
                "A tour can have at most {MAX_TOUR_STEPS} steps ({} were given)",
                tour.steps.len()
            )));
        }
        let request = next_request();
        let project = project.clone();
        cx.spawn(async move |cx| {
            let assist = cx.update(|cx| assist_for_project(&project, cx))?;
            let targets = resolve_targets(
                &project,
                tour.steps
                    .iter()
                    .map(|step| (step.path.clone(), step.lines))
                    .collect(),
                cx,
            )
            .await;
            let mut steps = Vec::new();
            let mut skipped = Vec::new();
            for (step, target) in tour.steps.into_iter().zip(targets) {
                if let Err(reason) = &target {
                    skipped.push(SkippedDisplay {
                        path: step.path.clone(),
                        reason: reason.for_agent.clone(),
                    });
                }
                steps.push(PendingTourStep {
                    path: step.path,
                    title: single_line(&step.title, MAX_TITLE_CHARS),
                    markdown: DisplayMarkdown::new(&step.markdown),
                    target: target.map_err(|reason| reason.for_display),
                });
            }
            let shown = steps.iter().filter(|step| step.target.is_ok()).count();
            // Nothing of the new tour can be shown: keep whatever is on screen instead of
            // replacing it with an empty tour.
            if shown > 0 {
                let title = single_line(&tour.title, MAX_TITLE_CHARS);
                assist.update(cx, |assist, cx| {
                    assist.start_tour(source, title, steps, request, cx)
                });
            }
            Ok(DisplayOutcome { shown, skipped })
        })
    }

    fn clear(&self, project: &Entity<Project>, source: &AssistSource, cx: &mut App) {
        let request = next_request();
        let project = project.clone();
        let source = source.clone();
        // Called from connection teardown, which may run inside the workspace window's update;
        // the windows can only be searched once that is over.
        cx.defer(move |cx| match assist_for_project(&project, cx) {
            Ok(assist) => assist.update(cx, |assist, cx| assist.clear_source(&source, request, cx)),
            Err(error) => log::debug!("ide_assist: nothing to clear: {error:#}"),
        });
    }
}

fn workspace_for_project(project: &Entity<Project>, cx: &App) -> Option<Entity<Workspace>> {
    cx.windows().into_iter().find_map(|window| {
        if let Some(multi_workspace) = window.downcast::<MultiWorkspace>() {
            let multi_workspace = multi_workspace.read(cx).ok()?;
            multi_workspace
                .workspaces()
                .find(|workspace| workspace.read(cx).project() == project)
                .cloned()
        } else {
            let workspace = window.downcast::<Workspace>()?.entity(cx).ok()?;
            (workspace.read(cx).project() == project).then_some(workspace)
        }
    })
}

fn workspace_assist(workspace: &Workspace, cx: &App) -> Option<Entity<WorkspaceAssist>> {
    workspace
        .status_bar()
        .read(cx)
        .item_of_type::<WorkspaceAssist>()
}

fn assist_for_project(project: &Entity<Project>, cx: &App) -> Result<Entity<WorkspaceAssist>> {
    let workspace = workspace_for_project(project, cx).with_context(|| {
        format!(
            "No {} window has this project open",
            branding::CURRENT.product_name
        )
    })?;
    workspace_assist(workspace.read(cx), cx).context("This window can't show the agent's displays")
}

/// Why a target can't be shown. The agent that asked is told in English whatever language
/// the tour card shows it in, so that the UI language does not change what the model reads.
#[derive(Clone, Debug)]
struct Unavailable {
    for_agent: String,
    for_display: SharedString,
}

#[derive(Clone, Debug)]
struct ResolvedTarget {
    buffer: Entity<Buffer>,
    /// Start of the first line to the end of the last one.
    range: Range<language::Anchor>,
    /// Where a highlight's note goes, so that it stays next to the code it talks about.
    first_line_end: language::Anchor,
    /// Clamped to the file, for labels.
    lines: LineRange,
}

async fn resolve_targets(
    project: &Entity<Project>,
    requests: Vec<(ProjectPath, LineRange)>,
    cx: &AsyncApp,
) -> Vec<Result<ResolvedTarget, Unavailable>> {
    futures::future::join_all(requests.into_iter().map(|(path, lines)| {
        let project = project.clone();
        let mut cx = cx.clone();
        async move { resolve_target(&project, &path, lines, &mut cx).await }
    }))
    .await
}

async fn resolve_target(
    project: &Entity<Project>,
    path: &ProjectPath,
    lines: LineRange,
    cx: &mut AsyncApp,
) -> Result<ResolvedTarget, Unavailable> {
    if lines.start_line == 0 || lines.end_line < lines.start_line {
        return Err(Unavailable {
            for_agent: format!(
                "Invalid line range (lines {}-{})",
                lines.start_line, lines.end_line
            ),
            for_display: i18n::t!(
                "Invalid line range (lines {}-{})",
                lines.start_line,
                lines.end_line
            )
            .into(),
        });
    }
    cx.update(|cx| check_entry(project, path, cx))?;
    let buffer = project
        .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
        .await
        .map_err(|error| {
            let error = format!("{error:#}");
            Unavailable {
                for_agent: format!("Couldn't open the file ({error})"),
                for_display: i18n::t!("Couldn't open the file ({error})").into(),
            }
        })?;
    let (range, first_line_end, lines) =
        buffer.read_with(cx, |buffer, _| line_anchors(buffer, lines))?;
    Ok(ResolvedTarget {
        buffer,
        range,
        first_line_end,
        lines,
    })
}

fn check_entry(project: &Entity<Project>, path: &ProjectPath, cx: &App) -> Result<(), Unavailable> {
    let unavailable = |for_agent: &str, for_display: &'static str| Unavailable {
        for_agent: for_agent.to_string(),
        for_display: for_display.into(),
    };
    let Some(entry) = project.read(cx).entry_for_path(path, cx) else {
        return Err(unavailable("File not found", i18n::t!("File not found")));
    };
    if entry.is_dir() {
        return Err(unavailable(
            "Folders can't be shown",
            i18n::t!("Folders can't be shown"),
        ));
    }
    if entry.is_fifo {
        return Err(unavailable(
            "Not a regular file",
            i18n::t!("Not a regular file"),
        ));
    }
    if entry.size > MAX_DISPLAY_FILE_BYTES {
        return Err(unavailable(
            "The file is too large (over 4 MiB)",
            i18n::t!("The file is too large (over 4 MiB)"),
        ));
    }
    Ok(())
}

fn line_anchors(
    buffer: &Buffer,
    lines: LineRange,
) -> Result<(Range<language::Anchor>, language::Anchor, LineRange), Unavailable> {
    let line_count = buffer.max_point().row + 1;
    if lines.start_line > line_count {
        let start_line = lines.start_line;
        return Err(Unavailable {
            for_agent: format!(
                "Line {start_line} is past the end of the file ({line_count} lines)"
            ),
            for_display: i18n::t!(
                "Line {start_line} is past the end of the file ({line_count} lines)"
            )
            .into(),
        });
    }
    let start_row = lines.start_line - 1;
    let end_row = lines.end_line.min(line_count) - 1;
    let start = Point::new(start_row, 0);
    let end = Point::new(end_row, buffer.line_len(end_row));
    // Right-biased start / left-biased end: a line inserted just above or just below the range
    // stays outside it. An empty range keeps one bias so that its ends can't cross.
    let range = if start == end {
        buffer.anchor_before(start)..buffer.anchor_before(end)
    } else {
        buffer.anchor_after(start)..buffer.anchor_before(end)
    };
    let first_line_end = buffer.anchor_before(Point::new(start_row, buffer.line_len(start_row)));
    Ok((
        range,
        first_line_end,
        LineRange {
            start_line: lines.start_line,
            end_line: end_row + 1,
        },
    ))
}

/// Agent-written Markdown, made safe to render: no images, links or HTML, and at most
/// [`MAX_DISPLAY_MARKDOWN_CHARS`] characters.
#[derive(Clone, Debug, PartialEq)]
struct DisplayMarkdown {
    source: String,
    truncated: bool,
}

impl DisplayMarkdown {
    fn new(markdown: &str) -> Self {
        let (kept, truncated) = match markdown.char_indices().nth(MAX_DISPLAY_MARKDOWN_CHARS) {
            Some((byte_index, _)) => (markdown.get(..byte_index).unwrap_or_default(), true),
            None => (markdown, false),
        };
        Self {
            source: sanitize_markdown(kept),
            truncated,
        }
    }
}

/// Rewrites links and images to their text and HTML to escaped text, using the same parser the
/// renderer uses so that what it would have made clickable or loaded is exactly what goes. Bare
/// URLs are still linkified by the renderer; [`assist_markdown_style`] keeps those inert.
///
/// Removing a link can join the text around it into new syntax (`[x [a](b) y](c)` becomes the
/// link `[x a y](c)`), so the result is parsed again and, if anything that must not render
/// survived, the agent's text is shown as plain text instead.
fn sanitize_markdown(source: &str) -> String {
    let sanitized = strip_markdown(source);
    if renders_inert(&sanitized) {
        sanitized
    } else {
        Markdown::escape(source).into_owned()
    }
}

/// Every level of nesting is an element the renderer lays out recursively; a few thousand `>`
/// overflow the main thread's stack.
const MAX_MARKDOWN_NESTING: usize = 32;

fn renders_inert(source: &str) -> bool {
    let events = parse_markdown_events(source);
    let mut depth = 0;
    for (index, (range, event)) in events.iter().enumerate() {
        match event {
            MarkdownEvent::Start(MarkdownTag::Image { .. })
            | MarkdownEvent::Html
            | MarkdownEvent::InlineHtml
            | MarkdownEvent::InlineHtmlStyleStart(_) => return false,
            // What is left are the bare URLs the parser linkifies; they show their destination.
            MarkdownEvent::Start(MarkdownTag::Link { dest_url, .. })
                if text_within(events.get(index + 1..).unwrap_or_default(), range, source)
                    != dest_url.as_ref() =>
            {
                return false;
            }
            _ => {}
        }
        match event {
            MarkdownEvent::Start(_) => {
                depth += 1;
                if depth > MAX_MARKDOWN_NESTING {
                    return false;
                }
            }
            MarkdownEvent::End(_) => depth = usize::saturating_sub(depth, 1),
            _ => {}
        }
    }
    true
}

fn strip_markdown(source: &str) -> String {
    let events = parse_markdown_events(source);
    let mut output = String::with_capacity(source.len());
    let mut copied_until = 0;
    for (index, (range, event)) in events.iter().enumerate() {
        if range.start < copied_until {
            continue;
        }
        let replacement = match event {
            MarkdownEvent::Start(MarkdownTag::Link { .. }) => {
                text_within(events.get(index + 1..).unwrap_or_default(), range, source)
            }
            MarkdownEvent::Start(MarkdownTag::Image { .. }) => {
                let alt = text_within(events.get(index + 1..).unwrap_or_default(), range, source);
                if alt.trim().is_empty() {
                    i18n::t!("(image)").to_string()
                } else {
                    i18n::t!("(image: {alt})")
                }
            }
            MarkdownEvent::Html | MarkdownEvent::InlineHtml => {
                source.get(range.clone()).unwrap_or_default().to_string()
            }
            _ => continue,
        };
        output.push_str(source.get(copied_until..range.start).unwrap_or_default());
        output.push_str(&Markdown::escape(&replacement));
        copied_until = range.end;
    }
    output.push_str(source.get(copied_until..).unwrap_or_default());
    output
}

fn text_within(
    events: &[(Range<usize>, MarkdownEvent)],
    outer: &Range<usize>,
    source: &str,
) -> String {
    let mut text = String::new();
    for (range, event) in events {
        if range.start >= outer.end {
            break;
        }
        match event {
            MarkdownEvent::Text | MarkdownEvent::Code => {
                text.push_str(source.get(range.clone()).unwrap_or_default())
            }
            MarkdownEvent::SubstitutedText(substituted)
            | MarkdownEvent::SubstitutedCode(substituted) => text.push_str(substituted),
            MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => text.push(' '),
            _ => {}
        }
    }
    text
}

/// Collapses whitespace (so a title or note can't grow the frame around it) and cuts the text
/// at `max_chars` characters.
fn single_line(text: &str, max_chars: usize) -> SharedString {
    let mut line = String::new();
    for (count, word) in text.split_whitespace().enumerate() {
        if count > 0 {
            line.push(' ');
        }
        line.push_str(word);
    }
    match line.char_indices().nth(max_chars) {
        Some((byte_index, _)) => {
            let mut cut = line.get(..byte_index).unwrap_or_default().to_string();
            cut.push('…');
            cut.into()
        }
        None => line.into(),
    }
}

fn lines_label(lines: LineRange) -> String {
    if lines.start_line == lines.end_line {
        i18n::t!("Line {}", lines.start_line)
    } else {
        i18n::t!("Lines {}-{}", lines.start_line, lines.end_line)
    }
}

fn tour_status_label(current: usize, total: usize) -> String {
    i18n::t!("Tour {}/{}", current + 1, total)
}

/// Links, images and selection stay inert; see [`sanitize_markdown`] for why links still reach
/// the renderer at all.
fn assist_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = MarkdownStyle::themed(MarkdownFont::Editor, window, cx);
    style.prevent_mouse_interaction = true;
    style.link = TextStyleRefinement::default();
    style
}

fn assist_markdown_element(
    markdown: &Entity<Markdown>,
    window: &Window,
    cx: &App,
) -> MarkdownElement {
    MarkdownElement::new(markdown.clone(), assist_markdown_style(window, cx))
        .code_block_renderer(CodeBlockRenderer::Default {
            copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
            wrap_button_visibility: WrapButtonVisibility::Hidden,
            border: false,
        })
        .on_url_click(|_, _, _| {})
}

fn highlight_background(cx: &App) -> Hsla {
    cx.theme().colors().editor_highlighted_line_background
}

fn annotation_background(cx: &App) -> Hsla {
    cx.theme().status().info_background.opacity(0.4)
}

fn tour_step_background(cx: &App) -> Hsla {
    cx.theme().status().info_background
}

struct AssistHighlightRows;
struct AssistAnnotationRows;
struct TourStepRows;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum RequestKind {
    Highlights,
    Annotations,
    Tour,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RevealMode {
    /// Asked for by an agent: never takes focus and stays away while a modal is open.
    Agent,
    /// Asked for by the user (tour navigation): focuses the editor and moves the cursor there.
    User,
}

struct PendingDisplay {
    path: ProjectPath,
    target: ResolvedTarget,
    content: PendingContent,
}

enum PendingContent {
    Highlight { note: Option<SharedString> },
    Annotation { markdown: DisplayMarkdown },
}

struct Display {
    source: AssistSource,
    target: ResolvedTarget,
    content: DisplayContent,
}

#[derive(Clone)]
enum DisplayContent {
    Highlight {
        note: Option<SharedString>,
    },
    Annotation {
        markdown: Entity<Markdown>,
        truncated: bool,
    },
}

impl Display {
    fn kind(&self) -> RequestKind {
        match self.content {
            DisplayContent::Highlight { .. } => RequestKind::Highlights,
            DisplayContent::Annotation { .. } => RequestKind::Annotations,
        }
    }
}

struct PendingTourStep {
    path: ProjectPath,
    title: SharedString,
    markdown: DisplayMarkdown,
    target: Result<ResolvedTarget, SharedString>,
}

struct TourStepState {
    path: ProjectPath,
    title: SharedString,
    markdown: Entity<Markdown>,
    truncated: bool,
    target: Result<ResolvedTarget, SharedString>,
}

struct ActiveTour {
    source: AssistSource,
    title: SharedString,
    steps: Vec<TourStepState>,
    /// The step the user asked for (the status bar shows it at once).
    current: usize,
    /// Where the tour's card is and which step it shows; lags `current` while a file opens.
    shown: Option<ShownStep>,
    /// Drops the result of a navigation that a later one overtook.
    generation: usize,
}

struct ShownStep {
    editor: WeakEntity<Editor>,
    /// Where the card sits. For a step that can't be opened, this stays at the last step that
    /// could, and the card says why.
    target: ResolvedTarget,
    step: usize,
    unavailable: Option<SharedString>,
}

struct AppliedEditor {
    editor: WeakEntity<Editor>,
    block_ids: HashSet<CustomBlockId>,
}

/// Everything an agent shows in one workspace. Lives in the workspace's status bar (it shows
/// "ツアー 2/10" while a tour runs), so it is dropped together with the workspace.
pub struct WorkspaceAssist {
    workspace: WeakEntity<Workspace>,
    languages: Arc<LanguageRegistry>,
    displays: Vec<Display>,
    tour: Option<ActiveTour>,
    applied: HashMap<EntityId, AppliedEditor>,
    latest_requests: HashMap<(SharedString, RequestKind), u64>,
    pending_sync: Option<Task<()>>,
    navigation: Option<Task<()>>,
    agent_reveal: Option<Task<()>>,
    _workspace_subscription: Subscription,
}

impl WorkspaceAssist {
    fn new(
        workspace: &Entity<Workspace>,
        languages: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(workspace, |this, _, event, cx| {
            if let workspace::Event::ItemAdded { item } = event
                && let Some(editor) = item.downcast::<Editor>()
                && this.has_anything_to_show()
            {
                this.sync_editor(&editor, cx);
            }
        });
        Self {
            workspace: workspace.downgrade(),
            languages,
            displays: Vec::new(),
            tour: None,
            applied: HashMap::default(),
            latest_requests: HashMap::default(),
            pending_sync: None,
            navigation: None,
            agent_reveal: None,
            _workspace_subscription: subscription,
        }
    }

    fn has_anything_to_show(&self) -> bool {
        !self.displays.is_empty() || self.tour.as_ref().is_some_and(|tour| tour.shown.is_some())
    }

    fn accept_request(&mut self, source: &AssistSource, kind: RequestKind, request: u64) -> bool {
        let latest = self
            .latest_requests
            .entry((source.id.clone(), kind))
            .or_insert(0);
        if *latest > request {
            return false;
        }
        *latest = request;
        true
    }

    fn new_markdown(&self, markdown: &DisplayMarkdown, cx: &mut Context<Self>) -> Entity<Markdown> {
        let source = SharedString::from(markdown.source.clone());
        let languages = self.languages.clone();
        cx.new(|cx| {
            Markdown::new_with_options(
                source,
                Some(languages),
                None,
                // `sanitize_markdown` reads the text with `parse_markdown_events`, which takes a
                // leading `---` block as front matter. Without this the renderer would parse
                // that block as Markdown and show the images and links in it.
                MarkdownOptions {
                    render_metadata_blocks: true,
                    ..MarkdownOptions::default()
                },
                cx,
            )
        })
    }

    fn replace_displays(
        &mut self,
        source: &AssistSource,
        kind: RequestKind,
        request: u64,
        pending: Vec<PendingDisplay>,
        cx: &mut Context<Self>,
    ) {
        if !self.accept_request(source, kind, request) {
            return;
        }
        self.displays
            .retain(|display| !(display.source.id == source.id && display.kind() == kind));
        let first = pending
            .first()
            .map(|display| (display.path.clone(), display.target.clone()));
        for display in pending {
            let content = match display.content {
                PendingContent::Highlight { note } => DisplayContent::Highlight { note },
                PendingContent::Annotation { markdown } => DisplayContent::Annotation {
                    markdown: self.new_markdown(&markdown, cx),
                    truncated: markdown.truncated,
                },
            };
            self.displays.push(Display {
                source: source.clone(),
                target: display.target,
                content,
            });
        }
        self.sync_all(cx);
        if let Some((path, target)) = first {
            let reveal = self.reveal(path, target.buffer.clone(), RevealMode::Agent, cx);
            self.agent_reveal = Some(cx.spawn(async move |_, cx| {
                if let Some(editor) = reveal.await.log_err().flatten() {
                    cx.update(|cx| scroll_to_target(&editor, &target, RevealMode::Agent, cx));
                }
            }));
        }
    }

    fn start_tour(
        &mut self,
        source: AssistSource,
        title: SharedString,
        steps: Vec<PendingTourStep>,
        request: u64,
        cx: &mut Context<Self>,
    ) {
        if !self.accept_request(&source, RequestKind::Tour, request) {
            return;
        }
        let Some(first) = steps.iter().position(|step| step.target.is_ok()) else {
            return;
        };
        let steps = steps
            .into_iter()
            .map(|step| TourStepState {
                markdown: self.new_markdown(&step.markdown, cx),
                truncated: step.markdown.truncated,
                path: step.path,
                title: step.title,
                target: step.target,
            })
            .collect();
        self.tour = Some(ActiveTour {
            source,
            title,
            steps,
            current: first,
            shown: None,
            generation: 0,
        });
        self.schedule_sync(cx);
        self.go_to_step(first, RevealMode::Agent, cx);
    }

    fn clear_source(&mut self, source: &AssistSource, request: u64, cx: &mut Context<Self>) {
        for kind in [
            RequestKind::Highlights,
            RequestKind::Annotations,
            RequestKind::Tour,
        ] {
            self.accept_request(source, kind, request);
        }
        self.displays
            .retain(|display| display.source.id != source.id);
        if self
            .tour
            .as_ref()
            .is_some_and(|tour| tour.source.id == source.id)
        {
            self.tour = None;
            self.navigation = None;
            cx.notify();
        }
        self.sync_all(cx);
    }

    fn step_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(tour) = self.tour.as_ref() else {
            return;
        };
        let Some(index) = tour.current.checked_add_signed(delta) else {
            return;
        };
        if index >= tour.steps.len() {
            return;
        }
        self.go_to_step(index, RevealMode::User, cx);
    }

    fn return_to_current_step(&mut self, cx: &mut Context<Self>) {
        if let Some(current) = self.tour.as_ref().map(|tour| tour.current) {
            self.go_to_step(current, RevealMode::User, cx);
        }
    }

    fn end_tour(&mut self, cx: &mut Context<Self>) {
        if self.tour.take().is_some() {
            self.navigation = None;
            cx.notify();
            self.schedule_sync(cx);
        }
    }

    fn go_to_step(&mut self, index: usize, mode: RevealMode, cx: &mut Context<Self>) {
        let Some(tour) = self.tour.as_mut() else {
            return;
        };
        let Some(step) = tour.steps.get(index) else {
            return;
        };
        let path = step.path.clone();
        let target = step.target.clone();
        tour.current = index;
        tour.generation += 1;
        let generation = tour.generation;
        cx.notify();
        match target {
            Err(reason) => {
                if let Some(shown) = tour.shown.as_mut() {
                    shown.step = index;
                    shown.unavailable = Some(reason);
                }
                self.navigation = None;
                self.schedule_sync(cx);
            }
            Ok(target) => {
                let reveal = self.reveal(path, target.buffer.clone(), mode, cx);
                self.navigation = Some(cx.spawn(async move |this, cx| {
                    let result = reveal.await;
                    this.update(cx, |this, cx| {
                        this.finish_navigation(generation, index, target, result, mode, cx)
                    })
                    .log_err();
                }));
            }
        }
    }

    fn finish_navigation(
        &mut self,
        generation: usize,
        index: usize,
        target: ResolvedTarget,
        result: Result<Option<Entity<Editor>>>,
        mode: RevealMode,
        cx: &mut Context<Self>,
    ) {
        let Some(tour) = self.tour.as_mut() else {
            return;
        };
        if tour.generation != generation {
            return;
        }
        let editor = match result {
            Ok(Some(editor)) => {
                tour.shown = Some(ShownStep {
                    editor: editor.downgrade(),
                    target: target.clone(),
                    step: index,
                    unavailable: None,
                });
                Some(editor)
            }
            // A modal was open; the status bar item brings the step back later.
            Ok(None) => None,
            Err(error) => {
                if let Some(shown) = tour.shown.as_mut() {
                    shown.step = index;
                    shown.unavailable = Some(format!("{error:#}").into());
                }
                None
            }
        };
        self.sync_all(cx);
        if let Some(editor) = editor {
            scroll_to_target(&editor, &target, mode, cx);
        }
    }

    fn dismiss_in_editor(&mut self, editor: &WeakEntity<Editor>, cx: &mut Context<Self>) {
        if let Some(buffer) = editor
            .upgrade()
            .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton())
        {
            self.displays
                .retain(|display| display.target.buffer != buffer);
        }
        let hosts_tour = self.tour.as_ref().is_some_and(|tour| {
            tour.shown
                .as_ref()
                .is_some_and(|shown| shown.editor.entity_id() == editor.entity_id())
        });
        if hosts_tour {
            self.tour = None;
            self.navigation = None;
            cx.notify();
        }
        self.sync_all(cx);
    }

    /// Opens (or brings forward) the editor for `buffer`, for an agent the way the IDE server's
    /// `openFile` does with `makeFrontmost: false` (a preview tab, no focus). `Ok(None)` when an
    /// agent asked while a modal was open.
    fn reveal(
        &self,
        path: ProjectPath,
        buffer: Entity<Buffer>,
        mode: RevealMode,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Entity<Editor>>>> {
        let workspace = self.workspace.clone();
        cx.spawn(async move |_, cx| {
            let workspace = workspace.upgrade().context("The workspace was closed")?;
            let open = cx
                .update(|cx| {
                    cx.with_window(workspace.entity_id(), |window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            open_or_activate(workspace, path, &buffer, mode, window, cx)
                        })
                    })
                })
                .context("The workspace's window wasn't found")?;
            match open {
                Some(open) => Ok(Some(open.await?)),
                None => Ok(None),
            }
        })
    }

    fn schedule_sync(&mut self, cx: &mut Context<Self>) {
        if self.pending_sync.is_some() {
            return;
        }
        self.pending_sync = Some(cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.pending_sync = None;
                this.sync_all(cx);
            })
            .log_err();
        }));
    }

    /// Brings every editor that shows, or showed, something of ours up to date. Must not run
    /// while the workspace or one of its editors is being updated.
    fn sync_all(&mut self, cx: &mut Context<Self>) {
        let mut editors = self
            .workspace
            .upgrade()
            .map(|workspace| {
                workspace
                    .read(cx)
                    .items_of_type::<Editor>(cx)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.applied
            .retain(|_, applied| applied.editor.upgrade().is_some());
        // Tabs moved to another window leave this workspace but still carry our blocks.
        let elsewhere = self
            .applied
            .values()
            .filter_map(|applied| applied.editor.upgrade())
            .chain(
                self.tour
                    .as_ref()
                    .and_then(|tour| tour.shown.as_ref())
                    .and_then(|shown| shown.editor.upgrade()),
            )
            .collect::<Vec<_>>();
        for editor in elsewhere {
            if !editors.contains(&editor) {
                editors.push(editor);
            }
        }
        for editor in editors {
            self.sync_editor(&editor, cx);
        }
    }

    fn sync_editor(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let editor_id = editor.entity_id();
        let buffer = editor.read(cx).buffer().read(cx).as_singleton();
        let displays = match &buffer {
            Some(buffer) => self
                .displays
                .iter()
                .filter(|display| display.target.buffer == *buffer)
                .map(|display| EditorDisplay {
                    label: display.source.label.clone(),
                    target: display.target.clone(),
                    content: display.content.clone(),
                })
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        let card = self.tour_card_for(editor_id, cx);
        let previous = self.applied.remove(&editor_id);
        if displays.is_empty() && card.is_none() && previous.is_none() {
            return;
        }
        let shows_something = !displays.is_empty() || card.is_some();
        let previous_blocks = previous
            .map(|previous| previous.block_ids)
            .unwrap_or_default();
        let assist = cx.weak_entity();
        let block_ids = editor.update(cx, |editor, cx| {
            apply_to_editor(editor, previous_blocks, displays, card, assist, cx)
        });
        if shows_something {
            self.applied.insert(
                editor_id,
                AppliedEditor {
                    editor: editor.downgrade(),
                    block_ids,
                },
            );
        }
    }

    fn tour_card_for(&self, editor_id: EntityId, cx: &Context<Self>) -> Option<TourCard> {
        let tour = self.tour.as_ref()?;
        let shown = tour.shown.as_ref()?;
        if shown.editor.entity_id() != editor_id {
            return None;
        }
        let step = tour.steps.get(shown.step)?;
        let body = match (&shown.unavailable, &step.target) {
            (Some(reason), _) | (None, Err(reason)) => TourCardBody::Unavailable(reason.clone()),
            (None, Ok(_)) => TourCardBody::Markdown {
                markdown: step.markdown.clone(),
                truncated: step.truncated,
            },
        };
        Some(TourCard {
            assist: cx.weak_entity(),
            source_label: tour.source.label.clone(),
            tour_title: tour.title.clone(),
            step_title: step.title.clone(),
            body,
            step: shown.step,
            total: tour.steps.len(),
            target: shown.target.clone(),
        })
    }
}

fn open_or_activate(
    workspace: &mut Workspace,
    path: ProjectPath,
    buffer: &Entity<Buffer>,
    mode: RevealMode,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Task<Result<Entity<Editor>>>> {
    if mode == RevealMode::Agent && workspace.has_active_modal(window, cx) {
        return None;
    }
    let focus = mode == RevealMode::User;
    // Switching the visible tab of the pane the user is typing in would move their keyboard
    // focus to it (the pane refocuses its active item), so an agent only adds a tab there.
    let user_works_in = |pane: &Entity<Pane>, window: &Window, cx: &App| {
        mode == RevealMode::Agent && pane.read(cx).focus_handle(cx).contains_focused(window, cx)
    };
    let existing = workspace.panes().iter().find_map(|pane| {
        pane.read(cx)
            .items_of_type::<Editor>()
            .find(|editor| {
                editor.read(cx).buffer().read(cx).as_singleton().as_ref() == Some(buffer)
            })
            .map(|editor| (pane.clone(), editor))
    });
    if let Some((pane, editor)) = existing {
        if !user_works_in(&pane, window, cx) {
            workspace.activate_item(&editor, false, focus, window, cx);
        }
        return Some(Task::ready(Ok(editor)));
    }
    let pane = workspace.active_pane().clone();
    let in_front = !user_works_in(&pane, window, cx);
    let open = workspace.open_path_preview(
        path,
        Some(pane.downgrade()),
        focus,
        in_front,
        in_front,
        window,
        cx,
    );
    Some(cx.spawn(async move |_, _| {
        let item = open.await?;
        item.downcast::<Editor>()
            .context("This file can't be opened in a text editor")
    }))
}

fn scroll_to_target(
    editor: &Entity<Editor>,
    target: &ResolvedTarget,
    mode: RevealMode,
    cx: &mut App,
) {
    let start = editor.update(cx, |editor, cx| {
        editor
            .buffer()
            .read(cx)
            .snapshot(cx)
            .anchor_in_buffer(target.range.start)
    });
    let Some(start) = start else {
        return;
    };
    match mode {
        RevealMode::User => {
            let moved = cx.with_window(editor.entity_id(), |window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_anchor_ranges([start..start]),
                    );
                })
            });
            if moved.is_some() {
                return;
            }
        }
        RevealMode::Agent => {
            // Scrolling the editor the user is typing in would pull the text away from under
            // their cursor.
            let user_types_here = cx
                .with_window(editor.entity_id(), |window, cx| {
                    editor.focus_handle(cx).contains_focused(window, cx)
                })
                .unwrap_or(false);
            if user_types_here {
                return;
            }
        }
    }
    editor.update(cx, |editor, cx| {
        editor.request_autoscroll(Autoscroll::center().for_anchor(start), cx);
    });
}

struct EditorDisplay {
    label: SharedString,
    target: ResolvedTarget,
    content: DisplayContent,
}

#[derive(Clone)]
enum TourCardBody {
    Markdown {
        markdown: Entity<Markdown>,
        truncated: bool,
    },
    Unavailable(SharedString),
}

#[derive(Clone)]
struct TourCard {
    assist: WeakEntity<WorkspaceAssist>,
    source_label: SharedString,
    tour_title: SharedString,
    step_title: SharedString,
    body: TourCardBody,
    step: usize,
    total: usize,
    target: ResolvedTarget,
}

fn apply_to_editor(
    editor: &mut Editor,
    previous_blocks: HashSet<CustomBlockId>,
    displays: Vec<EditorDisplay>,
    card: Option<TourCard>,
    assist: WeakEntity<WorkspaceAssist>,
    cx: &mut Context<Editor>,
) -> HashSet<CustomBlockId> {
    if !previous_blocks.is_empty() {
        editor.remove_blocks(previous_blocks, None, cx);
    }
    editor.clear_row_highlights::<AssistHighlightRows>();
    editor.clear_row_highlights::<AssistAnnotationRows>();
    editor.clear_row_highlights::<TourStepRows>();
    cx.notify();
    if displays.is_empty() && card.is_none() {
        if editor
            .addon::<AssistAddon>()
            .is_some_and(|addon| addon.assist.entity_id() == assist.entity_id())
        {
            editor.unregister_addon::<AssistAddon>();
        }
        return HashSet::default();
    }

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let mut blocks = Vec::new();
    for display in displays {
        let Some(range) = snapshot.anchor_range_in_buffer(display.target.range.clone()) else {
            continue;
        };
        let label = display.label;
        match display.content {
            DisplayContent::Highlight { note } => {
                editor.highlight_rows::<AssistHighlightRows>(
                    range,
                    highlight_background,
                    RowHighlightOptions::default(),
                    cx,
                );
                if let Some(note) = note
                    && let Some(anchor) = snapshot.anchor_in_buffer(display.target.first_line_end)
                {
                    blocks.push(BlockProperties {
                        placement: BlockPlacement::Near(anchor),
                        height: Some(1),
                        style: BlockStyle::Flex,
                        render: Arc::new(move |cx| render_note(&label, &note, cx)),
                        priority: 0,
                    });
                }
            }
            DisplayContent::Annotation {
                markdown,
                truncated,
            } => {
                editor.highlight_rows::<AssistAnnotationRows>(
                    range.clone(),
                    annotation_background,
                    RowHighlightOptions::default(),
                    cx,
                );
                let lines = display.target.lines;
                let height = estimated_height(&markdown, cx);
                blocks.push(BlockProperties {
                    placement: BlockPlacement::Below(range.end),
                    height: Some(height),
                    style: BlockStyle::Sticky,
                    render: Arc::new(move |cx| {
                        render_annotation(&label, lines, &markdown, truncated, cx)
                    }),
                    priority: 0,
                });
            }
        }
    }

    let hosts_tour = card.is_some();
    if let Some(card) = card
        && let Some(range) = snapshot.anchor_range_in_buffer(card.target.range.clone())
    {
        if matches!(card.body, TourCardBody::Markdown { .. }) {
            editor.highlight_rows::<TourStepRows>(
                range.clone(),
                tour_step_background,
                RowHighlightOptions::default(),
                cx,
            );
        }
        let height = match &card.body {
            TourCardBody::Markdown { markdown, .. } => estimated_height(markdown, cx) + 2,
            TourCardBody::Unavailable(_) => 4,
        };
        blocks.push(BlockProperties {
            placement: BlockPlacement::Below(range.end),
            height: Some(height),
            style: BlockStyle::Sticky,
            render: Arc::new(move |cx| render_tour_card(&card, cx)),
            priority: 1,
        });
    }

    editor.register_addon(AssistAddon {
        assist,
        editor: cx.weak_entity(),
        hosts_tour,
    });
    editor.insert_blocks(blocks, None, cx).into_iter().collect()
}

/// A first guess; the editor resizes the block to what it renders to.
fn estimated_height(markdown: &Entity<Markdown>, cx: &App) -> u32 {
    let lines = markdown.read(cx).source().lines().count() as u32;
    lines.clamp(1, 40) + 1
}

struct AssistAddon {
    assist: WeakEntity<WorkspaceAssist>,
    editor: WeakEntity<Editor>,
    hosts_tour: bool,
}

impl Addon for AssistAddon {
    fn extend_key_context(&self, context: &mut KeyContext, _: &App) {
        if self.hosts_tour {
            context.add(TOUR_KEY_CONTEXT);
        }
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn dismiss_on_escape(&self, _window: &mut Window, cx: &mut App) -> bool {
        let assist = self.assist.clone();
        let editor = self.editor.clone();
        cx.defer(move |cx| {
            if let Some(assist) = assist.upgrade() {
                assist.update(cx, |assist, cx| assist.dismiss_in_editor(&editor, cx));
            }
        });
        true
    }
}

fn frame_header(icon: IconName, title: String) -> impl IntoElement {
    h_flex()
        .gap_1()
        .min_w_0()
        .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Accent))
        .child(
            Label::new(title)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
        )
}

fn truncation_notice() -> impl IntoElement {
    Label::new(i18n::t!(
        "Too long, cut to {max_chars} characters",
        max_chars = MAX_DISPLAY_MARKDOWN_CHARS
    ))
    .size(LabelSize::XSmall)
    .color(Color::Muted)
}

fn render_note(label: &SharedString, note: &SharedString, cx: &mut BlockContext) -> AnyElement {
    let colors = cx.app.theme().colors();
    h_flex()
        .block_mouse_except_scroll()
        .gap_1()
        .px_1()
        .rounded_sm()
        .bg(colors.surface_background)
        .child(
            Icon::new(IconName::Sparkle)
                .size(IconSize::XSmall)
                .color(Color::Accent),
        )
        .child(
            Label::new(format!("{label}: {note}"))
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .into_any_element()
}

fn render_annotation(
    label: &SharedString,
    lines: LineRange,
    markdown: &Entity<Markdown>,
    truncated: bool,
    cx: &mut BlockContext,
) -> AnyElement {
    let colors = cx.app.theme().colors();
    let border = cx.app.theme().status().info_border;
    let max_width = cx.em_width * 100.;
    let element = assist_markdown_element(markdown, cx.window, cx.app);
    div()
        .w_full()
        .pl(cx.margins.gutter.full_width())
        .pr_4()
        .py_0p5()
        .child(
            v_flex()
                .max_w(max_width)
                .block_mouse_except_scroll()
                .gap_1()
                .px_2()
                .py_1()
                .border_l_2()
                .border_color(border)
                .rounded_r_md()
                .bg(colors.surface_background)
                .child(
                    h_flex()
                        .gap_2()
                        .child(frame_header(
                            IconName::Sparkle,
                            i18n::t!("Annotation from {label}"),
                        ))
                        .child(
                            Label::new(lines_label(lines))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    div()
                        .debug_selector(|| "ide-assist-annotation-markdown".into())
                        .child(element),
                )
                .when(truncated, |this| this.child(truncation_notice())),
        )
        .into_any_element()
}

fn render_tour_card(card: &TourCard, cx: &mut BlockContext) -> AnyElement {
    let colors = cx.app.theme().colors();
    let border = cx.app.theme().status().info_border;
    let max_width = cx.em_width * 100.;
    let body = match &card.body {
        TourCardBody::Markdown {
            markdown,
            truncated,
        } => v_flex()
            .child(assist_markdown_element(markdown, cx.window, cx.app))
            .when(*truncated, |this| this.child(truncation_notice()))
            .into_any_element(),
        TourCardBody::Unavailable(reason) => {
            Label::new(i18n::t!("Can't open this step ({reason})"))
                .size(LabelSize::Small)
                .color(Color::Warning)
                .into_any_element()
        }
    };
    let step_button = |id: &'static str, label: &'static str, delta: isize, disabled: bool| {
        let assist = card.assist.clone();
        Button::new(id, label)
            .label_size(LabelSize::Small)
            .disabled(disabled)
            .on_click(move |_, _, cx| {
                if let Some(assist) = assist.upgrade() {
                    assist.update(cx, |assist, cx| assist.step_by(delta, cx));
                }
            })
    };
    let end_button = {
        let assist = card.assist.clone();
        Button::new("ide-assist-tour-end", i18n::tc!("tour", "End"))
            .label_size(LabelSize::Small)
            .tooltip(Tooltip::for_action_title(i18n::t!("End Tour"), &EndTour))
            .on_click(move |_, _, cx| {
                if let Some(assist) = assist.upgrade() {
                    assist.update(cx, |assist, cx| assist.end_tour(cx));
                }
            })
    };
    div()
        .w_full()
        .pl(cx.margins.gutter.full_width())
        .pr_4()
        .py_1()
        .child(
            v_flex()
                .max_w(max_width)
                .block_mouse_except_scroll()
                .gap_1()
                .p_2()
                .border_1()
                .border_color(border)
                .rounded_md()
                .bg(colors.surface_background)
                .child(frame_header(
                    IconName::Sparkle,
                    i18n::t!("Tour from {}: {}", card.source_label, card.tour_title),
                ))
                .child(Label::new(card.step_title.clone()).weight(FontWeight::SEMIBOLD))
                .child(body)
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .debug_selector(|| "ide-assist-tour-previous".into())
                                .child(
                                    step_button(
                                        "ide-assist-tour-previous",
                                        i18n::t!("‹ Previous"),
                                        -1,
                                        card.step == 0,
                                    )
                                    .tooltip(
                                        Tooltip::for_action_title(
                                            i18n::t!("Previous Step"),
                                            &PreviousTourStep,
                                        ),
                                    ),
                                ),
                        )
                        .child(
                            Label::new(format!("{}/{}", card.step + 1, card.total))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            div()
                                .debug_selector(|| "ide-assist-tour-next".into())
                                .child(
                                    step_button(
                                        "ide-assist-tour-next",
                                        i18n::t!("Next ›"),
                                        1,
                                        card.step + 1 >= card.total,
                                    )
                                    .tooltip(
                                        Tooltip::for_action_title(
                                            i18n::t!("Next Step"),
                                            &NextTourStep,
                                        ),
                                    ),
                                ),
                        )
                        .child(div().flex_1())
                        .child(end_button),
                ),
        )
        .into_any_element()
}

impl Render for WorkspaceAssist {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(tour) = self.tour.as_ref() else {
            return div().into_any_element();
        };
        Button::new(
            "ide-assist-tour-status",
            tour_status_label(tour.current, tour.steps.len()),
        )
        .label_size(LabelSize::Small)
        .tooltip(Tooltip::text(i18n::t!(
            "Tour from {}: {} (click to go to the current step)",
            tour.source.label,
            tour.title
        )))
        .on_click(cx.listener(|this, _, _, cx| this.return_to_current_step(cx)))
        .into_any_element()
    }
}

impl StatusItemView for WorkspaceAssist {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    /// Shown only while an agent's tour runs, so there is nothing to hide.
    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod ide_assist_tests;
