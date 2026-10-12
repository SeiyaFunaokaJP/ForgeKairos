use crate::{BaseKeymapContent, SettingsContent};

/// How keys edit text, offered as one choice by onboarding and the settings
/// window (ADR-F91).
///
/// It is not stored: it is read from and written to `vim_mode`, `helix_mode`
/// and `base_keymap`, which stay the settings that the editor reads. Vim and
/// Helix are layered over the base keymap, while Emacs *is* a base keymap, so
/// one choice has to touch up to three settings.
#[derive(Copy, Clone, Debug, PartialEq, Eq, strum::VariantArray, strum::VariantNames)]
pub enum EditingModeContent {
    Normal,
    Vim,
    Helix,
    Emacs,
}

impl EditingModeContent {
    /// The mode the resolved settings give. When more than one is switched on,
    /// the editor runs Vim over Helix, and either over the Emacs keymap.
    pub fn from_resolved(vim_mode: bool, helix_mode: bool, base_keymap_is_emacs: bool) -> Self {
        if vim_mode {
            Self::Vim
        } else if helix_mode {
            Self::Helix
        } else if base_keymap_is_emacs {
            Self::Emacs
        } else {
            Self::Normal
        }
    }

    /// The mode one settings file sets, or `None` when the file leaves it to
    /// the files below it.
    ///
    /// A file that only switches Vim or Helix off says "Normal" (the files
    /// below can only be the defaults, which switch both off). A file that
    /// only names a base keymap other than Emacs says nothing, so that
    /// choosing VS Code keys does not show up as a choice of editing mode.
    pub fn read(content: &SettingsContent) -> Option<Self> {
        if content.vim_mode == Some(true) {
            Some(Self::Vim)
        } else if content.helix_mode == Some(true) {
            Some(Self::Helix)
        } else if content.base_keymap == Some(BaseKeymapContent::Emacs) {
            Some(Self::Emacs)
        } else if content.vim_mode.is_some() || content.helix_mode.is_some() {
            Some(Self::Normal)
        } else {
            None
        }
    }

