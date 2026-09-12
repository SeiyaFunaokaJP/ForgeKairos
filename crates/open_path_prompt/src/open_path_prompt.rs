pub mod file_finder_settings;

#[cfg(test)]
mod open_path_prompt_tests;

use file_finder_settings::FileFinderSettings;
use file_icons::FileIcons;
use futures::channel::oneshot;
use fuzzy::{CharBag, StringMatch, StringMatchCandidate};
use gpui::{HighlightStyle, StyledText, Task};
use picker::{Picker, PickerDelegate};
use project::{DirectoryItem, DirectoryLister};
use settings::SettingsStore;
use settings::{ProjectPanelSortMode, Settings};
use std::{
    path::{self, Path, PathBuf},
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
};
use ui::{Context, Divider, LabelLike, ListItem, Tooltip, Window};
use ui::{HighlightedLabel, ListItemSpacing, prelude::*};
use util::{
    ResultExt, maybe,
    paths::{PathStyle, compare_paths},
};
use workspace::Workspace;

pub struct OpenPathPrompt;

pub struct OpenPathDelegate {
    tx: Option<oneshot::Sender<Option<Vec<PathBuf>>>>,
    lister: DirectoryLister,
    selected_index: usize,
    directory_state: DirectoryState,
    string_matches: Vec<StringMatch>,
    cancel_flag: Arc<AtomicBool>,
    should_dismiss: bool,
    prompt_root: String,
    path_style: PathStyle,
    replace_prompt: Task<()>,
    render_footer:
        Arc<dyn Fn(&mut Window, &mut Context<Picker<Self>>) -> Option<AnyElement> + 'static>,
    hidden_entries: bool,
    sort_mode: ProjectPanelSortMode,
}

impl OpenPathDelegate {
    pub fn new(
        tx: oneshot::Sender<Option<Vec<PathBuf>>>,
        lister: DirectoryLister,
        creating_path: bool,
        cx: &App,
    ) -> Self {
        let path_style = lister.path_style(cx);
        let sort_mode = cx
            .try_global::<SettingsStore>()
            .and_then(|store| store.merged_settings().project_panel.as_ref()?.sort_mode)
            .unwrap_or_default();
        Self {
            tx: Some(tx),
            lister,
            selected_index: 0,
            directory_state: DirectoryState::None {
                create: creating_path,
            },
            string_matches: Vec::new(),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            should_dismiss: true,
            prompt_root: match path_style {
                PathStyle::Unix => "/".to_string(),
                PathStyle::Windows => "C:\\".to_string(),
            },
            path_style,
            replace_prompt: Task::ready(()),
            render_footer: Arc::new(|_, _| None),
            hidden_entries: false,
            sort_mode,
        }
    }

    pub fn with_footer(
        mut self,
        footer: Arc<
            dyn Fn(&mut Window, &mut Context<Picker<Self>>) -> Option<AnyElement> + 'static,
        >,
    ) -> Self {
        self.render_footer = footer;
        self
    }

    pub fn show_hidden(mut self) -> Self {
        self.hidden_entries = true;
        self
    }
    fn get_entry(&self, selected_match_index: usize) -> Option<CandidateInfo> {
        match &self.directory_state {
            DirectoryState::List { entries, .. } => {
                let id = self.string_matches.get(selected_match_index)?.candidate_id;
                entries.iter().find(|entry| entry.path.id == id).cloned()
            }
            DirectoryState::Create {
                user_input,
                entries,
                ..
            } => {
                let mut i = selected_match_index;
                if let Some(user_input) = user_input
                    && (!user_input.exists || !user_input.is_dir)
                {
                    if i == 0 {
                        return Some(CandidateInfo {
                            path: user_input.file.clone(),
                            is_dir: false,
                        });
                    } else {
                        i -= 1;
                    }
                }
                let id = self.string_matches.get(i)?.candidate_id;
                entries.iter().find(|entry| entry.path.id == id).cloned()
            }
            DirectoryState::None { .. } => None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn collect_match_candidates(&self) -> Vec<String> {
        match &self.directory_state {
            DirectoryState::List { entries, .. } => self
                .string_matches
                .iter()
                .filter_map(|string_match| {
                    entries
                        .iter()
                        .find(|entry| entry.path.id == string_match.candidate_id)
                        .map(|candidate| candidate.path.string.clone())
                })
                .collect(),
            DirectoryState::Create {
                user_input,
                entries,
                ..
            } => user_input
                .iter()
                .filter(|user_input| !user_input.exists || !user_input.is_dir)
                .map(|user_input| user_input.file.string.clone())
                .chain(self.string_matches.iter().filter_map(|string_match| {
                    entries
                        .iter()
                        .find(|entry| entry.path.id == string_match.candidate_id)
                        .map(|candidate| candidate.path.string.clone())
                }))
                .collect(),
            DirectoryState::None { .. } => Vec::new(),
        }
    }

    fn current_dir(&self) -> &'static str {
        match self.path_style {
            PathStyle::Unix => "./",
            PathStyle::Windows => ".\\",
        }
    }

    fn parent_dir(&self) -> &'static str {
        match self.path_style {
            PathStyle::Unix => "../",
            PathStyle::Windows => "..\\",
        }
    }

