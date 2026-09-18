use std::fmt::{Display, Formatter};

use crate::{self as settings, settings_content::BaseKeymapContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings};

/// Base key bindings scheme. Base keymaps can be overridden with user keymaps.
///
/// Default: Zed
#[derive(
    Copy, Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default, RegisterSetting,
)]
pub enum BaseKeymap {
    #[default]
    Zed,
    VSCode,
    JetBrains,
    SublimeText,
    Atom,
    TextMate,
    Emacs,
    Cursor,
    None,
}

impl From<BaseKeymapContent> for BaseKeymap {
    fn from(value: BaseKeymapContent) -> Self {
        match value {
            BaseKeymapContent::Zed => Self::Zed,
            BaseKeymapContent::VSCode => Self::VSCode,
            BaseKeymapContent::JetBrains => Self::JetBrains,
            BaseKeymapContent::SublimeText => Self::SublimeText,
            BaseKeymapContent::Atom => Self::Atom,
            BaseKeymapContent::TextMate => Self::TextMate,
            BaseKeymapContent::Emacs => Self::Emacs,
            BaseKeymapContent::Cursor => Self::Cursor,
            BaseKeymapContent::None => Self::None,
        }
    }
}
impl Into<BaseKeymapContent> for BaseKeymap {
    fn into(self) -> BaseKeymapContent {
        match self {
            BaseKeymap::Zed => BaseKeymapContent::Zed,
            BaseKeymap::VSCode => BaseKeymapContent::VSCode,
            BaseKeymap::JetBrains => BaseKeymapContent::JetBrains,
            BaseKeymap::SublimeText => BaseKeymapContent::SublimeText,
            BaseKeymap::Atom => BaseKeymapContent::Atom,
            BaseKeymap::TextMate => BaseKeymapContent::TextMate,
            BaseKeymap::Emacs => BaseKeymapContent::Emacs,
            BaseKeymap::Cursor => BaseKeymapContent::Cursor,
            BaseKeymap::None => BaseKeymapContent::None,
        }
    }
}

impl Display for BaseKeymap {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BaseKeymap::Zed => write!(f, "Zed"),
            BaseKeymap::VSCode => write!(f, "VS Code"),
            BaseKeymap::JetBrains => write!(f, "JetBrains"),
            BaseKeymap::SublimeText => write!(f, "Sublime Text"),
            BaseKeymap::Atom => write!(f, "Atom"),
            BaseKeymap::TextMate => write!(f, "TextMate"),
            BaseKeymap::Emacs => write!(f, "Emacs (beta)"),
            BaseKeymap::Cursor => write!(f, "Cursor (beta)"),
            BaseKeymap::None => write!(f, "None"),
        }
    }
}

