use gpui::{IntoElement, ParentElement};
use ui::{Banner, prelude::*};

#[derive(IntoElement)]
pub struct YoungAccountBanner;

impl RenderOnce for YoungAccountBanner {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        const YOUNG_ACCOUNT_DISCLAIMER: &str = "サービスの不正利用を防ぐため、作成から 30 日未満の GitHub アカウントは Pro トライアルの対象外です。例外の適用を希望する場合は billing-support@zed.dev までご連絡ください。";

        let label = div()
            .w_full()
            .text_sm()
            .text_color(cx.theme().colors().text_muted)
            .child(YOUNG_ACCOUNT_DISCLAIMER);

        div()
            .max_w_full()
            .my_1()
            .child(Banner::new().severity(Severity::Warning).child(label))
    }
}
