//! ADR-F104: the directories of each worktree that the user is looking at —
//! expanded and visible in the project panel, and the parent directories of
//! open buffers — handed to the worktree, so that the host that holds the
//! files watches them natively before the rest. Without this a large worktree
//! leaves them to the fallback poller, whose round can take minutes.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use collections::HashMap;
use gpui::{AppContext as _, Context, Entity, Subscription, Task};
use language::Buffer;
use text::BufferId;
use util::rel_path::RelPath;
use worktree::{File, WorktreeId};

use crate::{
    buffer_store::{BufferStore, BufferStoreEvent},
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};

/// The project panel recomputes its entries after every file system event, so
/// a burst of them would otherwise send a set each time.
pub const WATCH_PRIORITY_DEBOUNCE: Duration = Duration::from_millis(250);

/// Each directory costs the host one of its scarce native watches.
pub const MAX_WATCH_PRIORITY_PATHS: usize = 1024;

pub struct WatchPriority {
    worktree_store: Entity<WorktreeStore>,
    open_file_directories: HashMap<BufferId, (WorktreeId, Arc<RelPath>)>,
    panel_directories: HashMap<WorktreeId, Vec<Arc<RelPath>>>,
    last_sent: HashMap<WorktreeId, Vec<Arc<RelPath>>>,
    send_task: Option<Task<()>>,
    _subscriptions: [Subscription; 2],
}

impl WatchPriority {
    pub fn new(
        worktree_store: &Entity<WorktreeStore>,
        buffer_store: &Entity<BufferStore>,
        cx: &mut gpui::App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let subscriptions = [
                cx.subscribe(buffer_store, Self::on_buffer_store_event),
                cx.subscribe(worktree_store, Self::on_worktree_store_event),
            ];
            let mut this = Self {
                worktree_store: worktree_store.clone(),
                open_file_directories: HashMap::default(),
                panel_directories: HashMap::default(),
                last_sent: HashMap::default(),
                send_task: None,
                _subscriptions: subscriptions,
            };
            let buffers = buffer_store.read(cx).buffers().collect::<Vec<_>>();
            for buffer in buffers {
                this.track_buffer(&buffer, cx);
            }
            this
        })
    }

    /// Replaces the directories the project panel shows expanded for
    /// `worktree_id`.
    pub fn set_panel_directories(
        &mut self,
        worktree_id: WorktreeId,
        directories: Vec<Arc<RelPath>>,
        cx: &mut Context<Self>,
    ) {
        // The panel reports a worktree it stopped showing as empty, which can
        // come after the worktree is gone and this tracker forgot it.
        if directories.is_empty() {
            self.panel_directories.remove(&worktree_id);
        } else {
            self.panel_directories.insert(worktree_id, directories);
        }
        self.schedule_send(cx);
    }

    fn on_buffer_store_event(
        &mut self,
        _: Entity<BufferStore>,
        event: &BufferStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            BufferStoreEvent::BufferAdded(buffer)
            | BufferStoreEvent::BufferChangedFilePath { buffer, .. } => {
                self.track_buffer(buffer, cx);
            }
            BufferStoreEvent::BufferDropped(buffer_id) => {
                if self.open_file_directories.remove(buffer_id).is_some() {
                    self.schedule_send(cx);
                }
            }
            BufferStoreEvent::SharedBufferClosed(..) => {}
        }
    }

    fn on_worktree_store_event(
        &mut self,
        _: Entity<WorktreeStore>,
        event: &WorktreeStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            WorktreeStoreEvent::WorktreeAdded(_) => self.schedule_send(cx),
            WorktreeStoreEvent::WorktreeRemoved(_, worktree_id)
            | WorktreeStoreEvent::WorktreeReleased(_, worktree_id) => {
                self.panel_directories.remove(worktree_id);
                self.last_sent.remove(worktree_id);
                self.open_file_directories
                    .retain(|_, (buffer_worktree_id, _)| buffer_worktree_id != worktree_id);
            }
            _ => {}
        }
    }

    fn track_buffer(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        let buffer = buffer.read(cx);
        let directory = File::from_dyn(buffer.file())
            .and_then(|file| Some((file.worktree_id(cx), file.path.parent()?.into_arc())));
        let changed = match directory {
            Some(directory) => {
                self.open_file_directories
                    .insert(buffer.remote_id(), directory.clone())
                    != Some(directory)
            }
            None => self
                .open_file_directories
                .remove(&buffer.remote_id())
                .is_some(),
        };
        if changed {
            self.schedule_send(cx);
        }
    }

    fn schedule_send(&mut self, cx: &mut Context<Self>) {
        let timer = cx.background_executor().timer(WATCH_PRIORITY_DEBOUNCE);
        self.send_task = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| this.send_changed(cx)).ok();
        }));
    }

    fn send_changed(&mut self, cx: &mut Context<Self>) {
        self.send_task = None;
        let worktrees = self.worktree_store.read(cx).worktrees().collect::<Vec<_>>();
        for worktree in worktrees {
            let worktree_id = worktree.read(cx).id();
            let paths = self.combined(worktree_id);
            let last_sent = self
                .last_sent
                .get(&worktree_id)
                .map_or(&[][..], Vec::as_slice);
            if paths == last_sent {
                continue;
            }
            self.last_sent.insert(worktree_id, paths.clone());
            worktree.update(cx, |worktree, cx| worktree.set_watch_priority(paths, cx));
        }
    }

    /// Open files first, so that the cap drops what the panel shows before the
    /// directory of a file being edited.
    fn combined(&self, worktree_id: WorktreeId) -> Vec<Arc<RelPath>> {
        let open_file_directories = self
            .open_file_directories
            .values()
            .filter(|(buffer_worktree_id, _)| *buffer_worktree_id == worktree_id)
            .map(|(_, directory)| directory.clone())
            .collect::<BTreeSet<_>>();
        let panel_directories = self
            .panel_directories
            .get(&worktree_id)
            .into_iter()
            .flatten()
            .filter(|directory| !open_file_directories.contains(*directory))
            .cloned()
            .collect::<BTreeSet<_>>();
        open_file_directories
            .into_iter()
            .chain(panel_directories)
            .take(MAX_WATCH_PRIORITY_PATHS)
            .collect()
    }
}
