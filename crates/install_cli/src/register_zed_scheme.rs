use gpui::{AsyncApp, actions};

actions!(
    cli,
    [
        /// Registers this application's URL scheme handler.
        RegisterZedScheme
    ]
);

pub async fn register_zed_scheme(cx: &AsyncApp) -> anyhow::Result<()> {
    // zed-jp-enhance: the scheme the app bundle declares, not upstream's, so
    // that an official Zed keeps the links meant for it.
    cx.update(|cx| cx.register_url_scheme(branding::CURRENT.url_scheme))
        .await
}