    /// [`Self::read`] for a setting field that has to hand out a reference.
    pub fn read_static(content: &SettingsContent) -> Option<&'static Self> {
        Self::read(content).map(|mode| match mode {
            Self::Normal => &Self::Normal,
            Self::Vim => &Self::Vim,
            Self::Helix => &Self::Helix,
            Self::Emacs => &Self::Emacs,
        })
    }

    /// Writes `mode` into one settings file; `None` removes what the choice
    /// wrote, giving the defaults back.
    ///
    /// Leaving Emacs gives the base keymap back to the default only when the
    /// file holds Emacs: a base keymap chosen with Vim or Helix, such as VS
    /// Code, stays.
    pub fn write(content: &mut SettingsContent, mode: Option<Self>) {
        let Some(mode) = mode else {
            content.vim_mode = None;
            content.helix_mode = None;
            if content.base_keymap == Some(BaseKeymapContent::Emacs) {
                content.base_keymap = None;
            }
            return;
        };

        let turn_on_or_off = |field: &mut Option<bool>, on: bool| {
            if on {
                *field = Some(true);
            } else if field.is_some() {
                *field = Some(false);
            }
        };
        turn_on_or_off(&mut content.vim_mode, mode == Self::Vim);
        turn_on_or_off(&mut content.helix_mode, mode == Self::Helix);

        if mode == Self::Emacs {
            content.base_keymap = Some(BaseKeymapContent::Emacs);
        } else if content.base_keymap == Some(BaseKeymapContent::Emacs) {
            content.base_keymap = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(
        vim_mode: Option<bool>,
        helix_mode: Option<bool>,
        base_keymap: Option<BaseKeymapContent>,
    ) -> SettingsContent {
        SettingsContent {
            vim_mode,
            helix_mode,
            base_keymap,
            ..SettingsContent::default()
        }
    }

    fn written(
        content: &SettingsContent,
    ) -> (Option<bool>, Option<bool>, Option<BaseKeymapContent>) {
        (content.vim_mode, content.helix_mode, content.base_keymap)
    }

    #[test]
    fn resolved_settings_give_vim_over_helix_over_emacs() {
        use EditingModeContent::*;
        assert_eq!(
            EditingModeContent::from_resolved(false, false, false),
            Normal
        );
        assert_eq!(EditingModeContent::from_resolved(true, false, false), Vim);
        assert_eq!(EditingModeContent::from_resolved(false, true, false), Helix);
        assert_eq!(EditingModeContent::from_resolved(false, false, true), Emacs);
        assert_eq!(EditingModeContent::from_resolved(true, true, true), Vim);
        assert_eq!(EditingModeContent::from_resolved(false, true, true), Helix);
    }

    #[test]
    fn a_file_names_a_mode_only_when_it_sets_one() {
        use EditingModeContent as Mode;
        assert_eq!(Mode::read(&settings(None, None, None)), None);
        assert_eq!(
            Mode::read(&settings(None, None, Some(BaseKeymapContent::VSCode))),
            None
        );
        assert_eq!(
            Mode::read(&settings(Some(false), None, None)),
            Some(Mode::Normal)
        );
        assert_eq!(
            Mode::read(&settings(
                None,
                Some(false),
                Some(BaseKeymapContent::VSCode)
            )),
            Some(Mode::Normal)
        );
        assert_eq!(
            Mode::read(&settings(Some(true), None, None)),
            Some(Mode::Vim)
        );
        assert_eq!(
            Mode::read(&settings(None, Some(true), None)),
            Some(Mode::Helix)
        );
        assert_eq!(
            Mode::read(&settings(None, None, Some(BaseKeymapContent::Emacs))),
            Some(Mode::Emacs)
        );
        assert_eq!(
            Mode::read(&settings(
                Some(false),
                Some(false),
                Some(BaseKeymapContent::Emacs)
            )),
            Some(Mode::Emacs)
        );
        assert_eq!(
            Mode::read(&settings(
                Some(true),
                Some(true),
                Some(BaseKeymapContent::Emacs)
            )),
            Some(Mode::Vim)
        );
        assert_eq!(
            Mode::read_static(&settings(None, Some(true), None)),
            Some(&Mode::Helix)
        );
        assert_eq!(Mode::read_static(&settings(None, None, None)), None);
    }

    #[test]
    fn choosing_a_mode_switches_the_others_off() {
        use EditingModeContent as Mode;

        let mut content = settings(None, None, None);
        Mode::write(&mut content, Some(Mode::Vim));
        // Helix was never set, so it is left to the default rather than
        // written as false.
        assert_eq!(written(&content), (Some(true), None, None));

        Mode::write(&mut content, Some(Mode::Helix));
        assert_eq!(written(&content), (Some(false), Some(true), None));

        Mode::write(&mut content, Some(Mode::Emacs));
        assert_eq!(
            written(&content),
            (Some(false), Some(false), Some(BaseKeymapContent::Emacs))
        );

        Mode::write(&mut content, Some(Mode::Normal));
        assert_eq!(written(&content), (Some(false), Some(false), None));

        let mut content = settings(None, None, None);
        Mode::write(&mut content, Some(Mode::Normal));
        assert_eq!(written(&content), (None, None, None));

        let mut content = settings(None, None, None);
        Mode::write(&mut content, Some(Mode::Emacs));
        assert_eq!(
            written(&content),
            (None, None, Some(BaseKeymapContent::Emacs))
        );
    }

    #[test]
    fn leaving_emacs_restores_the_default_keymap_but_keeps_any_other() {
        use EditingModeContent as Mode;

        let mut content = settings(None, None, Some(BaseKeymapContent::Emacs));
        Mode::write(&mut content, Some(Mode::Vim));
        assert_eq!(written(&content), (Some(true), None, None));

        let mut content = settings(None, None, Some(BaseKeymapContent::Emacs));
        Mode::write(&mut content, Some(Mode::Helix));
        assert_eq!(written(&content), (None, Some(true), None));

        let mut content = settings(None, None, Some(BaseKeymapContent::JetBrains));
        Mode::write(&mut content, Some(Mode::Vim));
        assert_eq!(
            written(&content),
            (Some(true), None, Some(BaseKeymapContent::JetBrains))
        );
        Mode::write(&mut content, Some(Mode::Normal));
        assert_eq!(
            written(&content),
            (Some(false), None, Some(BaseKeymapContent::JetBrains))
        );

        // Emacs replaces the base keymap: it is one.
        Mode::write(&mut content, Some(Mode::Emacs));
        assert_eq!(
            written(&content),
            (Some(false), None, Some(BaseKeymapContent::Emacs))
        );
    }

    #[test]
    fn writing_none_removes_what_the_choice_wrote() {
        use EditingModeContent as Mode;

        let mut content = settings(Some(true), Some(false), Some(BaseKeymapContent::Emacs));
        Mode::write(&mut content, None);
        assert_eq!(written(&content), (None, None, None));
        assert_eq!(Mode::read(&content), None);

        let mut content = settings(Some(true), None, Some(BaseKeymapContent::SublimeText));
        Mode::write(&mut content, None);
        assert_eq!(
            written(&content),
            (None, None, Some(BaseKeymapContent::SublimeText))
        );
    }

    #[test]
    fn every_mode_written_reads_back() {
        for start in [
            settings(None, None, None),
            settings(Some(true), Some(false), Some(BaseKeymapContent::Emacs)),
            settings(Some(false), Some(true), Some(BaseKeymapContent::VSCode)),
        ] {
            for mode in <EditingModeContent as strum::VariantArray>::VARIANTS {
                let mut content = start.clone();
                EditingModeContent::write(&mut content, Some(*mode));
                let expected = if *mode == EditingModeContent::Normal
                    && content.vim_mode.is_none()
                    && content.helix_mode.is_none()
                {
                    None
                } else {
                    Some(*mode)
                };
                assert_eq!(
                    EditingModeContent::read(&content),
                    expected,
                    "{mode:?} from {start:?}"
                );
                assert_eq!(
                    EditingModeContent::from_resolved(
                        content.vim_mode.unwrap_or(false),
                        content.helix_mode.unwrap_or(false),
                        content.base_keymap == Some(BaseKeymapContent::Emacs),
                    ),
                    *mode,
                    "{mode:?} from {start:?}"
                );
            }
        }
    }

    #[test]
    fn every_variant_has_exactly_one_label() {
        assert_eq!(
            <EditingModeContent as strum::VariantNames>::VARIANTS,
            &["Normal", "Vim", "Helix", "Emacs"]
        );
    }
}
