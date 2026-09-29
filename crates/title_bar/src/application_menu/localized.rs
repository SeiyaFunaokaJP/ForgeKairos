use gpui::{Context, OwnedMenu, OwnedMenuItem};
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
        self.reload_menus_if_replaced(cx);
    }

    /// Zed rebuilds the menus from settings other than `disable_ai` (the fork's
    /// `agent.enabled` hides the Agent Panel item), so compare the menus themselves
    /// rather than the settings that feed them.
    ///
    /// Zed replaces the menus from its own observer of the settings, which may run
    /// after this view's observer; comparing on the next effect sees its menus.
    pub(super) fn reload_menus_after_settings_change(&mut self, cx: &mut Context<Self>) {
        self.reload_menus_if_replaced(cx);
        let application_menu = cx.weak_entity();
        cx.defer(move |cx| {
            // The title bar may be gone by the time this runs; nothing to refresh then.
            application_menu
                .update(cx, |application_menu, cx| {
                    if application_menu.reload_menus_if_replaced(cx) {
                        cx.notify();
                    }
                })
                .ok();
        });
    }

    fn reload_menus_if_replaced(&mut self, cx: &mut Context<Self>) -> bool {
        let menus = cx.get_menus().unwrap_or_default();
        if self.entries.len() == menus.len()
            && self
                .entries
                .iter()
                .zip(&menus)
                .all(|(entry, menu)| same_menu(&entry.menu, menu))
        {
            return false;
        }
        for entry in &self.entries {
            if entry.handle.is_deployed() {
                entry.handle.hide(cx);
            }
        }
        self.entries = menus
            .into_iter()
            .map(|menu| MenuEntry {
                menu,
                handle: PopoverMenuHandle::default(),
            })
            .collect();
        true
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn menu_names(&self) -> Vec<gpui::SharedString> {
        self.entries
            .iter()
            .map(|entry| entry.menu.name.clone())
            .collect()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn menu_item_names(&self, menu_name: &str) -> Option<Vec<String>> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.menu.name == menu_name)?;
        Some(
            entry
                .menu
                .items
                .iter()
                .filter_map(|item| match item {
                    OwnedMenuItem::Action { name, .. } => Some(name.clone()),
                    _ => None,
                })
                .collect(),
        )
    }
}

fn same_menu(shown: &OwnedMenu, current: &OwnedMenu) -> bool {
    shown.name == current.name
        && shown.disabled == current.disabled
        && shown.items.len() == current.items.len()
        && shown
            .items
            .iter()
            .zip(&current.items)
            .all(|(shown, current)| same_menu_item(shown, current))
}

fn same_menu_item(shown: &OwnedMenuItem, current: &OwnedMenuItem) -> bool {
    match (shown, current) {
        (OwnedMenuItem::Separator, OwnedMenuItem::Separator) => true,
        (OwnedMenuItem::Submenu(shown), OwnedMenuItem::Submenu(current)) => {
            same_menu(shown, current)
        }
        (OwnedMenuItem::SystemMenu(shown), OwnedMenuItem::SystemMenu(current)) => {
            shown.name == current.name
        }
        (
            OwnedMenuItem::Action {
                name: shown_name,
                action: shown_action,
                checked: shown_checked,
                disabled: shown_disabled,
                ..
            },
            OwnedMenuItem::Action {
                name: current_name,
                action: current_action,
                checked: current_checked,
                disabled: current_disabled,
                ..
            },
        ) => {
            shown_name == current_name
                && shown_checked == current_checked
                && shown_disabled == current_disabled
                && shown_action.partial_eq(current_action.as_ref())
        }
        _ => false,
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

    /// The action items of one menu on the Windows and Linux menu bar.
    pub fn application_menu_item_names(
        &self,
        menu_name: &str,
        cx: &gpui::App,
    ) -> Option<Vec<String>> {
        self.application_menu
            .as_ref()
            .and_then(|menu| menu.read(cx).menu_item_names(menu_name))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "macos"))]
    use super::super::OpenApplicationMenu;
    use super::*;
    use gpui::{Menu, MenuItem, TestAppContext, UpdateGlobal as _, VisualTestContext};
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

    fn item_names(menu: &gpui::Entity<ApplicationMenu>, cx: &mut VisualTestContext) -> Vec<String> {
        menu.read_with(cx, |menu, _| menu.menu_item_names("View"))
            .expect("the View menu is on the bar")
    }

    /// Zed rebuilds the menus from settings it reads itself (the fork's
    /// `agent.enabled`), from an observer that may run after the menu bar's.
    #[gpui::test]
    fn menu_bar_follows_menus_replaced_after_a_settings_change(cx: &mut TestAppContext) {
        init(cx);
        let view_menu = |items: &[&'static str]| {
            Menu::new("View").items(items.iter().map(|item| MenuItem::action(*item, NoOp)))
        };
        cx.update(|cx| cx.set_menus([view_menu(&["Agent Panel", "Diagnostics"])]));
        let (menu, cx) = cx.add_window_view(|window, cx| ApplicationMenu::new(window, cx));
        let with_agent_panel = std::rc::Rc::new(std::cell::Cell::new(true));
        let _replaces_menus = cx.update(|_, cx| {
            let with_agent_panel = with_agent_panel.clone();
            cx.observe_global::<settings::SettingsStore>(move |cx| {
                let items: &[&'static str] = if with_agent_panel.get() {
                    &["Agent Panel", "Diagnostics"]
                } else {
                    &["Diagnostics"]
                };
                cx.set_menus([view_menu(items)]);
            })
        });
        assert_eq!(item_names(&menu, cx), ["Agent Panel", "Diagnostics"]);

        for (agent_panel, expected) in [
            (false, &["Diagnostics"][..]),
            (true, &["Agent Panel", "Diagnostics"][..]),
        ] {
            with_agent_panel.set(agent_panel);
            cx.update(|_, cx| settings::SettingsStore::update_global(cx, |_, _| {}));
            cx.run_until_parked();
            assert_eq!(item_names(&menu, cx), expected);
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
