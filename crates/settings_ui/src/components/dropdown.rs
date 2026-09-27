use std::rc::Rc;

use gpui::{App, ElementId, IntoElement, RenderOnce, SharedString};
use heck::ToTitleCase as _;
use ui::{
    ButtonSize, ContextMenu, Disableable as _, DropdownMenu, DropdownStyle, FluentBuilder as _,
    IconPosition, px,
};

#[derive(IntoElement)]
pub struct EnumVariantDropdown<T>
where
    T: strum::VariantArray + strum::VariantNames + Copy + PartialEq + Send + Sync + 'static,
{
    id: ElementId,
    current_value: T,
    variants: &'static [T],
    labels: &'static [&'static str],
    should_do_title_case: bool,
    tab_index: Option<isize>,
    disabled: bool,
    aria_label: Option<SharedString>,
    aria_description: Option<SharedString>,
    on_change: Rc<dyn Fn(T, &mut ui::Window, &mut App) + 'static>,
}

impl<T> EnumVariantDropdown<T>
where
    T: strum::VariantArray + strum::VariantNames + Copy + PartialEq + Send + Sync + 'static,
{
    pub fn new(
        id: impl Into<ElementId>,
        current_value: T,
        variants: &'static [T],
        labels: &'static [&'static str],
        on_change: impl Fn(T, &mut ui::Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            current_value,
            variants,
            labels,
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

impl<T> RenderOnce for EnumVariantDropdown<T>
where
    T: strum::VariantArray + strum::VariantNames + Copy + PartialEq + Send + Sync + 'static,
{
    fn render(self, window: &mut ui::Window, cx: &mut ui::App) -> impl gpui::IntoElement {
        let current_value_label = self.labels[self
            .variants
            .iter()
            .position(|v| *v == self.current_value)
            .unwrap()];
        let enum_name = enum_context::<T>();

        // The menu is built once per key, so the key has to change with the
        // language for the entries to be rebuilt in it.
        let context_menu_key =
            SharedString::from(format!("{current_value_label}-{}", i18n::current_locale()));
        let context_menu = window.use_keyed_state(context_menu_key, cx, |window, cx| {
            ContextMenu::new(window, cx, move |mut menu, _, _| {
                for (&value, &label) in std::iter::zip(self.variants, self.labels) {
                    let on_change = self.on_change.clone();
                    let current_value = self.current_value;
                    menu = menu.toggleable_entry(
                        display_label(enum_name, label, self.should_do_title_case),
                        value == current_value,
                        IconPosition::End,
                        None,
                        move |window, cx| {
                            on_change(value, window, cx);
                        },
                    );
                }
                menu
            })
        });

        DropdownMenu::new(
            self.id,
            display_label(enum_name, current_value_label, self.should_do_title_case),
            context_menu,
        )
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
    let english = if title_case {
        label.to_title_case()
    } else {
        label.to_string()
    };
    i18n::tr_ctx(enum_name, &english).to_string()
}
