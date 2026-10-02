use std::rc::Rc;

use gpui::{App, ElementId, IntoElement, RenderOnce, SharedString};
use heck::ToTitleCase as _;
use ui::{
    ButtonSize, ContextMenu, Disableable as _, DropdownMenu, DropdownStyle, FluentBuilder as _,
    IconPosition, px,
};

#[derive(IntoElement)]
pub struct EnumVariantDropdown {
    id: ElementId,
    selected_index: usize,
    labels: &'static [&'static str],
    /// The msgctxt of the labels (`enum_context`). `new_indexed` has no enum
    /// to name, so its labels stay in English rather than borrow a
    /// translation made for another enum.
    enum_name: Option<&'static str>,
    should_do_title_case: bool,
    tab_index: Option<isize>,
    disabled: bool,
    aria_label: Option<SharedString>,
    aria_description: Option<SharedString>,
    on_change: Rc<dyn Fn(usize, &mut ui::Window, &mut App) + 'static>,
}

impl EnumVariantDropdown {
    pub fn new<T>(
        id: impl Into<ElementId>,
        current_value: T,
        variants: &'static [T],
        labels: &'static [&'static str],
        on_change: impl Fn(T, &mut ui::Window, &mut App) + 'static,
    ) -> Self
    where
        T: strum::VariantArray + strum::VariantNames + Copy + PartialEq + Send + Sync + 'static,
    {
        let selected_index = variants
            .iter()
            .position(|v| *v == current_value)
            .unwrap_or(0);
        let mut dropdown =
            Self::new_indexed(id, selected_index, labels, move |index, window, cx| {
                on_change(variants[index], window, cx)
            });
        dropdown.enum_name = Some(enum_context::<T>());
        dropdown
    }

    pub fn new_indexed(
        id: impl Into<ElementId>,
        selected_index: usize,
        labels: &'static [&'static str],
        on_change: impl Fn(usize, &mut ui::Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            selected_index,
            labels,
            enum_name: None,
            should_do_title_case: true,
            tab_index: None,
            disabled: false,
            aria_label: None,
            aria_description: None,
            on_change: Rc::new(on_change),
        }
    }

    pub fn title_case(mut self, title_case: bool) -> Self {
        self.should_do_title_case = title_case;
        self
    }

    pub fn tab_index(mut self, tab_index: isize) -> Self {
        self.tab_index = Some(tab_index);
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Sets the label announced by assistive technology.
    /// Defaults to the currently selected value's label.
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }

    /// Sets the supplementary description announced by assistive technology
    /// after the combobox's name, role, and value (e.g. a setting subtitle).
    pub fn aria_description(mut self, description: impl Into<SharedString>) -> Self {
        self.aria_description = Some(description.into());
        self
    }
}

impl RenderOnce for EnumVariantDropdown {
    fn render(self, window: &mut ui::Window, cx: &mut ui::App) -> impl gpui::IntoElement {
        let current_value_label = self.labels[self.selected_index];
        let enum_name = self.enum_name;
        let title_case = self.should_do_title_case;
        let label_text = move |label: &str| match enum_name {
            Some(enum_name) => display_label(enum_name, label, title_case),
            None if title_case => label.to_title_case(),
            None => label.to_string(),
        };

        // The menu is built once per key, so the key has to change with the
        // language for the entries to be rebuilt in it.
        let context_menu_key =
            SharedString::from(format!("{current_value_label}-{}", i18n::current_locale()));
        let context_menu = window.use_keyed_state(context_menu_key, cx, |window, cx| {
            ContextMenu::new(window, cx, move |mut menu, _, _| {
                for (index, &label) in self.labels.iter().enumerate() {
                    let on_change = self.on_change.clone();
                    menu = menu.toggleable_entry(
                        label_text(label),
                        index == self.selected_index,
                        IconPosition::End,
                        None,
                        move |window, cx| {
                            on_change(index, window, cx);
                        },
                    );
                }
                menu
            })
        });

        DropdownMenu::new(self.id, label_text(current_value_label), context_menu)
            .when_some(self.aria_label, |this, label| this.aria_label(label))
            .when_some(self.aria_description, |this, description| {
                this.aria_description(description)
            })
            .disabled(self.disabled)
            .when_some(self.tab_index, |elem, tab_index| elem.tab_index(tab_index))
            .trigger_size(ButtonSize::Medium)
            .style(DropdownStyle::Outlined)
            .offset(gpui::Point {
                x: px(0.0),
                y: px(2.0),
            })
            .into_any_element()
    }
}

/// The msgctxt under which the labels of `T` are translated: the enum's name, so
/// that two enums sharing an English label ("Default", "None") can read differently.
pub(crate) fn enum_context<T>() -> &'static str {
    let name = std::any::type_name::<T>();
    name.rsplit("::").next().unwrap_or(name)
}

/// What the dropdown shows for a strum label: the English as upstream draws it,
/// translated only by an entry made for `enum_name`. Without the context, "No"
/// or "Selection" would take the translation of the same English from anywhere
/// else in the catalog, whatever it means there.
pub(crate) fn display_label(enum_name: &str, label: &str, title_case: bool) -> String {
    if let Some(name) = base_keymap_option_name(enum_name, label) {
        return name.to_string();
    }
    let english = if title_case {
        label.to_title_case()
    } else {
        label.to_string()
    };
    i18n::tr_ctx(enum_name, &english).to_string()
}

/// The strum names of the base keymaps are upstream's spellings ("Zed",
/// "VSCode"); the window shows the names the onboarding picker gives the same
/// keymaps, so the application's own keymap is called by the product's name in
/// both places. The stored value is the variant, not this label.
fn base_keymap_option_name(enum_name: &str, label: &str) -> Option<&'static str> {
    type Content = settings::BaseKeymapContent;
    if enum_name != enum_context::<Content>() {
        return None;
    }
    let index = <Content as strum::VariantNames>::VARIANTS
        .iter()
        .position(|name| *name == label)?;
    let variant = <Content as strum::VariantArray>::VARIANTS.get(index)?;
    settings::BaseKeymap::from(*variant).option_name()
}
