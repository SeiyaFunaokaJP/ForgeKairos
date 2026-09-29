//! A bound on what one language server response can make the project read.
//!
//! Every location in a response (definitions, references, call hierarchy) is turned into anchors
//! in a buffer, so every file the response names is opened whole. The server chooses the files
//! and their number, and on a remote project each one is also copied to the client. Callers cap
//! what they show, but only after all of that has happened.

use std::path::PathBuf;

use anyhow::Result;
use collections::HashMap;
use gpui::{AsyncApp, Entity};
use language::Buffer;
use lsp::LanguageServerId;
use util::{ResultExt as _, paths::UrlExt as _};

use crate::LspStore;

/// Files one response may open that were not open before it.
pub const MAX_FILES_OPENED_PER_RESPONSE: usize = 1000;
/// Bytes on disk one response may open that were not open before it. A single file larger than
/// this is never opened for a response. The bound is on the sum and not on each file because
/// going to the one definition inside a large generated source has to keep working.
pub const MAX_BYTES_OPENED_PER_RESPONSE: u64 = 64 * 1024 * 1024;

#[derive(Default)]
pub struct ResultBuffers {
    decisions: HashMap<lsp::Uri, bool>,
    files_opened: usize,
    bytes_opened: u64,
    locations_dropped: usize,
}

impl ResultBuffers {
    /// `None` when the response has used up what it may open: the caller drops the location.
    pub(crate) async fn open(
        &mut self,
        uri: lsp::Uri,
        server_id: LanguageServerId,
        lsp_store: &Entity<LspStore>,
        cx: &mut AsyncApp,
    ) -> Result<Option<Entity<Buffer>>> {
        let allowed = match self.decisions.get(&uri) {
            Some(allowed) => *allowed,
            None => {
                let allowed = self.admit(&uri, lsp_store, cx).await;
                self.decisions.insert(uri.clone(), allowed);
                allowed
            }
        };
        if !allowed {
            self.locations_dropped += 1;
            return Ok(None);
        }
        let buffer = lsp_store
            .update(cx, |lsp_store, cx| {
                lsp_store.open_local_buffer_via_lsp(uri, server_id, cx)
            })
            .await?;
        Ok(Some(buffer))
    }

    async fn admit(
        &mut self,
        uri: &lsp::Uri,
        lsp_store: &Entity<LspStore>,
        cx: &mut AsyncApp,
    ) -> bool {
        let (abs_path, already_open, fs) = lsp_store.read_with(cx, |lsp_store, cx| {
            let path_style = lsp_store.worktree_store.read(cx).path_style();
            let abs_path: Option<PathBuf> = uri.to_file_path_ext(path_style).ok();
            let already_open = abs_path
                .as_deref()
                .and_then(|abs_path| {
                    lsp_store
                        .worktree_store
                        .read(cx)
                        .project_path_for_absolute_path(abs_path, cx)
                })
                .and_then(|project_path| lsp_store.buffer_store.read(cx).get_by_path(&project_path))
                .is_some();
            let fs = lsp_store.as_local().map(|local| local.fs.clone());
            (abs_path, already_open, fs)
        });
        if already_open {
            return true;
        }
        if self.files_opened >= MAX_FILES_OPENED_PER_RESPONSE {
            return false;
        }
        // A path that is not a file on disk (inside an archive, or gone) is left to the opening
        // itself, which knows about the former and fails on the latter as it always did.
        let length = match (abs_path, fs) {
            (Some(abs_path), Some(fs)) => fs
                .metadata(&abs_path)
                .await
                .log_err()
                .flatten()
                .map_or(0, |metadata| metadata.len),
            _ => 0,
        };
        let Some(bytes_opened) = self
            .bytes_opened
            .checked_add(length)
            .filter(|bytes_opened| *bytes_opened <= MAX_BYTES_OPENED_PER_RESPONSE)
        else {
            return false;
        };
        self.bytes_opened = bytes_opened;
        self.files_opened += 1;
        true
    }

    pub(crate) fn finish(self, request: &str) {
        if self.locations_dropped > 0 {
            log::warn!(
                "{request}: dropped {} location(s) of the response after opening {} file(s) \
                 ({} bytes); one response opens at most {MAX_FILES_OPENED_PER_RESPONSE} files \
                 and {MAX_BYTES_OPENED_PER_RESPONSE} bytes",
                self.locations_dropped,
                self.files_opened,
                self.bytes_opened,
            );
        }
    }
}
