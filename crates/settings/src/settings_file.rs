use crate::{settings_content::SettingsContent, settings_store::SettingsStore};
use collections::HashSet;
use fs::{Fs, PathEventKind};
use futures::{StreamExt, channel::mpsc};
use gpui::{App, BackgroundExecutor, ReadGlobal};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;

    use gpui::TestAppContext;
    use serde_json::json;
    use std::path::Path;

    #[gpui::test]
    async fn test_watch_config_dir_reloads_tracked_file_on_rescan(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let fs = FakeFs::new(cx.background_executor.clone());
        let config_dir = PathBuf::from("/root/config");
        let settings_path = PathBuf::from("/root/config/settings.json");

        fs.insert_tree(
            Path::new("/root"),
            json!({
                "config": {
                    "settings.json": "A"
                }
            }),
        )
        .await;

        let mut rx = watch_config_dir(
            &cx.background_executor,
            fs.clone(),
            config_dir.clone(),
            HashSet::from_iter([settings_path.clone()]),
        );

        assert_eq!(rx.next().await.as_deref(), Some("A"));
        cx.run_until_parked();

        fs.pause_events();
        fs.insert_file(&settings_path, b"B".to_vec()).await;
        fs.clear_buffered_events();

        fs.emit_fs_event(&settings_path, Some(PathEventKind::Rescan));
        fs.unpause_events_and_flush();
        assert_eq!(rx.next().await.as_deref(), Some("B"));

        fs.pause_events();
        fs.insert_file(&settings_path, b"A".to_vec()).await;
        fs.clear_buffered_events();

        fs.emit_fs_event(&config_dir, Some(PathEventKind::Rescan));
        fs.unpause_events_and_flush();
        assert_eq!(rx.next().await.as_deref(), Some("A"));
    }

    #[gpui::test]
    async fn test_watch_config_file_reloads_when_parent_dir_is_symlink(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fs = FakeFs::new(cx.background_executor.clone());
        let config_settings_path = PathBuf::from("/root/.config/zed/settings.json");
        let target_settings_path = PathBuf::from("/root/dotfiles/zed/settings.json");

        fs.insert_tree(
            Path::new("/root"),
            json!({
                ".config": {},
                "dotfiles": {
                    "zed": {
                        "settings.json": "A"
                    }
                }
            }),
        )
        .await;

        fs.create_symlink(
            Path::new("/root/.config/zed"),
            PathBuf::from("/root/dotfiles/zed"),
        )
        .await
        .unwrap();

        let (mut rx, _task) =
            watch_config_file(&cx.background_executor, fs.clone(), config_settings_path);
        assert_eq!(rx.next().await.as_deref(), Some("A"));

        fs.insert_file(&target_settings_path, b"B".to_vec()).await;
        assert_eq!(rx.next().await.as_deref(), Some("B"));
    }
}

pub const EMPTY_THEME_NAME: &str = "empty-theme";

/// Settings for visual tests that use proper fonts instead of Courier.
/// Uses Helvetica Neue for UI (sans-serif) and Menlo for code (monospace),
/// which are available on all macOS systems.
#[cfg(any(test, feature = "test-support"))]
pub fn visual_test_settings() -> String {
    let mut value =
        crate::parse_json_with_comments::<serde_json::Value>(crate::default_settings().as_ref())
            .unwrap();
    util::merge_non_null_json_value_into(
        serde_json::json!({
            "ui_font_family": ".SystemUIFont",
            "ui_font_features": {},
            "ui_font_size": 14,
            "ui_font_fallback": [],
            "buffer_font_family": "Menlo",
            "buffer_font_features": {},
            "buffer_font_size": 14,
            "buffer_font_fallbacks": [],
            "theme": EMPTY_THEME_NAME,
        }),
        &mut value,
    );
    value.as_object_mut().unwrap().remove("languages");
    serde_json::to_string(&value).unwrap()
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_settings() -> &'static str {
    static CACHED: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let mut value = crate::parse_json_with_comments::<serde_json::Value>(
            crate::default_settings().as_ref(),
        )
        .unwrap();
        // This fork ships the agent off by default (.forgekairos-docs/DESIGN.md
        // ADR-F09). Tests across the workspace assume upstream's "on" default -
        // the multi-workspace sidebar in particular is gated on it and its
        // absence changes every layout measurement - so opt back in here rather
        // than in each crate's `init_test`. Tests that care about the off state
        // set it themselves.
        // `agent.sidebar_enabled` rides along for the same reason: this fork
        // hides the window-wide threads sidebar by default (the agent panel has
        // its own history), but `multi_workspace_enabled` gates on it, so
        // leaving it off would change every layout measurement upstream tests
        // make.
        util::merge_non_null_json_value_into(
            serde_json::json!({ "agent": { "enabled": true, "sidebar_enabled": true } }),
            &mut value,
        );
        // Same story for telemetry: this fork ships it off (F2-1), but
        // upstream tests (e.g. remote_server's telemetry forwarding test)
        // assume events are actually queued.
        util::merge_non_null_json_value_into(
            serde_json::json!({ "telemetry": { "diagnostics": true, "metrics": true } }),
            &mut value,
        );
        // This fork defaults to the VSCode 風 arrangement (files left, agent
        // right), i.e. `PanelLayout::EDITOR`, while upstream defaults to
        // `PanelLayout::AGENT`. Upstream tests assert against the agent preset
        // (agent_settings::test_get_layout, test_set_layout_minimal_diff) and
        // several workspace/panel tests measure the default dock sides, so keep
        // the upstream docks here. Tests that care about the fork default set
        // it themselves.
        util::merge_non_null_json_value_into(
            serde_json::json!({
                "agent": { "dock": "left" },
                "project_panel": { "dock": "right" },
                "outline_panel": { "dock": "right" },
                "collaboration_panel": { "dock": "right" },
                "git_panel": { "dock": "right" },
            }),
            &mut value,
        );
        // This fork defaults `show_whitespaces` to "all" (matching the
        // Japanese editor convention of always showing whitespace as a dot),
        // while upstream defaults to "selection". Several editor rendering
        // tests assert on the *absence* of invisibles outside an explicit
        // `ShowWhitespaceSetting::All` override, so restore upstream's
        // default here rather than patching every such test.
        util::merge_non_null_json_value_into(
            serde_json::json!({ "show_whitespaces": "selection" }),
            &mut value,
        );
        #[cfg(not(target_os = "windows"))]
        util::merge_non_null_json_value_into(
            serde_json::json!({
                "format_on_save": "on",
                "ui_font_family": "Courier",
                "ui_font_features": {},
                "ui_font_size": 14,
                "ui_font_fallback": [],
                "buffer_font_family": "Courier",
                "buffer_font_features": {},
                "buffer_font_size": 14,
                "buffer_font_fallbacks": [],
                "theme": EMPTY_THEME_NAME,
            }),
            &mut value,
        );
        #[cfg(target_os = "windows")]
        util::merge_non_null_json_value_into(
            serde_json::json!({
                "format_on_save": "on",
                "ui_font_family": "Courier New",
                "ui_font_features": {},
                "ui_font_size": 14,
                "ui_font_fallback": [],
                "buffer_font_family": "Courier New",
                "buffer_font_features": {},
                "buffer_font_size": 14,
                "buffer_font_fallbacks": [],
                "theme": EMPTY_THEME_NAME,
            }),
            &mut value,
        );
        value.as_object_mut().unwrap().remove("languages");
        serde_json::to_string(&value).unwrap()
    });
    &CACHED
}

