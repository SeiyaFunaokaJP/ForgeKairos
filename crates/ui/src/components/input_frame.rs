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
