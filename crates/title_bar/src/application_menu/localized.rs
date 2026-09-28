use gpui::Context;
use ui::PopoverMenuHandle;

use super::{ApplicationMenu, MenuEntry};

impl ApplicationMenu {
    /// The menu bar keeps its own copy of the platform menus, which Zed replaces with
    /// menus in the new language whenever the UI language changes; read them again
    /// once the catalog has been replaced.
    ///
    /// Called while rendering: the menus are replaced by an observer of the language
    /// change, which runs before the next frame is drawn.
    pub(super) fn reload_menus_if_language_changed(&mut self, cx: &mut Context<Self>) {
        let generation = i18n::generation();
        if self.menus_i18n_generation == generation {
            return;
        }
        self.menus_i18n_generation = generation;
        for entry in &self.entries {
            entry.handle.hide(cx);
        }
        self.entries = cx
            .get_menus()
            .unwrap_or_default()
            .into_iter()
            .map(|menu| MenuEntry {
                menu,
                handle: PopoverMenuHandle::default(),
            })
            .collect();
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn menu_names(&self) -> Vec<gpui::SharedString> {
        self.entries
            .iter()
            .map(|entry| entry.menu.name.clone())
            .collect()
    }
}

/// The menus are named in the UI language, while `OpenApplicationMenu` in a keymap
/// may name one in English ("File") or as it is shown.
#[cfg(any(test, not(target_os = "macos")))]
pub(super) fn shown_menu_name(name: &str) -> String {
    i18n::tr_ctx(i18n::context::MENU, name).to_string()
}

#[cfg(any(test, feature = "test-support"))]
impl crate::TitleBar {
    /// The names on the Windows and Linux menu bar, or `None` where the platform
    /// draws the menus.
    pub fn application_menu_names(&self, cx: &gpui::App) -> Option<Vec<gpui::SharedString>> {
        self.application_menu
            .as_ref()
            .map(|menu| menu.read(cx).menu_names())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "macos"))]
    use super::super::OpenApplicationMenu;
    use super::*;
    use gpui::{Menu, MenuItem, TestAppContext, VisualTestContext};
    use i18n::test_support::LocaleOverride;
    gpui::actions!(application_menu_test, [NoOp]);

    fn set_menus(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let menu = |name| i18n::tr_ctx(i18n::context::MENU, name);
            cx.set_menus([
                Menu::new(menu("File")).items([MenuItem::action(menu("Save"), NoOp)]),
                Menu::new(menu("Edit")).items([MenuItem::action(menu("Undo"), NoOp)]),
            ])
        });
    }

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    fn names(
        menu: &gpui::Entity<ApplicationMenu>,
        cx: &mut VisualTestContext,
    ) -> Vec<gpui::SharedString> {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        menu.read_with(cx, |menu, _| menu.menu_names())
    }

    #[gpui::test]
    fn menu_bar_reads_the_menus_again_after_the_language_changes(cx: &mut TestAppContext) {
        let _installs = i18n::test_support::ThreadLocalInstalls::new();
        init(cx);
        set_menus(cx);
        let (menu, cx) = cx.add_window_view(|window, cx| ApplicationMenu::new(window, cx));
        assert_eq!(names(&menu, cx), ["File", "Edit"]);

        let japanese = i18n_catalog::test_support::bundled_catalog("ja");
        for (catalog, expected) in [
            (Some(japanese), ["ファイル", "編集"]),
            (None, ["File", "Edit"]),
            (Some(japanese), ["ファイル", "編集"]),
        ] {
            i18n::install(catalog.map(|catalog| catalog as &'static dyn i18n::Catalog));
            set_menus(cx);
            assert_eq!(names(&menu, cx), expected);
        }

        #[cfg(not(target_os = "macos"))]
        for requested in ["File", "ファイル"] {
            menu.update_in(cx, |menu, window, cx| {
                menu.open_menu(&OpenApplicationMenu(requested.to_string()), window, cx);
                assert_eq!(menu.pending_menu_open.as_deref(), Some("ファイル"));
                assert!(
                    menu.entries
                        .iter()
                        .any(|entry| menu.pending_menu_open.as_deref()
                            == Some(entry.menu.name.as_ref())),
                    "{requested:?} names a menu on the bar"
                );
                menu.pending_menu_open = None;
            });
        }
    }

    #[gpui::test]
    fn menus_do_not_reload_while_the_language_stays(cx: &mut TestAppContext) {
        init(cx);
        set_menus(cx);
        let (menu, cx) = cx.add_window_view(|window, cx| ApplicationMenu::new(window, cx));
        assert_eq!(names(&menu, cx), ["File", "Edit"]);
        // Menus set without a language change (a keymap reload) are picked up by
        // upstream's own paths, not by this one.
        cx.update(|_, cx| cx.set_menus([Menu::new("Other")]));
        assert_eq!(names(&menu, cx), ["File", "Edit"]);
    }

    #[test]
    fn open_application_menu_accepts_english_and_translated_names() {
        let _japanese =
            LocaleOverride::new(Some(i18n_catalog::test_support::bundled_catalog("ja")));
        assert_eq!(shown_menu_name("File"), "ファイル");
        assert_eq!(shown_menu_name("ファイル"), "ファイル");
        drop(_japanese);
        let _english = LocaleOverride::new(None);
        assert_eq!(shown_menu_name("File"), "File");
    }
}