pub fn watch_config_file(
    executor: &BackgroundExecutor,
    fs: Arc<dyn Fs>,
    path: PathBuf,
) -> (mpsc::UnboundedReceiver<String>, gpui::Task<()>) {
    let (tx, rx) = mpsc::unbounded();
    let task = executor.spawn(async move {
        let path = fs.canonicalize(&path).await.unwrap_or_else(|_| path);
        let (events, _) = fs.watch(&path, Duration::from_millis(100)).await;
        futures::pin_mut!(events);

        let contents = fs.load(&path).await.unwrap_or_default();
        if tx.unbounded_send(contents).is_err() {
            return;
        }

        loop {
            if events.next().await.is_none() {
                break;
            }

            if let Ok(contents) = fs.load(&path).await
                && tx.unbounded_send(contents).is_err()
            {
                break;
            }
        }
    });
    (rx, task)
}

pub fn watch_config_dir(
    executor: &BackgroundExecutor,
    fs: Arc<dyn Fs>,
    dir_path: PathBuf,
    config_paths: HashSet<PathBuf>,
) -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded();
    executor
        .spawn(async move {
            for file_path in &config_paths {
                if fs.metadata(file_path).await.is_ok_and(|v| v.is_some())
                    && let Ok(contents) = fs.load(file_path).await
                    && tx.unbounded_send(contents).is_err()
                {
                    return;
                }
            }

            let (events, _) = fs.watch(&dir_path, Duration::from_millis(100)).await;
            futures::pin_mut!(events);

            while let Some(event_batch) = events.next().await {
                for event in event_batch {
                    if config_paths.contains(&event.path) {
                        match event.kind {
                            Some(PathEventKind::Removed) => {
                                if tx.unbounded_send(String::new()).is_err() {
                                    return;
                                }
                            }
                            Some(PathEventKind::Created) | Some(PathEventKind::Changed) => {
                                if let Ok(contents) = fs.load(&event.path).await
                                    && tx.unbounded_send(contents).is_err()
                                {
                                    return;
                                }
                            }
                            Some(PathEventKind::Rescan) => {
                                for file_path in &config_paths {
                                    if let Ok(contents) = fs.load(file_path).await
                                        && tx.unbounded_send(contents).is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            _ => {}
                        }
                    } else if matches!(event.kind, Some(PathEventKind::Rescan))
                        && event.path == dir_path
                    {
                        for file_path in &config_paths {
                            if let Ok(contents) = fs.load(file_path).await
                                && tx.unbounded_send(contents).is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
        })
        .detach();

    rx
}

pub fn update_settings_file(
    fs: Arc<dyn Fs>,
    cx: &App,
    update: impl 'static + Send + FnOnce(&mut SettingsContent, &App),
) {
    SettingsStore::global(cx).update_settings_file(fs, update)
}

pub fn update_settings_file_with_completion(
    fs: Arc<dyn Fs>,
    cx: &App,
    update: impl 'static + Send + FnOnce(&mut SettingsContent, &App),
) -> futures::channel::oneshot::Receiver<anyhow::Result<()>> {
    SettingsStore::global(cx).update_settings_file_with_completion(fs, update)
}
