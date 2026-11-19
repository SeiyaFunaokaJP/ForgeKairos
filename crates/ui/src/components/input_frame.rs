use crate::prelude::*;

/// Vertical footprint of an [`input_frame`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum InputFrameSize {
    /// Matches the settings UI text fields (`h_8`).
    #[default]
    Default,
    /// For dense rows such as toolbars or table cells (`h_7`).
    Compact,
}

/// Frame for an `Editor` (or any element) so a text input reads as an input:
/// border, editor background and a focus ring.
///
/// The `ui` crate cannot depend on `editor`, so the caller passes the focus state.
///
/// ```ignore
/// input_frame(self.host.focus_handle(cx).contains_focused(window, cx), cx)
///     .w(rems(13.))
///     .child(self.host.clone())
/// ```
pub fn input_frame(is_focused: bool, cx: &App) -> Div {
    input_frame_sized(InputFrameSize::Default, is_focused, cx)
}

/// [`input_frame`] with an explicit [`InputFrameSize`].
pub fn input_frame_sized(size: InputFrameSize, is_focused: bool, cx: &App) -> Div {
    let colors = cx.theme().colors();
    let border_color = if is_focused {
        colors.border_focused
    } else {
        colors.border
    };

    h_flex()
        .min_w_0()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(border_color)
        .bg(colors.editor_background)
        .map(|this| match size {
            InputFrameSize::Default => this.h_8().px_2().py_1(),
            InputFrameSize::Compact => this.h_7().px_1p5(),
        })
}

/// [`input_frame`] that turns its border red while the value is invalid.
pub fn input_frame_with_status(is_focused: bool, has_error: bool, cx: &App) -> Div {
    let frame = input_frame(is_focused, cx);
    if has_error {
        frame.border_color(cx.theme().status().error_border)
    } else {
        frame
    }
}

/// Tab and Shift+Tab between the text fields of a dialog or form.
///
/// The fields' own editors bind Tab to indenting, so a form adds
/// [`form::KEY_CONTEXT`] to its root's key context (for example
/// `.key_context("QuickConnect FormFields")`), handles the two actions, and
/// passes its fields in visual order to [`form::focus_adjacent_field`]. The
/// keymaps bind Tab inside that context.
pub mod form {
    use gpui::{App, FocusHandle, Window, actions};

    actions!(
        form,
        [
            /// Moves to the next input field.
            FocusNextField,
            /// Moves to the previous input field.
            FocusPreviousField,
        ]
    );

    pub const KEY_CONTEXT: &str = "FormFields";

    /// Moves focus one field along `fields`, wrapping at both ends.
    ///
    /// Deliberately not `window.focus_next`: that walks the window's tab-stop
    /// map, which status bar buttons join too, and it runs off the end of the
    /// form instead of wrapping back to the first field.
    ///
    /// Fields that `root` did not render in the last frame are skipped, so a
    /// form can pass every field it owns, hidden sections included.
    pub fn focus_adjacent_field(
        root: &FocusHandle,
        fields: &[FocusHandle],
        forward: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let visible = fields
            .iter()
            .filter(|field| root.contains(field, window))
            .collect::<Vec<_>>();
        let Some(last) = visible.len().checked_sub(1) else {
            return;
        };
        let focused = visible
            .iter()
            .position(|field| field.contains_focused(window, cx));
        let target = match focused {
            Some(index) if forward => (index + 1) % visible.len(),
            Some(index) => (index + last) % visible.len(),
            None if forward => 0,
            None => last,
        };
        if let Some(field) = visible.get(target) {
            window.focus(field, cx);
        }
    }
}