    /// The query that moves the prompt one directory up, or `None` when it is
    /// already at the root (or has nothing listed yet).
    fn parent_query(&self) -> Option<String> {
        let parent_path = match &self.directory_state {
            DirectoryState::List { parent_path, .. }
            | DirectoryState::Create { parent_path, .. } => parent_path,
            DirectoryState::None { .. } => return None,
        };
        parent_directory_query(parent_path, self.path_style, &self.prompt_root)
    }

    fn is_parent_dir_entry(&self, candidate: &CandidateInfo) -> bool {
        candidate.is_dir && candidate.path.string == self.parent_dir()
    }

    fn is_current_dir_entry(&self, candidate: &CandidateInfo) -> bool {
        candidate.is_dir && candidate.path.string == self.current_dir()
    }

    /// The query that lists `candidate`'s own contents: the directory currently
    /// being listed, plus the candidate's name, plus a trailing separator.
    fn child_query(&self, candidate: &CandidateInfo) -> Option<String> {
        match &self.directory_state {
            DirectoryState::List { parent_path, .. }
            | DirectoryState::Create { parent_path, .. } => Some(format!(
                "{}{}{}",
                parent_path,
                candidate.path.string,
                self.path_style.primary_separator()
            )),
            DirectoryState::None { .. } => None,
        }
    }

    /// Whether confirming `candidate` should browse into it rather than pick it.
    ///
    /// A folder is picked with the `./` row, the footer button or
    /// `menu::SecondaryConfirm`; plain confirm walks into it, because that is
    /// what a file browser does and because otherwise a directory that has the
    /// wanted folder inside it can only be reached by typing its name.
    ///
    /// Only the "open an existing path" prompt browses this way. In the "new
    /// path" prompt confirm belongs to the name being typed, not to the
    /// highlighted row.
    fn should_browse_into(&self, candidate: &CandidateInfo) -> bool {
        matches!(self.directory_state, DirectoryState::List { .. })
            && candidate.is_dir
            && !candidate.path.string.is_empty()
            && !self.is_current_dir_entry(candidate)
            && !self.is_parent_dir_entry(candidate)
    }

    /// The directory whose contents are currently listed, as an absolute path.
    ///
    /// This is what the `./` row and the footer button hand back. The listed
    /// path is normalized first: `join`ing `./` onto it the way an ordinary
    /// candidate is joined would make the chosen worktree's path
    /// `/home/ubuntu/./`.
    fn listed_directory_path(&self, cx: &App) -> Option<PathBuf> {
        let parent_path = match &self.directory_state {
            DirectoryState::List { parent_path, .. }
            | DirectoryState::Create { parent_path, .. } => parent_path,
            DirectoryState::None { .. } => return None,
        };
        let resolved = self.lister.resolve_tilde(parent_path, cx);
        Some(PathBuf::from(self.path_style.normalize(resolved.as_ref())))
    }

    /// Picks the directory whose contents are listed, closing the prompt. Shared
    /// by the `./` row and the footer's "このフォルダーを開く" button.
    fn confirm_listed_directory(&mut self, cx: &mut Context<Picker<Self>>) {
        let Some(path) = self.listed_directory_path(cx) else {
            return;
        };
        if let Some(tx) = self.tx.take() {
            tx.send(Some(vec![path])).ok();
        }
        cx.emit(gpui::DismissEvent);
    }
}

#[derive(Debug)]
enum DirectoryState {
    List {
        parent_path: String,
        entries: Vec<CandidateInfo>,
        error: Option<SharedString>,
    },
    Create {
        parent_path: String,
        user_input: Option<UserInput>,
        entries: Vec<CandidateInfo>,
    },
    None {
        create: bool,
    },
}

#[derive(Debug, Clone)]
struct UserInput {
    file: StringMatchCandidate,
    exists: bool,
    is_dir: bool,
}

#[derive(Debug, Clone)]
struct CandidateInfo {
    path: StringMatchCandidate,
    is_dir: bool,
}

impl OpenPathPrompt {
    pub fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.set_prompt_for_open_path(Box::new(|workspace, lister, window, cx| {
            let (tx, rx) = futures::channel::oneshot::channel();
            Self::prompt_for_open_path(workspace, lister, false, None, None, tx, window, cx);
            rx
        }));
    }

    pub fn register_new_path(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.set_prompt_for_new_path(Box::new(
            |workspace, lister, start_directory, suggested_name, window, cx| {
                let (tx, rx) = futures::channel::oneshot::channel();
                Self::prompt_for_new_path(
                    workspace,
                    lister,
                    start_directory,
                    suggested_name,
                    tx,
                    window,
                    cx,
                );
                rx
            },
        ));
    }

    fn prompt_for_open_path(
        workspace: &mut Workspace,
        lister: DirectoryLister,
        creating_path: bool,
        start_directory: Option<PathBuf>,
        suggested_name: Option<String>,
        tx: oneshot::Sender<Option<Vec<PathBuf>>>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        workspace.toggle_modal(window, cx, |window, cx| {
            let delegate =
                OpenPathDelegate::new(tx, lister.clone(), creating_path, cx).show_hidden();
            let picker = Picker::uniform_list(delegate, window, cx);
            let query = initial_query(
                &lister,
                start_directory.as_deref(),
                suggested_name.as_deref(),
                cx,
            );
            picker.set_query(&query, window, cx);
            picker
        });
    }

    fn prompt_for_new_path(
        workspace: &mut Workspace,
        lister: DirectoryLister,
        start_directory: Option<PathBuf>,
        suggested_name: Option<String>,
        tx: oneshot::Sender<Option<Vec<PathBuf>>>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::prompt_for_open_path(
            workspace,
            lister,
            true,
            start_directory,
            suggested_name,
            tx,
            window,
            cx,
        );
    }
}

