use std::sync::Arc;

use gpui::{Pixels, px};
use settings::{CallGraphSources, DockSide, IntoGpui, RegisterSetting, Settings};

const DEFAULT_WIDTH: f32 = 320.;
const DEFAULT_MAX_CHILDREN: usize = 200;
const DEFAULT_EXCLUDED_CALLEES: [&str; 11] = [
    "printk", "pr_info", "pr_err", "pr_debug", "printf", "fprintf", "assert", "BUG_ON", "WARN_ON",
    "likely", "unlikely",
];

#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct CallGraphPanelSettings {
    pub button: bool,
    pub dock: DockSide,
    pub default_width: Pixels,
    pub max_children: usize,
    pub follow_cursor: bool,
    pub sources: CallGraphSources,
    pub excluded_callees: Arc<[String]>,
}

impl Settings for CallGraphPanelSettings {
    // Falls back to the defaults instead of unwrapping, so that a settings
    // file written before this panel existed still loads.
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.call_graph_panel.clone().unwrap_or_default();
        Self {
            button: panel.button.unwrap_or(true),
            dock: panel.dock.unwrap_or(DockSide::Right),
            default_width: panel
                .default_width
                .map(IntoGpui::into_gpui)
                .unwrap_or(px(DEFAULT_WIDTH)),
            max_children: panel.max_children.unwrap_or(DEFAULT_MAX_CHILDREN).max(1),
            follow_cursor: panel.follow_cursor.unwrap_or(false),
            sources: panel.sources.unwrap_or_default(),
            excluded_callees: panel
                .excluded_callees
                .unwrap_or_else(|| {
                    DEFAULT_EXCLUDED_CALLEES
                        .iter()
                        .map(|name| name.to_string())
                        .collect()
                })
                .into(),
        }
    }
}
