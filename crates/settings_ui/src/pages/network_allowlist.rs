//! Settings page for the egress allowlist (`.forgekairos-docs/DESIGN.md` §4,
//! ADR-F03).
//!
//! Since ADR-F38 the list governs AI-originated traffic only — the IDE's own
//! tooling is not gated by it — which is what the page has to explain.
//!
//! Unlike every other page in this crate, the values here do not live in
//! `settings.json`: they are the `allowed_hosts` array and the
//! `proxy_child_processes` flag (ADR-F14) of
//! `%APPDATA%\Zed\network_allowlist.json`, which `egress_policy` reads and
//! watches on behalf of `http_client::AllowlistedHttpClient` and of the
//! loopback proxy it points agent child processes at. That watch is what
//! applies an edit made here, so this page only has to leave the file in the
//! right shape — there is no in-process policy handle to poke.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use egress_policy::NetworkAllowlistFile;
use fs::Fs;
use gpui::{ScrollHandle, Task, prelude::*};
use ui::{Banner, Divider, Severity, SwitchField, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;

use crate::SettingsWindow;
use crate::components::{SettingsInputField, SettingsSectionHeader};

const HOSTS_DESCRIPTION: &str = i18n::msg!(
    "Each entry is an exact host name (api.anthropic.com), a subdomain wildcard with a leading *. (*.corp.example, which does not match the parent domain itself), or an IP address (10.0.0.5 / [fd00::1]). Entries are case-insensitive and take no port."
);

/// See `.forgekairos-docs/DESIGN.md` §4.14 (ADR-F14) for what this actually covers.
const PROXY_CHILD_PROCESSES_DESCRIPTION: &str = i18n::msg!(
    "When enabled, each child process for ACP agents, MCP servers, and the npm that installs AI agents is given a loopback proxy, so its outbound traffic is also checked against the allowlist above. Enabled by default. When disabled, outbound traffic from these child processes is outside the allowlist's control (AI traffic from the IDE itself stays controlled)."
);

const PROXY_CHILD_PROCESSES_NOTE: &str = i18n::msg!(
    "Changes take effect from the next child process that starts (no restart needed). Processes that ignore proxy settings cannot be controlled even when this is enabled. Child processes unrelated to AI, such as npm used by language servers or extensions, are not covered even when this is enabled."
);

/// Written when the file has to be created before it can be opened in the
/// editor, so the user lands on something they can edit rather than a blank
/// buffer. Upstream's name in it is the product's own once translated (the
/// English catalog included).
const FILE_TEMPLATE_HEADER: &str = i18n::msg!(
    "\
// Zed egress allowlist (.forgekairos-docs/DESIGN.md §4)
//
// Outbound traffic that AI initiates (AI providers, agent tools, MCP servers,
// external agent processes) is allowed only to the hosts written here.
// Traffic from the IDE's own tooling, such as language servers and extensions, is not covered.
// 127.0.0.1 / ::1 / localhost are always allowed without being written here.
// You can use // comments and trailing commas. Changes take effect when you save.
"
);

/// Kept out of the translated header: a catalog message is checked as a format
/// string, and the braces of the JSON would read as a placeholder.
const FILE_TEMPLATE_BODY: &str = "{\n  \"allowed_hosts\": []\n}\n";

fn file_template() -> String {
    format!("{}{FILE_TEMPLATE_BODY}", i18n::tr(FILE_TEMPLATE_HEADER))
}

/// What the settings window knows about `network_allowlist.json`.
pub(crate) struct NetworkAllowlistState {
    status: Status,
    /// The `allowed_hosts` entries exactly as they appear in the file, in file
    /// order. Editing matches on the index, so this must stay the literal list
    /// rather than a canonicalized or filtered one.
    entries: Vec<String>,
    /// See [`NetworkAllowlistFile::proxy_child_processes`]. Starts out `true`
    /// so the toggle never reads as off in the window before the first read of
    /// the file lands, which is also what an absent key means.
    proxy_child_processes: bool,
    /// Shown in a banner until it is dismissed or the next operation clears it.
    error: Option<SharedString>,
    /// Whether `error` belongs to a write the external-AI consent flow asked
    /// for. Only that flow shows it, so an invalid host typed on this page does
    /// not light up every provider section on the AI pages.
    error_from_consent: bool,
    /// What the last consent-driven allowlist change did, and the consent key it
    /// was made for. Keyed rather than global so that revoking one provider does
    /// not print its note under every other provider's card.
    consent_notice: Option<(SharedString, SharedString)>,
    /// At most one file operation is in flight; each new one awaits its
    /// predecessor instead of replacing it, so two quick edits cannot land out
    /// of order.
    task: Option<Task<()>>,
}

impl Default for NetworkAllowlistState {
    fn default() -> Self {
        Self {
            status: Status::default(),
            entries: Vec::new(),
            proxy_child_processes: true,
            error: None,
            error_from_consent: false,
            consent_notice: None,
            task: None,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
enum Status {
    #[default]
    Loading,
    /// No file on disk. `egress_policy` treats that as an empty policy, so the
    /// page shows an empty list rather than an error.
    Missing,
    Present,
    /// The file exists but could not be read or parsed. Editing is disabled in
    /// this state: rewriting the file would discard whatever is in it.
    Unusable(SharedString),
}

impl NetworkAllowlistState {
    fn is_editable(&self) -> bool {
        matches!(self.status, Status::Missing | Status::Present)
    }

    /// Whether the file has been read at least once. Callers outside this page
    /// (the external-AI consent flow) render "not registered" only once this is
    /// true, so an unread file is never reported as an empty allowlist.
    pub(crate) fn is_loaded(&self) -> bool {
        self.status != Status::Loading
    }

    /// Whether one of the entries already covers `host`, wildcards included.
    /// Entries that no longer parse are skipped, exactly as `egress_policy`
    /// skips them.
    pub(crate) fn allows(&self, host: &str) -> bool {
        let Some(host) = http_client::RequestHost::parse(host) else {
            return false;
        };
        if host.is_loopback() {
            return true;
        }
        self.entries.iter().any(|entry| {
            http_client::HostPattern::parse(entry).is_ok_and(|pattern| pattern.matches(&host))
        })
    }

    /// Whether one entry names `host` outright, as opposed to merely covering
    /// it. This is what the external-AI consent blocks report as "registered"
    /// for a provider, and the only kind of entry withdrawing consent removes:
    /// a `*.corp.example` covering `api.corp.example` was written for more than
    /// this provider.
    pub(crate) fn lists_host(&self, host: &str) -> bool {
        let Ok(host) = http_client::HostPattern::parse(host) else {
            return false;
        };
        let host = host.to_string();
        self.entries.iter().any(|entry| {
            http_client::HostPattern::parse(entry).is_ok_and(|entry| entry.to_string() == host)
        })
    }

    /// The message from the last failed allowlist write the consent flow asked
    /// for, so that it can be reported where the button was pressed.
    pub(crate) fn consent_error(&self) -> Option<SharedString> {
        self.error.clone().filter(|_| self.error_from_consent)
    }

    /// What the last consent-driven allowlist change did, if it was made for
    /// `key`.
    pub(crate) fn consent_notice(&self, key: &str) -> Option<SharedString> {
        self.consent_notice
            .as_ref()
            .filter(|(notice_key, _)| notice_key.as_ref() == key)
            .map(|(_, notice)| notice.clone())
    }

    /// [`Self::consent_notice`] regardless of which provider it was made for.
    /// Only the acknowledged-providers list uses this: revoking there removes
    /// the row the keyed note would have hung under.
    pub(crate) fn consent_notice_text(&self) -> Option<SharedString> {
        self.consent_notice
            .as_ref()
            .map(|(_, notice)| notice.clone())
    }
}

/// The file's values plus how the file they came from was read.
struct Loaded {
    status: Status,
    entries: Vec<String>,
    proxy_child_processes: bool,
}

enum Operation {
    Reload,
    /// Replace `allowed_hosts` with this list, then read the file back.
    Write(Vec<String>),
    /// Append these hosts to whatever is on disk, skipping ones already
    /// covered. Used by the external-AI consent flow, which cannot assume this
    /// page has been opened, so it must not send a `Write` built from a
    /// possibly-unread `entries`.
    Add(Vec<String>),
    /// Drop these hosts from whatever is on disk, matching whole entries only.
    /// The inverse of `Add`, issued when an external-AI consent is withdrawn
    /// (ADR-F10), and subject to the same "the page may never have been opened"
    /// reasoning.
    Remove(Vec<String>),
    /// Write `proxy_child_processes`, leaving the rest of the file alone.
    SetProxyChildProcesses(bool),
}

/// Reads the policy file into the window's state. Called when the sub page is
/// opened and by the page's own reload button.
pub(crate) fn load_network_allowlist(
    settings_window: &mut SettingsWindow,
    cx: &mut Context<SettingsWindow>,
) {
    run(settings_window, Operation::Reload, cx);
}

fn run(
    settings_window: &mut SettingsWindow,
    operation: Operation,
    cx: &mut Context<SettingsWindow>,
) {
    run_for_consent(settings_window, operation, None, cx);
}

/// [`run`], additionally recording what a consent-driven change did under
/// `notice_key` once the write has landed.
fn run_for_consent(
    settings_window: &mut SettingsWindow,
    operation: Operation,
    notice: Option<(SharedString, SharedString)>,
    cx: &mut Context<SettingsWindow>,
) {
    let fs = <dyn Fs>::global(cx);
    let path = paths::network_allowlist_file().clone();

    // Show the edit right away; the read-back that follows the write is what
    // makes it authoritative.
    match &operation {
        Operation::Write(entries) => {
            settings_window.network_allowlist.entries = entries.clone();
        }
        Operation::SetProxyChildProcesses(enabled) => {
            settings_window.network_allowlist.proxy_child_processes = *enabled;
        }
        Operation::Reload | Operation::Add(_) | Operation::Remove(_) => {}
    }
    settings_window.network_allowlist.error = None;
    settings_window.network_allowlist.error_from_consent =
        matches!(operation, Operation::Add(_) | Operation::Remove(_));
    settings_window.network_allowlist.consent_notice = None;

    // Taking the previous task rather than dropping it keeps an in-flight write
    // alive, and awaiting it serializes the file operations.
    let previous = settings_window.network_allowlist.task.take();
    settings_window.network_allowlist.task = Some(cx.spawn(async move |this, cx| {
        if let Some(previous) = previous {
            previous.await;
        }
        let (loaded, error) = apply(fs, &path, operation).await;
        this.update(cx, |this, cx| {
            this.network_allowlist.status = loaded.status;
            this.network_allowlist.entries = loaded.entries;
            this.network_allowlist.proxy_child_processes = loaded.proxy_child_processes;
            // Only report a failure here; a validation message the user
            // provoked while this was in flight must not be wiped out.
            if let Some(error) = error {
                this.network_allowlist.error = Some(error);
            } else if let Some(notice) = notice {
                this.network_allowlist.consent_notice = Some(notice);
            }
            cx.notify();
        })
        .log_err();
    }));
    cx.notify();
}

async fn apply(
    fs: Arc<dyn Fs>,
    path: &Path,
    operation: Operation,
) -> (Loaded, Option<SharedString>) {
    let mut error = None;
    match operation {
        Operation::Reload => {}
        Operation::Write(entries) => {
            let written = write(&fs, path, |existing| {
                allowed_hosts_replaced(existing, &entries)
            })
            .await;
            if let Err(write_error) = written {
                let write_error = format!("{write_error:#}");
                error = Some(SharedString::from(i18n::t!(
                    "Failed to save the allowlist: {write_error}"
                )));
            }
        }
        Operation::SetProxyChildProcesses(enabled) => {
            let written = write(&fs, path, |existing| {
                proxy_child_processes_replaced(existing, enabled)
            })
            .await;
            if let Err(write_error) = written {
                let write_error = format!("{write_error:#}");
                error = Some(SharedString::from(i18n::t!(
                    "Failed to save the child process proxy setting: {write_error}"
                )));
            }
        }
        // Delegates to the shared read-modify-write in `egress_policy`, the
        // same one the F2-5 install consent flow (`agent_ui`) uses, rather
        // than replacing the in-memory `entries` this page may not have
        // loaded — the caller here (the external-AI consent flow) cannot
        // assume this page has been opened.
        Operation::Add(hosts) => {
            if let Err(write_error) = egress_policy::add_allowed_hosts(&fs, path, &hosts).await {
                let write_error = format!("{write_error:#}");
                error = Some(SharedString::from(i18n::t!(
                    "Failed to save the allowlist: {write_error}"
                )));
            }
        }
        Operation::Remove(hosts) => {
            if let Err(write_error) = egress_policy::remove_allowed_hosts(&fs, path, &hosts).await {
                let write_error = format!("{write_error:#}");
                error = Some(SharedString::from(i18n::t!(
                    "Failed to save the allowlist: {write_error}"
                )));
            }
        }
    }
    // Always read back, even after a failed write, so the list on screen is
    // what is actually on disk.
    (read(&fs, path).await, error)
}

async fn read(fs: &Arc<dyn Fs>, path: &Path) -> Loaded {
    // A file that is absent or unusable is the same policy as one that says
    // nothing: `egress_policy` reads both as loopback-only with the
    // child-process proxy on, so that is what this page shows.
    let denied_by_default = |status| Loaded {
        status,
        entries: Vec::new(),
        proxy_child_processes: true,
    };

    if !fs.is_file(path).await {
        return denied_by_default(Status::Missing);
    }
    let contents = match fs.load(path).await {
        Ok(contents) => contents,
        Err(error) => {
            let error = format!("{error:#}");
            return denied_by_default(Status::Unusable(
                i18n::t!("Failed to read network_allowlist.json: {error}").into(),
            ));
        }
    };
    // An empty file is a valid "allow nothing" policy for `egress_policy`, but
    // not valid JSON, so it is handled before parsing.
    if contents.trim().is_empty() {
        return denied_by_default(Status::Present);
    }
    match settings_json::parse_json_with_comments::<NetworkAllowlistFile>(&contents) {
        Ok(file) => Loaded {
            status: Status::Present,
            entries: file.allowed_hosts,
            proxy_child_processes: file.proxy_child_processes.unwrap_or(true),
        },
        Err(error) => denied_by_default(Status::Unusable(
            i18n::t!("Failed to parse network_allowlist.json: {error}").into(),
        )),
    }
}

/// Rewrites the file through `replace`, which is handed the current contents
/// (empty when there is no file yet) and returns what should stand in their
/// place.
async fn write(
    fs: &Arc<dyn Fs>,
    path: &Path,
    replace: impl FnOnce(&str) -> Result<String>,
) -> Result<()> {
    let exists = fs.is_file(path).await;
    let existing = if exists {
        fs.load(path)
            .await
            .context(i18n::t!("Failed to read network_allowlist.json"))?
    } else {
        String::new()
    };
    // Refuse to rewrite a file that cannot be parsed: the rewrite would throw
    // away whatever the user was in the middle of hand-editing.
    if !existing.trim().is_empty() {
        settings_json::parse_json_with_comments::<NetworkAllowlistFile>(&existing)
            .context(i18n::t!("Failed to parse network_allowlist.json"))?;
    }

    let new_text = replace(&existing)?;

    if let Some(parent) = path.parent() {
        fs.create_dir(parent).await?;
    }
    // `settings.json` is written the same way: to the canonical path, so that
    // the watch `egress_policy` registered (which canonicalizes too) sees it.
    // A path that cannot be canonicalized is still writable under its original
    // spelling.
    let target = if exists {
        fs.canonicalize(path)
            .await
            .unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    fs.atomic_write(target, new_text)
        .await
        .context(i18n::t!("Failed to write network_allowlist.json"))
}

/// Rewrites only the `allowed_hosts` value, so the rest of the file — including
/// the comments users are encouraged to keep there — survives. Comments *inside*
/// the array are part of the replaced value and do not.
fn allowed_hosts_replaced(existing: &str, entries: &[String]) -> Result<String> {
    let value = serde_json::Value::Array(
        entries
            .iter()
            .map(|entry| serde_json::Value::String(entry.clone()))
            .collect(),
    );
    key_replaced(existing, "allowed_hosts", &value)
}

/// Rewrites only the `proxy_child_processes` value. The `true` case is written
/// out rather than dropping the key: an explicit value is what the user asked
/// for, and leaving the file silent would make the toggle look like it had not
/// been touched next to a hand-written `false`.
fn proxy_child_processes_replaced(existing: &str, enabled: bool) -> Result<String> {
    key_replaced(
        existing,
        "proxy_child_processes",
        &serde_json::Value::Bool(enabled),
    )
}

/// Replaces one top-level key's value, creating it when it is absent, and
/// leaving every other byte of the file where it was.
fn key_replaced(existing: &str, key: &str, value: &serde_json::Value) -> Result<String> {
    let indent_size = settings_json::infer_json_indent_size(existing);
    let (range, replacement) =
        settings_json::replace_value_in_json_text(existing, &[key], indent_size, Some(value), None);
    ensure!(
        range.end <= existing.len()
            && existing.is_char_boundary(range.start)
            && existing.is_char_boundary(range.end),
        i18n::t!("Could not determine the range to rewrite")
    );
    let mut text = existing.to_string();
    text.replace_range(range, &replacement);
    Ok(text)
}

/// Validates one entry the way `egress_policy` will, returning the canonical
/// spelling to store or a message to show.
fn canonicalize_host(input: &str) -> Result<String, SharedString> {
    match http_client::HostPattern::parse(input) {
        Ok(pattern) => Ok(pattern.to_string()),
        Err(_) => Err(rejection_reason(input)),
    }
}

/// `HostPattern::parse` reports its reason as prose with no machine-readable
/// discriminant, so the Japanese message is derived from the shape of the input
/// instead. The generic message is the fallback whenever the shape does not
/// explain the rejection.
fn rejection_reason(input: &str) -> SharedString {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return i18n::t!("Enter a host.").into();
    }
    let (has_wildcard, rest) = match trimmed.strip_prefix("*.") {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    if rest.contains('*') {
        return i18n::t!(
            "A wildcard is allowed only as a leading `*.` (for example, *.corp.example)."
        )
        .into();
    }
    if has_wildcard
        && matches!(
            http_client::RequestHost::parse(rest),
            Some(http_client::RequestHost::Ip(_))
        )
    {
        return i18n::t!("`*.` cannot be used with an IP address.").into();
    }
    i18n::t!(
        "Not a valid host name or IP address. Examples: api.anthropic.com, *.corp.example, 10.0.0.5"
    )
    .into()
}

fn add_host(settings_window: &mut SettingsWindow, input: &str, cx: &mut Context<SettingsWindow>) {
    let host = match canonicalize_host(input) {
        Ok(host) => host,
        Err(reason) => {
            settings_window.network_allowlist.error = Some(reason);
            cx.notify();
            return;
        }
    };
    let mut entries = settings_window.network_allowlist.entries.clone();
    if entries.iter().any(|entry| entry == &host) {
        settings_window.network_allowlist.error =
            Some(i18n::t!("{host} is already in the allowlist.").into());
        cx.notify();
        return;
    }
    entries.push(host);
    run(settings_window, Operation::Write(entries), cx);
}

/// Adds hosts to `network_allowlist.json` without needing this page to have
/// been opened first: the write reads the file, appends what is missing and
/// writes it back. Hosts that `egress_policy` would reject are dropped with a
/// message rather than written.
pub(crate) fn add_allowed_hosts(
    settings_window: &mut SettingsWindow,
    hosts: &[SharedString],
    cx: &mut Context<SettingsWindow>,
) {
    let mut canonical = Vec::with_capacity(hosts.len());
    for host in hosts {
        match canonicalize_host(host) {
            Ok(host) => canonical.push(host),
            Err(reason) => {
                settings_window.network_allowlist.error =
                    Some(i18n::t!("Cannot add {host}: {reason}").into());
                settings_window.network_allowlist.error_from_consent = true;
                cx.notify();
                return;
            }
        }
    }
    if canonical.is_empty() {
        return;
    }
    run(settings_window, Operation::Add(canonical), cx);
}

/// Drops hosts from `network_allowlist.json`, the inverse of
/// [`add_allowed_hosts`] and with the same "this page may never have been
/// opened" reasoning: the removal is applied to what is on disk.
///
/// `notice` is what the consent block that asked for this shows once the write
/// lands, keyed by the consent key so it appears under that provider only.
/// Hosts that could never have been written (they do not parse) are dropped
/// here; an empty request writes nothing at all, which is what makes pressing
/// the button twice harmless.
pub(crate) fn remove_allowed_hosts(
    settings_window: &mut SettingsWindow,
    hosts: &[SharedString],
    notice: (SharedString, SharedString),
    cx: &mut Context<SettingsWindow>,
) {
    let canonical: Vec<String> = hosts
        .iter()
        .filter_map(|host| canonicalize_host(host).ok())
        .collect();
    if canonical.is_empty() {
        return;
    }
    run_for_consent(
        settings_window,
        Operation::Remove(canonical),
        Some(notice),
        cx,
    );
}

/// Records a consent-block message that needed no file write — "nothing was
/// removed, because another provider still needs these hosts".
pub(crate) fn set_consent_notice(
    settings_window: &mut SettingsWindow,
    notice: (SharedString, SharedString),
    cx: &mut Context<SettingsWindow>,
) {
    settings_window.network_allowlist.error = None;
    settings_window.network_allowlist.error_from_consent = false;
    settings_window.network_allowlist.consent_notice = Some(notice);
    cx.notify();
}

/// Clears the banner shown for the last failed allowlist file operation, and
/// the note shown for the last successful consent-driven one.
pub(crate) fn dismiss_error(
    settings_window: &mut SettingsWindow,
    cx: &mut Context<SettingsWindow>,
) {
    settings_window.network_allowlist.error = None;
    settings_window.network_allowlist.error_from_consent = false;
    settings_window.network_allowlist.consent_notice = None;
    cx.notify();
}

fn update_host(
    settings_window: &mut SettingsWindow,
    index: usize,
    input: &str,
    cx: &mut Context<SettingsWindow>,
) {
    let host = match canonicalize_host(input) {
        Ok(host) => host,
        Err(reason) => {
            settings_window.network_allowlist.error = Some(reason);
            cx.notify();
            return;
        }
    };
    let mut entries = settings_window.network_allowlist.entries.clone();
    if entries.get(index) == Some(&host) {
        return;
    }
    if entries
        .iter()
        .enumerate()
        .any(|(other, value)| other != index && value == &host)
    {
        settings_window.network_allowlist.error =
            Some(i18n::t!("{host} is already in the allowlist.").into());
        cx.notify();
        return;
    }
    let Some(entry) = entries.get_mut(index) else {
        return;
    };
    *entry = host;
    run(settings_window, Operation::Write(entries), cx);
}

fn remove_host(
    settings_window: &mut SettingsWindow,
    index: usize,
    cx: &mut Context<SettingsWindow>,
) {
    let mut entries = settings_window.network_allowlist.entries.clone();
    if index >= entries.len() {
        return;
    }
    entries.remove(index);
    run(settings_window, Operation::Write(entries), cx);
}

fn set_proxy_child_processes(
    settings_window: &mut SettingsWindow,
    enabled: bool,
    cx: &mut Context<SettingsWindow>,
) {
    // `SwitchField` reports the same new state from both the switch and the row
    // around it, so one click arrives twice; without this the second one would
    // queue a redundant rewrite of the file.
    if settings_window.network_allowlist.proxy_child_processes == enabled {
        return;
    }
    run(
        settings_window,
        Operation::SetProxyChildProcesses(enabled),
        cx,
    );
}

pub(crate) fn render_network_allowlist_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let state = &settings_window.network_allowlist;
    let is_editable = state.is_editable();
    let error = state.error.clone();
    let entry_count = state.entries.len();
    let proxy_child_processes = state.proxy_child_processes;

    let host_rows: Vec<AnyElement> = state
        .entries
        .iter()
        .enumerate()
        .map(|(index, host)| render_host_row(index, host.clone(), cx))
        .collect();
    let empty_border = cx.theme().colors().border_variant;

    v_flex()
        .id("network-allowlist-page")
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_6()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .child(render_intro(cx))
        .when_some(error, |this, error| {
            this.child(
                Banner::new()
                    .severity(Severity::Warning)
                    .child(Label::new(error).size(LabelSize::Small))
                    .action_slot(
                        Button::new("network-allowlist-dismiss-error", i18n::t!("Dismiss"))
                            .style(ButtonStyle::Tinted(ui::TintColor::Warning))
                            .on_click(cx.listener(|this, _, _, cx| {
                                dismiss_error(this, cx);
                            })),
                    ),
            )
        })
        .map(|this| match &state.status {
            Status::Loading => this.child(
                Label::new(i18n::t!("Loading…"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
            Status::Missing => this.child(
                Banner::new().severity(Severity::Info).child(
                    Label::new(
                        i18n::t!(
                            "network_allowlist.json does not exist yet. The allowlist is treated as empty (only loopback is allowed). The file is created when you add a host."
                        ),
                    )
                    .size(LabelSize::Small),
                ),
            ),
            Status::Present => this,
            Status::Unusable(message) => this.child(
                Banner::new().severity(Severity::Error).child(
                    v_flex()
                        .gap_0p5()
                        .child(Label::new(message.clone()).size(LabelSize::Small))
                        .child(
                            Label::new(
                                i18n::t!(
                                    "While the file cannot be parsed, the allowlist is treated as empty (only loopback is allowed). Fix the file directly. Editing from this page is disabled."
                                ),
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                ),
            ),
        })
        .when(is_editable, |this| {
            this.child(
                v_flex()
                    .gap_4()
                    .child(SettingsSectionHeader::new(i18n::t!("Allowed Hosts")).no_padding(true))
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                Label::new(i18n::tn!(
                                    entry_count,
                                    "{count} entry registered",
                                    "{count} entries registered"
                                ))
                                    .size(LabelSize::Small),
                            )
                            .child(
                                Label::new(i18n::tr(HOSTS_DESCRIPTION))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                v_flex()
                                    .mt_2()
                                    .w_full()
                                    .gap_1p5()
                                    .when(host_rows.is_empty(), |this| {
                                        this.child(render_empty_state(empty_border))
                                    })
                                    .when(!host_rows.is_empty(), |this| {
                                        this.child(v_flex().gap_1p5().children(host_rows))
                                    })
                                    .child(render_add_host_input(cx)),
                            ),
                    ),
            )
        })
        // Hidden rather than disabled while the file is unusable or unread, so
        // it behaves exactly like the host list above it: a toggle whose state
        // came from a file that could not be parsed would be a claim this page
        // cannot make.
        .when(is_editable, |this| {
            this.child(render_child_process_proxy(proxy_child_processes, cx))
        })
        .child(Divider::horizontal())
        .child(render_file_actions(settings_window, cx))
        .into_any_element()
}

fn render_intro(cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(Label::new(
            i18n::t!(
                "Controls only outbound traffic that AI initiates. All such traffic is denied by default, and only the hosts registered here are allowed."
            ),
        ))
        .child(
            Label::new(
                i18n::t!(
                    "This covers outbound traffic from AI providers (LLMs), agent tools (fetch / web search), MCP servers, external agent (ACP) processes, and commands that agents run in the sandbox. Traffic from the IDE's own tooling, such as language servers, formatters, debug adapters, extensions, app updates, remote development servers, Git, and the browser panel, is not covered and is not restricted."
                ),
            )
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .child(
            Label::new(
                i18n::t!(
                    "127.0.0.1 / ::1 / localhost are always allowed without being registered, so local LLM servers (such as Ollama) work even with an empty allowlist. A small number of hosts, such as the external agent (ACP) registry, are allowed by built-in rules. Every redirect target is checked the same way, one hop at a time."
                ),
            )
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .child(
            Label::new(
                i18n::t!(
                    "\"Sandbox\" on the \"AI\" page is a separate setting that isolates the commands agents run (restricting which domains they can reach and where they can write). However, domains allowed there take effect only within this list. Think of the sandbox as narrowing this scope further rather than widening what is allowed."
                ),
            )
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .child(
            Label::new(paths::network_allowlist_file().to_string_lossy().into_owned())
                .size(LabelSize::Small)
                .color(Color::Muted)
                .buffer_font(cx),
        )
}

fn render_child_process_proxy(enabled: bool, cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    let settings_window = cx.entity().downgrade();

    v_flex()
        .gap_4()
        .child(
            SettingsSectionHeader::new(i18n::t!("Outbound Traffic from Child Processes"))
                .no_padding(true),
        )
        .child(
            v_flex()
                .gap_1()
                .child(
                    SwitchField::new(
                        "network-allowlist-proxy-child-processes",
                        Some(i18n::t!("Apply the Proxy to AI Agent Child Processes")),
                        Some(i18n::tr(PROXY_CHILD_PROCESSES_DESCRIPTION).into()),
                        enabled,
                        move |state, _window, cx| {
                            let enabled = *state == ToggleState::Selected;
                            settings_window
                                .update(cx, |this, cx| {
                                    set_proxy_child_processes(this, enabled, cx);
                                })
                                .log_err();
                        },
                    )
                    .tab_index(0),
                )
                .child(
                    Label::new(i18n::tr(PROXY_CHILD_PROCESSES_NOTE))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
}

fn render_file_actions(
    settings_window: &SettingsWindow,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let is_loading = settings_window.network_allowlist.status == Status::Loading;
    // Opening the file needs a workspace to open it in; the settings window can
    // be running without one.
    let can_open_file = settings_window.original_window.is_some();

    h_flex()
        .gap_1()
        .child(
            Button::new("network-allowlist-reload", i18n::t!("Reload"))
                .label_size(LabelSize::Small)
                .color(Color::Muted)
                .disabled(is_loading)
                .start_icon(
                    Icon::new(IconName::RotateCw)
                        .color(Color::Muted)
                        .size(IconSize::XSmall),
                )
                .tooltip(Tooltip::text(i18n::t!("Read the file again")))
                .on_click(cx.listener(|this, _, _, cx| {
                    load_network_allowlist(this, cx);
                })),
        )
        .child(
            Button::new(
                "network-allowlist-open-file",
                i18n::t!("Open network_allowlist.json in Editor"),
            )
            .label_size(LabelSize::Small)
            .color(Color::Muted)
            .disabled(!can_open_file)
            .start_icon(
                Icon::new(IconName::FileCode)
                    .color(Color::Muted)
                    .size(IconSize::XSmall),
            )
            .tooltip(Tooltip::text(
                i18n::t!(
                    "Closes the settings window and opens the file in the editor. Creates the file if it does not exist"
                ),
            ))
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_network_allowlist_file(file_template(), window, cx);
            })),
        )
}

fn render_empty_state(border_color: gpui::Hsla) -> AnyElement {
    h_flex()
        .p_2()
        .rounded_md()
        .border_1()
        .border_dashed()
        .border_color(border_color)
        .child(
            Label::new(i18n::t!(
                "No hosts configured (everything except loopback is denied)"
            ))
            .size(LabelSize::Small)
            .color(Color::Disabled),
        )
        .into_any_element()
}

fn render_host_row(index: usize, host: String, cx: &mut Context<SettingsWindow>) -> AnyElement {
    let is_valid = http_client::HostPattern::parse(&host).is_ok();
    let settings_window = cx.entity().downgrade();

    let field = SettingsInputField::new(format!("network-allowlist-host-{index}"))
        .with_initial_text(host)
        .tab_index(0)
        .with_buffer_font()
        .color(if is_valid {
            Color::Default
        } else {
            Color::Error
        })
        .aria_label(i18n::t!("Allowed host"))
        .action_slot(
            IconButton::new(
                format!("network-allowlist-host-delete-{index}"),
                IconName::Trash,
            )
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text(i18n::t!("Remove Host")))
            .on_click(cx.listener(move |this, _, _, cx| {
                remove_host(this, index, cx);
            })),
        )
        .on_confirm(move |new_host, _window, cx| {
            let Some(new_host) = new_host else {
                return;
            };
            settings_window
                .update(cx, |this, cx| {
                    update_host(this, index, &new_host, cx);
                })
                .log_err();
        });

    if is_valid {
        field.into_any_element()
    } else {
        v_flex()
            .gap_0p5()
            .child(field)
            .child(
                Label::new(i18n::t!(
                    "This entry is ignored because its format is invalid."
                ))
                .size(LabelSize::Small)
                .color(Color::Error),
            )
            .into_any_element()
    }
}

fn render_add_host_input(cx: &mut Context<SettingsWindow>) -> AnyElement {
    let settings_window = cx.entity().downgrade();

    SettingsInputField::new("network-allowlist-host-new")
        .with_placeholder(i18n::t!(
            "Add a host (for example, api.anthropic.com, *.corp.example)…"
        ))
        .tab_index(0)
        .with_buffer_font()
        .display_clear_button()
        .display_confirm_button()
        .clear_on_confirm()
        .on_confirm(move |host, _window, cx| {
            let Some(host) = host else {
                return;
            };
            if host.trim().is_empty() {
                return;
            }
            settings_window
                .update(cx, |this, cx| {
                    add_host(this, &host, cx);
                })
                .log_err();
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The point of the page: what it writes has to reach the policy that
    /// `egress_policy` keeps for `AllowlistedHttpClient`, through nothing but
    /// the file. Covers the file being created as well as later rewrites, since
    /// only the latter go through `canonicalize`.
    #[gpui::test]
    async fn a_write_reaches_the_running_policy(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();

        let fake_fs = fs::FakeFs::new(cx.background_executor.clone());
        let fs: Arc<dyn Fs> = fake_fs.clone();
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fake_fs
            .insert_tree(Path::new(util::path!("/config")), serde_json::json!({}))
            .await;

        let handle = cx.update(|cx| egress_policy::watch(fake_fs.clone(), path.clone(), cx));
        cx.run_until_parked();

        let anthropic =
            http_client::RequestHost::parse("api.anthropic.com").expect("host should parse");
        assert!(!handle.allows(&anthropic));

        // Nothing on disk yet: an empty list, not an error.
        let (loaded, error) = apply(fs.clone(), &path, Operation::Reload).await;
        assert_eq!(error, None);
        assert_eq!(loaded.status, Status::Missing);
        assert!(loaded.entries.is_empty());

        let (loaded, error) = apply(
            fs.clone(),
            &path,
            Operation::Write(vec!["api.anthropic.com".to_string()]),
        )
        .await;
        assert_eq!(error, None);
        assert_eq!(loaded.status, Status::Present);
        assert_eq!(loaded.entries, vec!["api.anthropic.com".to_string()]);
        // A watch on a path that does not exist yet notices its creation by
        // polling on the executor's clock (upstream's watcher since v1.20), for
        // the fake filesystem as for the real one.
        cx.executor().advance_clock(fs::fs_watcher::poll_interval());
        cx.run_until_parked();
        assert!(
            handle.allows(&anthropic),
            "the egress policy watch should apply what this page wrote"
        );

        let (loaded, error) = apply(fs.clone(), &path, Operation::Write(Vec::new())).await;
        assert_eq!(error, None);
        assert!(loaded.entries.is_empty());
        cx.run_until_parked();
        assert!(
            !handle.allows(&anthropic),
            "removing the last host should close it again"
        );
    }

    /// `Operation::Add` is issued from the external-AI consent flow, which
    /// cannot assume this page has been opened. It therefore has to append to
    /// what is on disk rather than to the (possibly unread) in-memory list.
    #[gpui::test]
    async fn add_appends_to_the_file_rather_than_replacing_it(cx: &mut gpui::TestAppContext) {
        let fake_fs = fs::FakeFs::new(cx.background_executor.clone());
        let fs: Arc<dyn Fs> = fake_fs.clone();
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fake_fs
            .insert_tree(
                Path::new(util::path!("/config")),
                serde_json::json!({
                    "network_allowlist.json": "{ \"allowed_hosts\": [\"*.corp.example\"] }\n"
                }),
            )
            .await;

        let (loaded, error) = apply(
            fs.clone(),
            &path,
            Operation::Add(vec![
                "api.anthropic.com".to_string(),
                "*.corp.example".to_string(),
            ]),
        )
        .await;

        assert_eq!(error, None);
        assert_eq!(
            loaded.entries,
            vec![
                "*.corp.example".to_string(),
                "api.anthropic.com".to_string()
            ],
            "an entry already present must not be duplicated, and the existing ones must survive"
        );
    }

    /// `Operation::Remove` is what withdrawing an external-AI consent issues
    /// (ADR-F10). Like `Add` it works against what is on disk, and it must take
    /// away only the entry that names the host.
    #[gpui::test]
    async fn remove_takes_back_what_add_put_in(cx: &mut gpui::TestAppContext) {
        let fake_fs = fs::FakeFs::new(cx.background_executor.clone());
        let fs: Arc<dyn Fs> = fake_fs.clone();
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fake_fs
            .insert_tree(
                Path::new(util::path!("/config")),
                serde_json::json!({
                    "network_allowlist.json": "{ \"allowed_hosts\": [\"*.corp.example\"] }\n"
                }),
            )
            .await;

        let (loaded, error) = apply(
            fs.clone(),
            &path,
            Operation::Add(vec!["api.anthropic.com".to_string()]),
        )
        .await;
        assert_eq!(error, None);
        assert_eq!(
            loaded.entries,
            vec![
                "*.corp.example".to_string(),
                "api.anthropic.com".to_string()
            ]
        );

        let (loaded, error) = apply(
            fs.clone(),
            &path,
            Operation::Remove(vec!["api.anthropic.com".to_string()]),
        )
        .await;
        assert_eq!(error, None);
        assert_eq!(
            loaded.entries,
            vec!["*.corp.example".to_string()],
            "revoking one provider must not disturb an entry the user wrote"
        );

        // Pressing revoke on a provider whose hosts were never registered.
        let (loaded, error) = apply(
            fs.clone(),
            &path,
            Operation::Remove(vec!["api.openai.com".to_string()]),
        )
        .await;
        assert_eq!(error, None);
        assert_eq!(loaded.entries, vec!["*.corp.example".to_string()]);
    }

    /// What the consent blocks call "registered", and the only kind of entry a
    /// revoke removes: coverage by a wildcard is not this provider's entry.
    #[test]
    fn lists_host_matches_whole_entries_only() {
        let state = NetworkAllowlistState {
            status: Status::Present,
            entries: vec![
                "*.corp.example".to_string(),
                "API.Anthropic.COM".to_string(),
            ],
            ..Default::default()
        };

        assert!(state.lists_host("api.anthropic.com"));
        assert!(state.lists_host("*.corp.example"));
        assert!(
            !state.lists_host("api.corp.example"),
            "covered by the wildcard, but not an entry of its own"
        );
        assert!(
            state.allows("api.corp.example"),
            "it is still allowed, which is why the two questions are separate"
        );
        assert!(!state.lists_host("api.openai.com"));
        assert!(!state.lists_host("not a host"));
    }

    #[gpui::test]
    async fn refuses_to_rewrite_a_file_it_cannot_parse(cx: &mut gpui::TestAppContext) {
        let broken = "{ \"allowed_hosts\": [\n";
        let fake_fs = fs::FakeFs::new(cx.background_executor.clone());
        let fs: Arc<dyn Fs> = fake_fs.clone();
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fake_fs
            .insert_tree(
                Path::new(util::path!("/config")),
                serde_json::json!({ "network_allowlist.json": broken }),
            )
            .await;

        let (loaded, error) = apply(
            fs.clone(),
            &path,
            Operation::Write(vec!["zed.dev".to_string()]),
        )
        .await;

        assert!(error.is_some(), "the write should have been refused");
        assert!(matches!(loaded.status, Status::Unusable(_)));
        assert_eq!(
            fake_fs
                .load(&path)
                .await
                .expect("file should still be there"),
            broken,
            "a file that cannot be parsed must be left exactly as it was"
        );
    }

    #[test]
    fn rewrites_only_the_allowed_hosts_array() {
        let existing = concat!(
            "// Header comment that must survive.\n",
            "{\n",
            "  // Keep this one too.\n",
            "  \"allowed_hosts\": [\n",
            "    \"api.anthropic.com\"\n",
            "  ]\n",
            "}\n",
        );
        let rewritten = allowed_hosts_replaced(
            existing,
            &[
                "api.anthropic.com".to_string(),
                "*.corp.example".to_string(),
            ],
        )
        .expect("rewrite should succeed");

        assert!(rewritten.contains("// Header comment that must survive."));
        assert!(rewritten.contains("// Keep this one too."));
        let parsed: NetworkAllowlistFile = settings_json::parse_json_with_comments(&rewritten)
            .expect("rewritten file should parse");
        assert_eq!(
            parsed.allowed_hosts,
            vec![
                "api.anthropic.com".to_string(),
                "*.corp.example".to_string()
            ]
        );
    }

    #[test]
    fn creates_the_key_in_an_empty_or_keyless_file() {
        for existing in ["", "{}", "{\n  \"future_key\": true\n}\n"] {
            let rewritten = allowed_hosts_replaced(existing, &["zed.dev".to_string()])
                .expect("rewrite should succeed");
            let parsed: NetworkAllowlistFile = settings_json::parse_json_with_comments(&rewritten)
                .unwrap_or_else(|error| panic!("{existing:?} -> {rewritten:?}: {error}"));
            assert_eq!(parsed.allowed_hosts, vec!["zed.dev".to_string()]);
        }
    }

    /// The toggle rides in the same file as the host list (ADR-F14), so it has
    /// to leave that list — and the comments around it — untouched.
    #[test]
    fn rewrites_only_the_proxy_child_processes_value() {
        let existing = concat!(
            "// Header comment that must survive.\n",
            "{\n",
            "  \"proxy_child_processes\": true,\n",
            "  // Keep this one too.\n",
            "  \"allowed_hosts\": [\n",
            "    \"api.anthropic.com\"\n",
            "  ]\n",
            "}\n",
        );
        let rewritten =
            proxy_child_processes_replaced(existing, false).expect("rewrite should succeed");

        assert!(rewritten.contains("// Header comment that must survive."));
        assert!(rewritten.contains("// Keep this one too."));
        let parsed: NetworkAllowlistFile = settings_json::parse_json_with_comments(&rewritten)
            .expect("rewritten file should parse");
        assert_eq!(parsed.proxy_child_processes, Some(false));
        assert_eq!(
            parsed.allowed_hosts,
            vec!["api.anthropic.com".to_string()],
            "turning the proxy off must not touch the host allowlist"
        );
    }

    /// `true` is the default, but it is still written out rather than left
    /// implicit, so turning the toggle back on undoes an explicit `false`.
    #[test]
    fn creates_the_proxy_key_in_an_empty_or_keyless_file() {
        for (existing, enabled) in [
            ("", true),
            ("", false),
            ("{}", true),
            ("{ \"allowed_hosts\": [\"zed.dev\"] }\n", true),
            ("{ \"allowed_hosts\": [\"zed.dev\"] }\n", false),
        ] {
            let rewritten =
                proxy_child_processes_replaced(existing, enabled).expect("rewrite should succeed");
            let parsed: NetworkAllowlistFile = settings_json::parse_json_with_comments(&rewritten)
                .unwrap_or_else(|error| panic!("{existing:?} -> {rewritten:?}: {error}"));
            assert_eq!(parsed.proxy_child_processes, Some(enabled));
        }
    }

    /// What the page reads back is what `egress_policy::parse_file` would: an
    /// absent key means on, and only an explicit `false` turns it off.
    #[gpui::test]
    async fn reads_the_proxy_toggle_and_writes_it_back(cx: &mut gpui::TestAppContext) {
        let fake_fs = fs::FakeFs::new(cx.background_executor.clone());
        let fs: Arc<dyn Fs> = fake_fs.clone();
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fake_fs
            .insert_tree(
                Path::new(util::path!("/config")),
                serde_json::json!({
                    "network_allowlist.json": "// keep me\n{ \"allowed_hosts\": [\"zed.dev\"] }\n"
                }),
            )
            .await;

        let (loaded, error) = apply(fs.clone(), &path, Operation::Reload).await;
        assert_eq!(error, None);
        assert!(
            loaded.proxy_child_processes,
            "an absent key means the proxy is on"
        );

        let (loaded, error) =
            apply(fs.clone(), &path, Operation::SetProxyChildProcesses(false)).await;
        assert_eq!(error, None);
        assert!(!loaded.proxy_child_processes);
        assert_eq!(
            loaded.entries,
            vec!["zed.dev".to_string()],
            "the host list must survive the toggle"
        );
        let on_disk = fake_fs.load(&path).await.expect("file should exist");
        assert!(on_disk.contains("// keep me"));
        assert!(!egress_policy::parse_file(&on_disk).proxy_child_processes);

        let (loaded, error) =
            apply(fs.clone(), &path, Operation::SetProxyChildProcesses(true)).await;
        assert_eq!(error, None);
        assert!(loaded.proxy_child_processes);
        let on_disk = fake_fs.load(&path).await.expect("file should exist");
        assert!(egress_policy::parse_file(&on_disk).proxy_child_processes);
    }

    /// The same guard the host list gets: a file mid-hand-edit is never
    /// rewritten out from under the user, whichever control was touched.
    #[gpui::test]
    async fn refuses_to_toggle_the_proxy_in_a_file_it_cannot_parse(cx: &mut gpui::TestAppContext) {
        let broken = "{ \"allowed_hosts\": [\n";
        let fake_fs = fs::FakeFs::new(cx.background_executor.clone());
        let fs: Arc<dyn Fs> = fake_fs.clone();
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fake_fs
            .insert_tree(
                Path::new(util::path!("/config")),
                serde_json::json!({ "network_allowlist.json": broken }),
            )
            .await;

        let (loaded, error) =
            apply(fs.clone(), &path, Operation::SetProxyChildProcesses(false)).await;

        assert!(error.is_some(), "the write should have been refused");
        assert!(matches!(loaded.status, Status::Unusable(_)));
        assert!(
            loaded.proxy_child_processes,
            "an unusable file reads as the default, which is on"
        );
        assert_eq!(
            fake_fs
                .load(&path)
                .await
                .expect("file should still be there"),
            broken,
            "a file that cannot be parsed must be left exactly as it was"
        );
    }

    /// The header is translated and the body is not; the file created from the
    /// two has to be an empty allowlist.
    #[test]
    fn the_file_template_is_an_empty_allowlist() {
        let template = file_template();
        assert!(template.starts_with(FILE_TEMPLATE_HEADER));
        {
            let _english = i18n::test_support::LocaleOverride::new(
                i18n_catalog::english_catalog(None).map(|catalog| catalog as &dyn i18n::Catalog),
            );
            let english = file_template();
            assert!(
                english.starts_with("// ForgeKairos egress allowlist (.forgekairos-docs/DESIGN.md"),
                "{english}"
            );
        }
        let parsed: NetworkAllowlistFile =
            settings_json::parse_json_with_comments(&template).expect("the template should parse");
        assert!(parsed.allowed_hosts.is_empty());
        assert_eq!(parsed.proxy_child_processes, None);

        // Built when the button is clicked, so it follows a language switch.
        let _japanese = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        let japanese = file_template();
        assert!(
            japanese.starts_with("// ForgeKairos 外部通信の許可リスト"),
            "{japanese}"
        );
        let parsed: NetworkAllowlistFile = settings_json::parse_json_with_comments(&japanese)
            .expect("the translated template should parse");
        assert!(parsed.allowed_hosts.is_empty());
    }

    #[test]
    fn removing_the_last_entry_leaves_an_empty_array() {
        let existing = "{ \"allowed_hosts\": [\"zed.dev\"] }\n";
        let rewritten = allowed_hosts_replaced(existing, &[]).expect("rewrite should succeed");
        let parsed: NetworkAllowlistFile =
            settings_json::parse_json_with_comments(&rewritten).expect("should parse");
        assert!(parsed.allowed_hosts.is_empty());
    }

    #[test]
    fn canonicalizes_the_forms_the_policy_accepts() {
        assert_eq!(
            canonicalize_host(" API.Anthropic.COM. "),
            Ok("api.anthropic.com".to_string())
        );
        assert_eq!(
            canonicalize_host("*.corp.example"),
            Ok("*.corp.example".to_string())
        );
        assert_eq!(canonicalize_host("10.0.0.5"), Ok("10.0.0.5".to_string()));
        assert_eq!(canonicalize_host("[fd00::1]"), Ok("[fd00::1]".to_string()));
    }

    #[test]
    fn rejects_what_the_policy_would_skip() {
        for input in ["", "   ", "*.", "foo.*.com", "*.*", "a..b", "*.10.0.0.5"] {
            assert!(
                canonicalize_host(input).is_err(),
                "{input:?} should be rejected"
            );
        }
    }
}