fn initial_query(
    lister: &DirectoryLister,
    start_directory: Option<&Path>,
    suggested_name: Option<&str>,
    cx: &mut App,
) -> String {
    let mut query = match start_directory {
        Some(start_directory) => {
            let separator = lister.path_style(cx).primary_separator();
            let mut query = start_directory.to_string_lossy().into_owned();
            if !query.ends_with(separator) {
                query.push_str(separator);
            }
            query
        }
        None => lister.default_query(cx),
    };
    if let Some(suggested_name) = suggested_name {
        query.push_str(suggested_name);
    }
    query
}

impl PickerDelegate for OpenPathDelegate {
    type ListItem = ui::ListItem;

    fn name() -> &'static str {
        "open path prompt"
    }

    fn match_count(&self) -> usize {
        let user_input = if let DirectoryState::Create { user_input, .. } = &self.directory_state {
            user_input
                .as_ref()
                .filter(|input| !input.exists || !input.is_dir)
                .into_iter()
                .count()
        } else {
            0
        };
        self.string_matches.len() + user_input
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
        cx.notify();
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let lister = &self.lister;
        let input_is_empty = query.is_empty();
        let (dir, suffix) = get_dir_and_suffix(query, self.path_style);

        let query = match &self.directory_state {
            DirectoryState::List { parent_path, .. } => {
                if parent_path == &dir {
                    None
                } else {
                    Some(lister.list_directory(dir.clone(), cx))
                }
            }
            DirectoryState::Create {
                parent_path,
                user_input,
                ..
            } => {
                if parent_path == &dir
                    && user_input.as_ref().map(|input| &input.file.string) == Some(&suffix)
                {
                    None
                } else {
                    Some(lister.list_directory(dir.clone(), cx))
                }
            }
            DirectoryState::None { .. } => Some(lister.list_directory(dir.clone(), cx)),
        };
        self.cancel_flag.store(true, atomic::Ordering::Release);
        self.cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_flag = self.cancel_flag.clone();
        let hidden_entries = self.hidden_entries;
        let parent_path_is_root = self.prompt_root == dir;
        let has_parent_target =
            parent_directory_query(&dir, self.path_style, &self.prompt_root).is_some();
        let current_dir = self.current_dir();
        let parent_dir = self.parent_dir();
        let sort_mode = self.sort_mode;
        cx.spawn_in(window, async move |this, cx| {
            if let Some(query) = query {
                let paths = query.await;
                if cancel_flag.load(atomic::Ordering::Acquire) {
                    return;
                }

                if this
                    .update(cx, |this, _| {
                        let new_state = match &this.delegate.directory_state {
                            DirectoryState::None { create: false }
                            | DirectoryState::List { .. } => match paths {
                                Ok(paths) => DirectoryState::List {
                                    entries: path_candidates(parent_path_is_root, paths, sort_mode),
                                    parent_path: dir.clone(),
                                    error: None,
                                },
                                // The directory the user asked for may simply be
                                // unreadable (a remote `/root`, say). Name it and
                                // keep the underlying reason: without the path the
                                // message reads as "nothing here" and the user has
                                // no idea their pick was refused rather than empty.
                                Err(error) => DirectoryState::List {
                                    entries: Vec::new(),
                                    parent_path: dir.clone(),
                                    error: Some(SharedString::from(format!(
                                        "{dir} を開けません: {error}"
                                    ))),
                                },
                            },
                            DirectoryState::None { create: true }
                            | DirectoryState::Create { .. } => match paths {
                                Ok(paths) => {
                                    let mut entries =
                                        path_candidates(parent_path_is_root, paths, sort_mode);
                                    let mut exists = false;
                                    let mut is_dir = false;
                                    let mut new_id = None;
                                    entries.retain(|entry| {
                                        new_id = new_id.max(Some(entry.path.id));
                                        if entry.path.string == suffix {
                                            exists = true;
                                            is_dir = entry.is_dir;
                                        }
                                        !exists || is_dir
                                    });

                                    let new_id = new_id.map(|id| id + 1).unwrap_or(0);
                                    let user_input = if suffix.is_empty() {
                                        None
                                    } else {
                                        Some(UserInput {
                                            file: StringMatchCandidate::new(new_id, &suffix),
                                            exists,
                                            is_dir,
                                        })
                                    };
                                    DirectoryState::Create {
                                        entries,
                                        parent_path: dir.clone(),
                                        user_input,
                                    }
                                }
                                Err(_) => DirectoryState::Create {
                                    entries: Vec::new(),
                                    parent_path: dir.clone(),
                                    user_input: Some(UserInput {
                                        exists: false,
                                        is_dir: false,
                                        file: StringMatchCandidate::new(0, &suffix),
                                    }),
                                },
                            },
                        };
                        this.delegate.directory_state = new_state;
                    })
                    .is_err()
                {
                    return;
                }
            }

            let Ok((mut new_entries, listing_failed, is_create_state)) =
                this.update(cx, |this, _| match &this.delegate.directory_state {
                    DirectoryState::List {
                        entries,
                        error: None,
                        ..
                    } => (entries.clone(), false, false),
                    DirectoryState::Create { entries, .. } => (entries.clone(), false, true),
                    DirectoryState::List { error: Some(_), .. } => (Vec::new(), true, false),
                    DirectoryState::None { create } => (Vec::new(), false, *create),
                })
            else {
                return;
            };

            if !hidden_entries {
                new_entries.retain(|entry| !entry.path.string.starts_with('.'));
            }

            let mut next_id = new_entries
                .iter()
                .map(|entry| entry.path.id)
                .max()
                .unwrap_or(0)
                + 1;

            // A directory listing never contains `..`, so the prompt has to offer
            // the way out itself. Without it the only way to leave the directory
            // the prompt opened in — the remote home directory, for the "open a
            // folder over SSH" picker — is to retype the whole path by hand, which
            // makes it look like nothing above that directory can be opened.
            //
            // The listing is reused as-is while the directory does not change, so
            // the row may already be in it from an earlier keystroke.
            let parent_dir_in_new_entries = new_entries
                .iter()
                .any(|entry| entry.path.string == parent_dir);
            let has_parent_entry = has_parent_target
                && !listing_failed
                && !is_create_state
                && !parent_dir_in_new_entries;
            if has_parent_entry {
                new_entries.insert(
                    0,
                    CandidateInfo {
                        path: StringMatchCandidate::new(next_id, parent_dir),
                        is_dir: true,
                    },
                );
                next_id += 1;
            }

            if suffix.is_empty() {
                let should_prepend_with_current_dir = this
                    .read_with(cx, |picker, _| {
                        !input_is_empty
                            && match &picker.delegate.directory_state {
                                DirectoryState::List { error, .. } => error.is_none(),
                                DirectoryState::Create { .. } => false,
                                DirectoryState::None { .. } => false,
                            }
                    })
                    .unwrap_or(false);

                let current_dir_in_new_entries = new_entries
                    .iter()
                    .any(|entry| &entry.path.string == current_dir);

                if should_prepend_with_current_dir && !current_dir_in_new_entries {
                    new_entries.insert(
                        0,
                        CandidateInfo {
                            path: StringMatchCandidate {
                                id: next_id,
                                string: current_dir.to_string(),
                                char_bag: CharBag::from(current_dir),
                            },
                            is_dir: true,
                        },
                    );
                }

                this.update(cx, |this, cx| {
                    this.delegate.selected_index = 0;
                    this.delegate.string_matches = new_entries
                        .iter()
                        .map(|m| StringMatch {
                            candidate_id: m.path.id,
                            score: 0.0,
                            positions: Vec::new(),
                            string: m.path.string.clone(),
                        })
                        .collect();
                    this.delegate.directory_state =
                        match &this.delegate.directory_state {
                            DirectoryState::None { create: false }
                            | DirectoryState::List { .. } => DirectoryState::List {
                                parent_path: dir.clone(),
                                entries: new_entries,
                                error: None,
                            },
                            DirectoryState::None { create: true }
                            | DirectoryState::Create { .. } => DirectoryState::Create {
                                parent_path: dir.clone(),
                                user_input: None,
                                entries: new_entries,
                            },
                        };
                    cx.notify();
                })
                .ok();
                return;
            }

            let candidates = new_entries
                .iter()
                .filter_map(|entry| {
                    if is_create_state && !entry.is_dir && Some(&suffix) == Some(&entry.path.string)
                    {
                        None
                    } else if !suffix.is_empty() && entry.path.string == current_dir {
                        None
                    } else {
                        Some(&entry.path)
                    }
                })
                .collect::<Vec<_>>();

            let matches = fuzzy::match_strings(
                candidates.as_slice(),
                &suffix,
                false,
                true,
                100,
                &cancel_flag,
                cx.background_executor().clone(),
            )
            .await;
            if cancel_flag.load(atomic::Ordering::Acquire) {
                return;
            }

            this.update(cx, |this, cx| {
                this.delegate.selected_index = 0;
                this.delegate.string_matches = matches.clone();
                this.delegate.string_matches.sort_by_key(|m| {
                    (
                        new_entries
                            .iter()
                            .find(|entry| entry.path.id == m.candidate_id)
                            .map(|entry| &entry.path)
                            .map(|candidate| !candidate.string.starts_with(&suffix)),
                        m.candidate_id,
                    )
                });
                this.delegate.directory_state = match &this.delegate.directory_state {
                    DirectoryState::None { create: false } | DirectoryState::List { .. } => {
                        DirectoryState::List {
                            entries: new_entries,
                            parent_path: dir.clone(),
                            error: None,
                        }
                    }
                    DirectoryState::None { create: true } => DirectoryState::Create {
                        entries: new_entries,
                        parent_path: dir.clone(),
                        user_input: Some(UserInput {
                            file: StringMatchCandidate::new(0, &suffix),
                            exists: false,
                            is_dir: false,
                        }),
                    },
                    DirectoryState::Create { user_input, .. } => {
                        let (new_id, exists, is_dir) = user_input
                            .as_ref()
                            .map(|input| (input.file.id, input.exists, input.is_dir))
                            .unwrap_or_else(|| (0, false, false));
                        DirectoryState::Create {
                            entries: new_entries,
                            parent_path: dir.clone(),
                            user_input: Some(UserInput {
                                file: StringMatchCandidate::new(new_id, &suffix),
                                exists,
                                is_dir,
                            }),
                        }
                    }
                };

                cx.notify();
            })
            .ok();
        })
    }
    fn select_child(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        let candidate = self.get_entry(self.selected_index)?;
        if candidate.path.string.is_empty() || !candidate.is_dir {
            return None;
        }
        if self.is_parent_dir_entry(&candidate) {
            return self.parent_query();
        }
        // Stepping into `./` would only re-list the directory already on screen,
        // and would do it through the query `<dir>/.//`.
        if self.is_current_dir_entry(&candidate) {
            return None;
        }
        self.child_query(&candidate)
    }

    fn select_parent(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        self.parent_query()
    }
    fn confirm_completion(
        &mut self,
        query: String,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        let candidate = self.get_entry(self.selected_index)?;
        if candidate.path.string.is_empty() || candidate.path.string == self.current_dir() {
            return None;
        }
        if self.is_parent_dir_entry(&candidate) {
            return self.parent_query();
        }

        let path_style = self.path_style;
        Some(
            maybe!({
                match &self.directory_state {
                    DirectoryState::Create { parent_path, .. } => Some(format!(
                        "{}{}{}",
                        parent_path,
                        candidate.path.string,
                        if candidate.is_dir {
                            path_style.primary_separator()
                        } else {
                            ""
                        }
                    )),
                    DirectoryState::List { parent_path, .. } => Some(format!(
                        "{}{}{}",
                        parent_path,
                        candidate.path.string,
                        if candidate.is_dir {
                            path_style.primary_separator()
                        } else {
                            ""
                        }
                    )),
                    DirectoryState::None { .. } => return None,
                }
            })
            .unwrap_or(query),
        )
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(candidate) = self.get_entry(self.selected_index) else {
            return;
        };

        // Confirming `../` has to move the prompt up rather than hand back
        // `<dir>/..` as the chosen path: the point of the row is to browse, and
        // an un-normalized `..` would end up as the opened worktree's path.
        if self.is_parent_dir_entry(&candidate) {
            if let Some(parent) = self.parent_query() {
                cx.defer_in(window, move |picker, window, cx| {
                    picker.set_query(&parent, window, cx);
                });
            }
            return;
        }

        if !secondary && self.should_browse_into(&candidate) {
            if let Some(child) = self.child_query(&candidate) {
                cx.defer_in(window, move |picker, window, cx| {
                    picker.set_query(&child, window, cx);
                });
            }
            return;
        }

        if self.is_current_dir_entry(&candidate) {
            self.confirm_listed_directory(cx);
            return;
        }

        match &self.directory_state {
            DirectoryState::None { .. } => return,
            DirectoryState::List { parent_path, .. } => {
                let confirmed_path =
                    if parent_path == &self.prompt_root && candidate.path.string.is_empty() {
                        PathBuf::from(&self.prompt_root)
                    } else {
                        let Some(confirmed_path) = self
                            .path_style
                            .join_path_preserving_components(
                                Path::new(self.lister.resolve_tilde(parent_path, cx).as_ref()),
                                &candidate.path.string,
                            )
                            .log_err()
                        else {
                            return;
                        };
                        confirmed_path
                    };
                if let Some(tx) = self.tx.take() {
                    tx.send(Some(vec![confirmed_path])).ok();
                }
            }
            DirectoryState::Create {
                parent_path,
                user_input,
                ..
            } => match user_input {
                None => return,
                Some(user_input) => {
                    if user_input.is_dir {
                        return;
                    }
                    let prompted_path =
                        if parent_path == &self.prompt_root && user_input.file.string.is_empty() {
                            PathBuf::from(&self.prompt_root)
                        } else {
                            let Some(prompted_path) = self
                                .path_style
                                .join_path_preserving_components(
                                    Path::new(self.lister.resolve_tilde(parent_path, cx).as_ref()),
                                    &user_input.file.string,
                                )
                                .log_err()
                            else {
                                return;
                            };
                            prompted_path
                        };
                    if user_input.exists {
                        self.should_dismiss = false;
                        let answer = window.prompt(
                            gpui::PromptLevel::Critical,
                            &format!("{prompted_path:?} はすでに存在します。置き換えますか?"),
                            Some(
                                "同じ名前のファイルまたはフォルダーがすでに存在します。置き換えると現在の内容が上書きされます。",
                            ),
                            // 上書き確認のボタンは F1-1 の project_panel に合わせて「置き換える」
                            &["置き換える", "キャンセル"],
                            cx
                        );
                        self.replace_prompt = cx.spawn_in(window, async move |picker, cx| {
                            let answer = answer.await.ok();
                            picker
                                .update(cx, |picker, cx| {
                                    picker.delegate.should_dismiss = true;
                                    if answer != Some(0) {
                                        return;
                                    }
                                    if let Some(tx) = picker.delegate.tx.take() {
                                        tx.send(Some(vec![prompted_path])).ok();
                                    }
                                    cx.emit(gpui::DismissEvent);
                                })
                                .ok();
                        });
                        return;
                    } else if let Some(tx) = self.tx.take() {
                        tx.send(Some(vec![prompted_path])).ok();
                    }
                }
            },
        }

        cx.emit(gpui::DismissEvent);
    }

    fn should_dismiss(&self) -> bool {
        self.should_dismiss
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.cancel_flag.store(true, atomic::Ordering::Release);
        if let Some(tx) = self.tx.take() {
            tx.send(None).ok();
        }
        cx.emit(gpui::DismissEvent)
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let settings = FileFinderSettings::get_global(cx);
        let candidate = self.get_entry(ix)?;
        let string_match = match &self.directory_state {
            DirectoryState::List { .. } => self.string_matches.get(ix),
            DirectoryState::Create { user_input, .. } => {
                if let Some(user_input) = user_input {
                    if !user_input.exists || !user_input.is_dir {
                        if ix == 0 {
                            None
                        } else {
                            self.string_matches.get(ix - 1)
                        }
                    } else {
                        self.string_matches.get(ix)
                    }
                } else {
                    self.string_matches.get(ix)
                }
            }
            DirectoryState::None { .. } => None,
        };

        // Directory entries and string matches can briefly go out of sync during
        // async updates. When that happens, render the row without highlights.
        let mut match_positions = string_match
            .filter(|string_match| {
                string_match.candidate_id == candidate.path.id
                    && string_match.string == candidate.path.string
            })
            .map(|string_match| string_match.positions.clone())
            .unwrap_or_default();

        let is_current_dir_candidate = candidate.path.string == self.current_dir();
        let is_parent_dir_candidate = self.is_parent_dir_entry(&candidate);

        let file_icon = maybe!({
            if !settings.file_icons {
                return None;
            }

            let path = path::Path::new(&candidate.path.string);
            let icon = if candidate.is_dir {
                if is_current_dir_candidate {
                    return Some(Icon::new(IconName::ReplyArrowRight).color(Color::Muted));
                } else if is_parent_dir_candidate {
                    return Some(Icon::new(IconName::ArrowUp).color(Color::Muted));
                } else {
                    FileIcons::get_folder_icon(false, path, cx)?
                }
            } else {
                FileIcons::get_icon(path, cx)?
            };
            Some(Icon::from_path(icon).color(Color::Muted))
        });

        match &self.directory_state {
            DirectoryState::List { parent_path, .. } => {
                let (label, indices) = if is_current_dir_candidate {
                    ("このフォルダーを開く".to_string(), vec![])
                } else if is_parent_dir_candidate {
                    (
                        match self.parent_query() {
                            Some(parent) => format!("上の階層へ移動 ({parent})"),
                            None => "上の階層へ移動".to_string(),
                        },
                        vec![],
                    )
                } else if *parent_path == self.prompt_root {
                    match_positions.iter_mut().for_each(|position| {
                        *position += self.prompt_root.len();
                    });
                    (
                        format!("{}{}", self.prompt_root, candidate.path.string),
                        match_positions,
                    )
                } else {
                    (candidate.path.string, match_positions)
                };
                Some(
                    ListItem::new(ix)
                        .spacing(ListItemSpacing::Sparse)
                        .start_slot::<Icon>(file_icon)
                        .inset(true)
                        .toggle_state(selected)
                        .child(HighlightedLabel::new(label, indices)),
                )
            }
            DirectoryState::Create {
                parent_path,
                user_input,
                ..
            } => {
                let (label, delta) = if *parent_path == self.prompt_root {
                    match_positions.iter_mut().for_each(|position| {
                        *position += self.prompt_root.len();
                    });
                    (
                        format!("{}{}", self.prompt_root, candidate.path.string),
                        self.prompt_root.len(),
                    )
                } else {
                    (candidate.path.string.clone(), 0)
                };

                let label_with_highlights = match user_input {
                    Some(user_input) => {
                        let label_len = label.len();
                        if user_input.file.string == candidate.path.string {
                            if user_input.exists {
                                let label = if user_input.is_dir {
                                    label
                                } else {
                                    format!("{label} (置換)")
                                };
                                StyledText::new(label)
                                    .with_default_highlights(
                                        &window.text_style(),
                                        vec![(
                                            delta..label_len,
                                            HighlightStyle::color(Color::Conflict.color(cx)),
                                        )],
                                    )
                                    .into_any_element()
                            } else {
                                StyledText::new(format!("{label} (作成)"))
                                    .with_default_highlights(
                                        &window.text_style(),
                                        vec![(
                                            delta..label_len,
                                            HighlightStyle::color(Color::Created.color(cx)),
                                        )],
                                    )
                                    .into_any_element()
                            }
                        } else {
                            HighlightedLabel::new(label, match_positions).into_any_element()
                        }
                    }
                    None => HighlightedLabel::new(label, match_positions).into_any_element(),
                };

                Some(
                    ListItem::new(ix)
                        .spacing(ListItemSpacing::Sparse)
                        .start_slot::<Icon>(file_icon)
                        .inset(true)
                        .toggle_state(selected)
                        .child(LabelLike::new().child(label_with_highlights)),
                )
            }
            DirectoryState::None { .. } => None,
        }
    }

    fn render_footer(
        &self,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        if let Some(footer) = (self.render_footer)(window, cx) {
            return Some(footer);
        }

        // Confirming a folder browses into it, so the way to actually choose one
        // has to be visible: without this the prompt looks like it can only ever
        // hand back whatever is highlighted.
        // An unreadable directory is not one the user can be offered to open.
        let DirectoryState::List {
            parent_path,
            error: None,
            ..
        } = &self.directory_state
        else {
            return None;
        };
        let listed_directory = SharedString::from(parent_path.clone());
        let selected_is_directory = self
            .get_entry(self.selected_index)
            .is_some_and(|candidate| self.should_browse_into(&candidate));
        let confirm_hint = if selected_is_directory {
            "Enter: フォルダーの中へ"
        } else {
            "Enter: 開く"
        };

        Some(
            v_flex()
                .child(Divider::horizontal())
                .child(
                    h_flex()
                        .p_1()
                        .gap_2()
                        .justify_between()
                        .child(
                            Label::new(format!(
                                "{confirm_hint} / Ctrl+Enter: 選択中のフォルダーに決定"
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .child(
                            Button::new("open-path-prompt-choose-folder", "このフォルダーを開く")
                                .label_size(LabelSize::Small)
                                .tooltip(move |_window, cx| {
                                    Tooltip::simple(
                                        format!("{listed_directory} をこのまま開きます"),
                                        cx,
                                    )
                                })
                                .on_click(cx.listener(|picker, _, _window, cx| {
                                    picker.delegate.confirm_listed_directory(cx);
                                })),
                        ),
                )
                .into_any(),
        )
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(match &self.directory_state {
            DirectoryState::Create { .. } => SharedString::from("パスを入力してください…"),
            DirectoryState::List {
                error: Some(error), ..
            } => error.clone(),
            DirectoryState::List { .. } | DirectoryState::None { .. } => {
                SharedString::from("該当するファイルまたはディレクトリはありません。")
            }
        })
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        Arc::from(
            format!(
                "[ディレクトリ{}]ファイル名.拡張子",
                self.path_style.primary_separator()
            )
            .as_str(),
        )
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        // The navigation rows (`./`, then `../`) are always first when present,
        // so the separator goes after the last of them rather than after a fixed
        // index.
        let navigation_rows = self
            .string_matches
            .iter()
            .take_while(|m| m.string == self.current_dir() || m.string == self.parent_dir())
            .count();
        if navigation_rows == 0 {
            Vec::new()
        } else {
            vec![navigation_rows - 1]
        }
    }
}

fn path_candidates(
    parent_path_is_root: bool,
    mut children: Vec<DirectoryItem>,
    sort_mode: ProjectPanelSortMode,
) -> Vec<CandidateInfo> {
    if parent_path_is_root {
        children.push(DirectoryItem {
            is_dir: true,
            path: PathBuf::default(),
        });
    }

    children.sort_by(|a, b| {
        let (a_is_file, b_is_file) = match sort_mode {
            ProjectPanelSortMode::DirectoriesFirst => (!a.is_dir, !b.is_dir),
            ProjectPanelSortMode::FilesFirst => (a.is_dir, b.is_dir),
            ProjectPanelSortMode::Mixed => (true, true),
        };
        compare_paths((&a.path, a_is_file), (&b.path, b_is_file))
    });
    children
        .iter()
        .enumerate()
        .map(|(ix, item)| CandidateInfo {
            path: StringMatchCandidate::new(ix, &item.path.to_string_lossy()),
            is_dir: item.is_dir,
        })
        .collect()
}

/// The query that lists the parent of `dir`, or `None` when `dir` already is the
/// root, resolves to it, or is not a path the prompt can walk up from.
///
/// `dir` is normalized first so that walking up from a query the user typed with
/// `.` or `..` components in it lands where those components actually point,
/// rather than one literal segment back.
fn parent_directory_query(dir: &str, path_style: PathStyle, prompt_root: &str) -> Option<String> {
    let normalized = path_style.normalize(dir);
    if normalized == prompt_root {
        return None;
    }
    let trimmed = normalized.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return None;
    }
    let (parent, _) = get_dir_and_suffix(trimmed.to_string(), path_style);
    (parent != dir).then_some(parent)
}

fn get_dir_and_suffix(query: String, path_style: PathStyle) -> (String, String) {
    match path_style {
        PathStyle::Unix => {
            let (mut dir, suffix) = if let Some(index) = query.rfind('/') {
                (query[..index].to_string(), query[index + 1..].to_string())
            } else {
                (query, String::new())
            };
            if !dir.ends_with('/') {
                dir.push('/');
            }
            (dir, suffix)
        }
        PathStyle::Windows => {
            let last_sep = query.rfind('\\').into_iter().chain(query.rfind('/')).max();
            let (mut dir, suffix) = if let Some(index) = last_sep {
                (
                    query[..index + 1].to_string(),
                    query[index + 1..].to_string(),
                )
            } else {
                (query, String::new())
            };
            if dir.len() < 3 {
                dir = "C:\\".to_string();
            }
            (dir, suffix)
        }
    }
}

#[cfg(test)]
mod tests {
    use util::paths::PathStyle;

    use super::{get_dir_and_suffix, parent_directory_query};

    #[test]
    fn test_parent_directory_query_with_posix_style() {
        let parent = |dir| parent_directory_query(dir, PathStyle::Unix, "/");

        assert_eq!(parent("/home/ubuntu/").as_deref(), Some("/home/"));
        assert_eq!(parent("/home/").as_deref(), Some("/"));
        assert_eq!(parent("/"), None);
        // A `..` the user typed points where it actually points, so walking up
        // from it does not just drop the last literal segment.
        assert_eq!(parent("/home/ubuntu/../").as_deref(), Some("/"));
        assert_eq!(parent("/home/ubuntu/./").as_deref(), Some("/home/"));
        // Nothing to walk up from: `~` is only expanded further down the stack.
        assert_eq!(parent("~/"), None);
    }

    #[test]
    fn test_parent_directory_query_with_windows_style() {
        let parent = |dir| parent_directory_query(dir, PathStyle::Windows, "C:\\");

        assert_eq!(parent("C:\\Users\\admin\\").as_deref(), Some("C:\\Users\\"));
        assert_eq!(parent("C:\\Users\\").as_deref(), Some("C:\\"));
        assert_eq!(parent("C:\\"), None);
        assert_eq!(parent("C:/Users/admin/").as_deref(), Some("C:\\Users\\"));
        assert_eq!(parent("C:\\Users\\admin\\..\\").as_deref(), Some("C:\\"));
    }

    #[test]
    fn test_get_dir_and_suffix_with_windows_style() {
        let (dir, suffix) = get_dir_and_suffix("".into(), PathStyle::Windows);
        assert_eq!(dir, "C:\\");
        assert_eq!(suffix, "");

        let (dir, suffix) = get_dir_and_suffix("C:".into(), PathStyle::Windows);
        assert_eq!(dir, "C:\\");
        assert_eq!(suffix, "");

        let (dir, suffix) = get_dir_and_suffix("C:\\".into(), PathStyle::Windows);
        assert_eq!(dir, "C:\\");
        assert_eq!(suffix, "");

        let (dir, suffix) = get_dir_and_suffix("C:\\Use".into(), PathStyle::Windows);
        assert_eq!(dir, "C:\\");
        assert_eq!(suffix, "Use");

        let (dir, suffix) =
            get_dir_and_suffix("C:\\Users\\Junkui\\Docum".into(), PathStyle::Windows);
        assert_eq!(dir, "C:\\Users\\Junkui\\");
        assert_eq!(suffix, "Docum");

        let (dir, suffix) =
            get_dir_and_suffix("C:\\Users\\Junkui\\Documents".into(), PathStyle::Windows);
        assert_eq!(dir, "C:\\Users\\Junkui\\");
        assert_eq!(suffix, "Documents");

        let (dir, suffix) =
            get_dir_and_suffix("C:\\Users\\Junkui\\Documents\\".into(), PathStyle::Windows);
        assert_eq!(dir, "C:\\Users\\Junkui\\Documents\\");
        assert_eq!(suffix, "");

        let (dir, suffix) = get_dir_and_suffix("C:\\root\\.".into(), PathStyle::Windows);
        assert_eq!(dir, "C:\\root\\");
        assert_eq!(suffix, ".");

        let (dir, suffix) = get_dir_and_suffix("C:\\root\\..".into(), PathStyle::Windows);
        assert_eq!(dir, "C:\\root\\");
        assert_eq!(suffix, "..");

        let (dir, suffix) = get_dir_and_suffix("C:\\root\\.hidden".into(), PathStyle::Windows);
        assert_eq!(dir, "C:\\root\\");
        assert_eq!(suffix, ".hidden");

        let (dir, suffix) = get_dir_and_suffix("C:/root/".into(), PathStyle::Windows);
        assert_eq!(dir, "C:/root/");
        assert_eq!(suffix, "");

        let (dir, suffix) = get_dir_and_suffix("C:/root/Use".into(), PathStyle::Windows);
        assert_eq!(dir, "C:/root/");
        assert_eq!(suffix, "Use");

        let (dir, suffix) = get_dir_and_suffix("C:\\root/Use".into(), PathStyle::Windows);
        assert_eq!(dir, "C:\\root/");
        assert_eq!(suffix, "Use");

        let (dir, suffix) = get_dir_and_suffix("C:/root\\.hidden".into(), PathStyle::Windows);
        assert_eq!(dir, "C:/root\\");
        assert_eq!(suffix, ".hidden");
    }

    #[test]
    fn test_get_dir_and_suffix_with_posix_style() {
        let (dir, suffix) = get_dir_and_suffix("".into(), PathStyle::Unix);
        assert_eq!(dir, "/");
        assert_eq!(suffix, "");

        let (dir, suffix) = get_dir_and_suffix("/".into(), PathStyle::Unix);
        assert_eq!(dir, "/");
        assert_eq!(suffix, "");

        let (dir, suffix) = get_dir_and_suffix("/Use".into(), PathStyle::Unix);
        assert_eq!(dir, "/");
        assert_eq!(suffix, "Use");

        let (dir, suffix) = get_dir_and_suffix("/Users/Junkui/Docum".into(), PathStyle::Unix);
        assert_eq!(dir, "/Users/Junkui/");
        assert_eq!(suffix, "Docum");

        let (dir, suffix) = get_dir_and_suffix("/Users/Junkui/Documents".into(), PathStyle::Unix);
        assert_eq!(dir, "/Users/Junkui/");
        assert_eq!(suffix, "Documents");

        let (dir, suffix) = get_dir_and_suffix("/Users/Junkui/Documents/".into(), PathStyle::Unix);
        assert_eq!(dir, "/Users/Junkui/Documents/");
        assert_eq!(suffix, "");

        let (dir, suffix) = get_dir_and_suffix("/root/.".into(), PathStyle::Unix);
        assert_eq!(dir, "/root/");
        assert_eq!(suffix, ".");

        let (dir, suffix) = get_dir_and_suffix("/root/..".into(), PathStyle::Unix);
        assert_eq!(dir, "/root/");
        assert_eq!(suffix, "..");

        let (dir, suffix) = get_dir_and_suffix("/root/.hidden".into(), PathStyle::Unix);
        assert_eq!(dir, "/root/");
        assert_eq!(suffix, ".hidden");
    }
}
