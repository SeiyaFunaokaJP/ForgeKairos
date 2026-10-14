use fs::Fs;
use gpui::App;
use settings::{BaseKeymap, EditingModeContent, Settings, update_settings_file};

use crate::{HelixModeSetting, VimModeSetting};

/// The editing mode the resolved settings give (ADR-F91).
pub fn current_editing_mode(cx: &App) -> EditingModeContent {
    EditingModeContent::from_resolved(
        VimModeSetting::is_enabled(cx),
        HelixModeSetting::is_enabled(cx),
        BaseKeymap::try_get(cx).is_some_and(|base_keymap| *base_keymap == BaseKeymap::Emacs),
    )
}

/// Writes a choice of editing mode to the user settings file, for every place
/// that offers the choice (onboarding, the status bar).
///
/// Returns the mode that was in effect before, or `None` when the choice
/// changes nothing and nothing was written.
pub fn write_editing_mode(mode: EditingModeContent, cx: &mut App) -> Option<EditingModeContent> {
    let resolved_vim_mode = VimModeSetting::is_enabled(cx);
    let resolved_helix_mode = HelixModeSetting::is_enabled(cx);
    let previous_mode = current_editing_mode(cx);
    // With both on, the choice shown is Helix, but choosing it again still has
    // to switch Vim off.
    if mode == previous_mode && !(resolved_vim_mode && resolved_helix_mode) {
        return None;
    }

    // The quick action bar's switches override these for the session only. A
    // settings file that already says what the choice writes is not reloaded,
    // so the override would outlive the choice unless it is replaced here.
    VimModeSetting::override_global(VimModeSetting(mode == EditingModeContent::Vim), cx);
    HelixModeSetting::override_global(HelixModeSetting(mode == EditingModeContent::Helix), cx);

    let fs = <dyn Fs>::global(cx);
    update_settings_file(fs, cx, move |setting, _| {
        EditingModeContent::write_over_resolved(
            setting,
            mode,
            resolved_vim_mode,
            resolved_helix_mode,
        );
    });

    Some(previous_mode)
}