impl BaseKeymap {
    #[cfg(target_os = "macos")]
    pub const OPTIONS: [(&'static str, Self); 8] = [
        ("Zed (既定)", Self::Zed),
        ("VS Code", Self::VSCode),
        ("Atom", Self::Atom),
        ("JetBrains", Self::JetBrains),
        ("Sublime Text", Self::SublimeText),
        ("Emacs (ベータ)", Self::Emacs),
        ("TextMate", Self::TextMate),
        ("Cursor (ベータ)", Self::Cursor),
    ];

    #[cfg(not(target_os = "macos"))]
    pub const OPTIONS: [(&'static str, Self); 7] = [
        ("Zed (既定)", Self::Zed),
        ("VS Code", Self::VSCode),
        ("Atom", Self::Atom),
        ("JetBrains", Self::JetBrains),
        ("Sublime Text", Self::SublimeText),
        ("Emacs (ベータ)", Self::Emacs),
        ("Cursor (ベータ)", Self::Cursor),
    ];

    pub fn asset_path(&self) -> Option<&'static str> {
        #[cfg(target_os = "macos")]
        match self {
            BaseKeymap::JetBrains => Some("keymaps/macos/jetbrains.json"),
            BaseKeymap::SublimeText => Some("keymaps/macos/sublime_text.json"),
            BaseKeymap::Atom => Some("keymaps/macos/atom.json"),
            BaseKeymap::TextMate => Some("keymaps/macos/textmate.json"),
            BaseKeymap::Emacs => Some("keymaps/macos/emacs.json"),
            BaseKeymap::Cursor => Some("keymaps/macos/cursor.json"),
            BaseKeymap::VSCode => Some("keymaps/macos/vscode.json"),
            BaseKeymap::Zed => None,
            BaseKeymap::None => None,
        }

        #[cfg(not(target_os = "macos"))]
        match self {
            BaseKeymap::JetBrains => Some("keymaps/linux/jetbrains.json"),
            BaseKeymap::SublimeText => Some("keymaps/linux/sublime_text.json"),
            BaseKeymap::Atom => Some("keymaps/linux/atom.json"),
            BaseKeymap::Emacs => Some("keymaps/linux/emacs.json"),
            BaseKeymap::Cursor => Some("keymaps/linux/cursor.json"),
            BaseKeymap::TextMate => None,
            BaseKeymap::VSCode => Some("keymaps/linux/vscode.json"),
            BaseKeymap::Zed => None,
            BaseKeymap::None => None,
        }
    }

    pub fn names() -> impl Iterator<Item = &'static str> {
        Self::OPTIONS.iter().map(|(name, _)| *name)
    }

    pub fn from_names(option: &str) -> BaseKeymap {
        Self::OPTIONS
            .iter()
            .copied()
            .find_map(|(name, value)| (name == option).then_some(value))
            .unwrap_or_default()
    }
}

impl Settings for BaseKeymap {
    fn from_settings(s: &crate::settings_content::SettingsContent) -> Self {
        s.base_keymap.unwrap().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_option_name_resolves_back_to_its_own_variant() {
        // `from_names` takes the label the onboarding picker showed and looks
        // it up in `OPTIONS` again, keeping the *first* entry that carries it
        // and otherwise falling back to `Zed`. Two labels that collide would
        // therefore hand the user a keymap they did not pick, without any
        // error — the one way translating the labels can go wrong.
        for (name, value) in BaseKeymap::OPTIONS {
            assert_eq!(BaseKeymap::from_names(name), value, "name: {name}");
        }
    }

    #[test]
    fn the_translated_picker_labels_stay_translated() {
        // The picker (`onboarding::base_keymap_picker`) and the command
        // palette are the only places these strings show up, so nothing else
        // would notice them turning back into English on an upstream-shaped
        // edit.
        let names = BaseKeymap::names().collect::<Vec<_>>();
        assert!(names.contains(&"Zed (既定)"), "{names:?}");
        assert!(names.contains(&"Emacs (ベータ)"), "{names:?}");
        assert!(names.contains(&"Cursor (ベータ)"), "{names:?}");
        // The vendors' own spellings are left alone, and typing them still
        // finds the row: the picker matches on these strings.
        assert!(names.contains(&"VS Code"), "{names:?}");
        assert!(
            names.iter().all(|name| !name.contains("(beta)")),
            "{names:?}"
        );
    }

    #[test]
    fn the_telemetry_display_stays_in_english() {
        // `Display` is what `base_keymap_picker.rs` and `main.rs` report to
        // telemetry (ADR §5.11); translating it would change the values Zed
        // Industries already collects, so only `OPTIONS` (this UI's labels)
        // and `BaseKeymapContent::VARIANTS` (the settings UI's) are Japanese.
        assert_eq!(BaseKeymap::Zed.to_string(), "Zed");
        assert_eq!(BaseKeymap::Emacs.to_string(), "Emacs (beta)");
        assert_eq!(BaseKeymap::Cursor.to_string(), "Cursor (beta)");
        assert_eq!(BaseKeymap::None.to_string(), "None");
    }
}
