use gpui::{App, PromptLevel, Task, Window};

/// Longer names are cut so that the address stays in view in the prompt.
const MAX_SERVER_NAME_CHARS: usize = 64;

/// zed-jp-enhance: a language server (Copilot included, or the remote host
/// relaying one) asks Zed to hand an address to the operating system. Only web
/// addresses are passed on, and only once the user has seen the whole address
/// and which server asked: other schemes launch whatever handler the OS has
/// registered for them, and the browser itself is outside the egress allowlist.
///
/// Resolves to whether the address was opened.
pub fn confirm_and_open_server_link(
    server_name: &str,
    address: &str,
    window: &mut Window,
    cx: &mut App,
) -> Task<bool> {
    let Some(url) = web_address(address) else {
        log::warn!(
            "not opening {address:?} for language server {server_name:?}: only http and https \
             addresses without user information are opened"
        );
        return Task::ready(false);
    };
    // A second prompt in the window would replace the one the user may be
    // answering (such as "save changes?"), cancelling it without their input.
    if window.has_active_prompt() {
        log::warn!(
            "not opening {address:?} for language server {server_name:?}: another prompt is open"
        );
        return Task::ready(false);
    }
    // The parsed form is both shown and opened, so the user approves exactly
    // what reaches the operating system.
    let url = url.to_string();
    let server = server_display_name(server_name);
    let detail = i18n::t!("{server} asks to open this address in your browser:\n\n{url}");
    let answer = window.prompt(
        PromptLevel::Warning,
        i18n::t!("Open a link from a language server?"),
        Some(&detail),
        &[i18n::t!("Open"), i18n::t!("Cancel")],
        cx,
    );
    cx.spawn(async move |cx| {
        let open = answer.await.ok() == Some(0);
        if open {
            cx.update(|cx| cx.open_url(&url));
        }
        open
    })
}

/// The address in the form that is both shown and opened, if it may be opened.
///
/// User information is refused because it reads like the host to a person but
/// is not where the browser goes (`https://trusted.example@evil.example/`).
/// The serialized form is ASCII only: the host is in Punycode, and control,
/// bidirectional and other non-ASCII characters are percent-encoded or
/// removed, so nothing in it can disguise the host or add lines to the prompt.
fn web_address(address: &str) -> Option<url::Url> {
    let url = url::Url::parse(address).ok()?;
    let is_web = matches!(url.scheme(), "http" | "https");
    let has_user_information = !url.username().is_empty() || url.password().is_some();
    (is_web && url.host_str().is_some() && !has_user_information).then_some(url)
}

/// Server names come from extensions and from remote hosts, so they are kept
/// to one short line that cannot pose as part of the message.
fn server_display_name(server_name: &str) -> String {
    let single_line: String = server_name
        .chars()
        .map(|character| {
            if character.is_control() || is_bidirectional_control(character) {
                ' '
            } else {
                character
            }
        })
        .collect();
    let trimmed = single_line.trim();
    if trimmed.is_empty() {
        return i18n::t!("A language server").to_string();
    }
    if trimmed.chars().count() > MAX_SERVER_NAME_CHARS {
        let mut shortened: String = trimmed.chars().take(MAX_SERVER_NAME_CHARS).collect();
        shortened.push('…');
        shortened
    } else {
        trimmed.to_string()
    }
}

fn is_bidirectional_control(character: char) -> bool {
    matches!(
        character,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn prompt_shows_and_opens_the_normalized_address(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let opened = cx.update(|window, cx| {
            confirm_and_open_server_link(
                "some
server",
                " HTTP://Example.COM:80/a/../login
?next=1 ",
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let (_, detail) = cx
            .pending_prompt()
            .expect("a web address should be confirmed");
        assert_eq!(
            detail,
            "some server asks to open this address in your browser:

http://example.com/login?next=1"
        );
        cx.simulate_prompt_answer("Open");
        assert!(opened.await);
        assert_eq!(
            cx.opened_url(),
            Some("http://example.com/login?next=1".to_string())
        );
    }

    fn shown(address: &str) -> Option<String> {
        web_address(address).map(|url| url.to_string())
    }

    #[test]
    fn web_addresses_are_normalized_before_they_are_shown() {
        for (address, expected) in [
            (
                "https://github.com/login/device",
                "https://github.com/login/device",
            ),
            ("HTTP://Example.COM/Path", "http://example.com/Path"),
            (
                "  https://example.com/a\u{1}b  ",
                "https://example.com/a%01b",
            ),
            (
                "https://example.com/a\n\nZed trusts this address",
                "https://example.com/aZed%20trusts%20this%20address",
            ),
            ("http:\\\\example.com\\path", "http://example.com/path"),
            ("https://bücher.example/", "https://xn--bcher-kva.example/"),
            (
                "https://example.com/\u{202E}moc.elgoog",
                "https://example.com/%E2%80%AEmoc.elgoog",
            ),
            (
                "https://example.com/?q=日本#片",
                "https://example.com/?q=%E6%97%A5%E6%9C%AC#%E7%89%87",
            ),
        ] {
            assert_eq!(shown(address).as_deref(), Some(expected), "{address:?}");
        }
    }

    #[test]
    fn other_addresses_are_refused() {
        for address in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "file:///C:/Windows/System32/calc.exe",
            "data:text/html,<script>alert(1)</script>",
            "vscode://file/c:/x",
            "zed://settings",
            "ms-settings:privacy",
            "ftp://example.com/file",
            "example.com",
            "//example.com/path",
            "",
            "https://",
            "https://user:pass@example.com/",
            "https://trusted.example@evil.example/",
            "https://:secret@example.com/",
            "http://127.0.0.1@evil.example/",
        ] {
            assert_eq!(shown(address), None, "{address:?}");
        }
    }

    #[test]
    fn every_shown_address_is_one_ascii_line() {
        let long_path = "a".repeat(10_000);
        for address in [
            format!("https://example.com/{long_path}"),
            "https://example.com/\r\nsecond line".to_string(),
            "https://example.com/\u{2028}\u{2029}\u{85}".to_string(),
            "https://ｅｘａｍｐｌｅ.com/".to_string(),
        ] {
            let shown = shown(&address).expect("a web address");
            assert!(
                shown.is_ascii() && !shown.chars().any(|c| c.is_control() || c == ' '),
                "{shown:?}"
            );
        }
    }

    #[test]
    fn server_names_are_one_short_line() {
        assert_eq!(server_display_name("copilot"), "copilot");
        assert_eq!(server_display_name(" rust-analyzer\n"), "rust-analyzer");
        assert_eq!(
            server_display_name("evil\n\nhttps://github.com/login/device\n\n"),
            "evil  https://github.com/login/device"
        );
        assert_eq!(server_display_name("a\u{202E}b\u{2066}c"), "a b c");
        assert_eq!(server_display_name(""), "A language server");
        assert_eq!(server_display_name("\n\t "), "A language server");
        let long = "日".repeat(MAX_SERVER_NAME_CHARS + 1);
        let shown = server_display_name(&long);
        assert_eq!(shown.chars().count(), MAX_SERVER_NAME_CHARS + 1);
        assert!(shown.ends_with('…'));
        assert_eq!(
            server_display_name(&"日".repeat(MAX_SERVER_NAME_CHARS)),
            "日".repeat(MAX_SERVER_NAME_CHARS)
        );
    }
}
