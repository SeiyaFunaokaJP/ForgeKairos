use std::ops::Range;
use std::time::Duration;

use client::zed_urls;
use collections::HashMap;
use editor::{Editor, EditorElement, EditorStyle};
use fs::Fs;
use futures::{FutureExt as _, StreamExt as _, channel::mpsc::UnboundedReceiver};
use gpui::{
    AnyElement, App, BackgroundExecutor, ClickEvent, Context, Entity, EventEmitter, Focusable,
    KeyContext, ParentElement, Render, RenderOnce, SharedString, Styled, Task, TextStyle,
    UniformListScrollHandle, WeakEntity, Window, point, uniform_list,
};
use http_client::{EgressPolicyHandle, RequestHost, hosts_required_to_download};
use project::agent_registry_store::current_platform_key;
use project::agent_server_store::{
    AllAgentServersSettings, CustomAgentServerSettings, installed_registry_archive_versions,
    registry_archive_version_matches,
};
use project::{AgentId, AgentRegistryStore, RegistryAgent};
use settings::{Settings, SettingsStore, update_settings_file};
use theme_settings::ThemeSettings;
use ui::{
    Banner, ButtonStyle, CommonAnimationExt, ScrollableHandle, ToggleButtonGroup,
    ToggleButtonGroupSize, ToggleButtonGroupStyle, ToggleButtonSimple, Tooltip, WithScrollbar,
    prelude::*,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

use crate::{Agent, AgentPanel, agent_connection_store::AgentConnectionStore};

/// A registry agent's install requires network access this fork's
/// deny-by-default egress policy has to be told about (`docs/jp-enhance/DESIGN.md`
/// §4, F2-5). For a `Binary` agent that is every host its current-platform
/// archive download touches — which for a GitHub release asset is more than
/// one, the URL's own host plus every host it is known to redirect to, hence
/// [`http_client::hosts_required_to_download`] rather than `Url::host_str`;
/// for an `Npx` agent it is `nodejs.org` (fetched once for Zed's bundled Node)
/// plus `registry.npmjs.org`: since ADR-F14 (DESIGN.md §4.14) the registry
/// install's npm runs through the child-process egress proxy, which judges by
/// the user's allowlist alone, so without the registry host the install is
/// refused with 511. What npm's own install scripts may fetch beyond the
/// registry cannot be enumerated from the manifest, which is what the npx
/// warning in the UI is about.
pub(crate) struct InstallRequirement {
    /// Hosts this install needs in `network_allowlist.json`, derived from the
    /// registry manifest. Empty when nothing beyond what is already open is
    /// required (or, for `Npx`, when Node has already been fetched — see
    /// [`InstallConsentBanner`], which still shows the npx warning either way).
    pub(crate) hosts: Vec<SharedString>,
    pub(crate) is_npx: bool,
}

pub(crate) fn install_requirement(agent: &RegistryAgent) -> InstallRequirement {
    match agent {
        RegistryAgent::Binary(binary) => {
            let hosts = current_platform_key()
                .and_then(|platform| binary.targets.get(platform))
                .map(|target| hosts_required_to_download(&target.archive))
                .unwrap_or_default();
            InstallRequirement {
                hosts: hosts.into_iter().map(SharedString::from).collect(),
                is_npx: false,
            }
        }
        RegistryAgent::Npx(_) => InstallRequirement {
            hosts: vec![
                SharedString::from("nodejs.org"),
                SharedString::from("registry.npmjs.org"),
            ],
            is_npx: true,
        },
    }
}

/// Registers a registry agent in `settings.json` and opens a thread with it —
/// the effect the "Install" button always had before F2-5 added a consent
/// step in front of it for agents that need one.
fn install_registry_agent(agent_id: &str, window: &mut Window, cx: &mut App) {
    let fs = <dyn Fs>::global(cx);
    let agent_id_owned = agent_id.to_string();
    update_settings_file(fs, cx, move |settings, _| {
        let agent_servers = settings.agent_servers.get_or_insert_default();
        agent_servers.entry(agent_id_owned).or_insert_with(|| {
            settings::CustomAgentServerSettings::Registry {
                default_mode: None,
                env: Default::default(),
                default_config_options: HashMap::default(),
                favorite_config_option_values: HashMap::default(),
                allow_subscription_sign_in: None,
                run_outside_sandbox: None,
            }
        });
    });
    window.dispatch_action(
        Box::new(zed_actions::agent::SelectAgent {
            agent: agent_id.to_string(),
        }),
        cx,
    );
}

/// The subset of `hosts` the running egress policy does not already allow.
/// Falls back to "everything is missing" when the policy has not been
/// published yet (`egress_policy::watch` sets it before the app's
/// `HttpClient` is built, so in practice this only matters for a test that
/// never calls that), which is the safe direction: showing the consent
/// banner when it may not strictly be needed, never skipping it.
pub(crate) fn missing_hosts(hosts: &[SharedString], cx: &App) -> Vec<SharedString> {
    let allowed = cx.try_global::<egress_policy::EgressAllowlistGlobal>();
    hosts
        .iter()
        .filter(|host| {
            let Some(request_host) = RequestHost::parse(host) else {
                return true;
            };
            !allowed.is_some_and(|policy| policy.0.allows(&request_host))
        })
        .cloned()
        .collect()
}

/// What installing (or updating) the agent registered under `agent_id` needs
/// from the allowlist, read out of the ACP registry manifest that is currently
/// in memory.
///
/// `None` means the manifest cannot answer the question right now — the
/// registry has not been fetched (offline, or the fetch failed), or the agent
/// is a custom / built-in one that never came from the registry. Callers must
/// treat that as "no consent step", not as "nothing is needed": the download
/// itself still goes through the shared `HttpClient`, so an unallowed host
/// comes back as the usual 511 `EgressDenied` error with its own recovery
/// affordance rather than escaping the gate.
pub(crate) fn registry_install_requirement(
    agent_id: &AgentId,
    cx: &App,
) -> Option<InstallRequirement> {
    let registry_store = AgentRegistryStore::try_global(cx)?;
    let agent = registry_store.read(cx).agent(agent_id)?;
    Some(install_requirement(agent))
}

/// Adds `hosts` to `network_allowlist.json` off the main thread, using the
/// comment-preserving rewrite both this page and the settings allowlist page
/// share (`egress_policy::add_allowed_hosts`).
///
/// A consent is for named hosts only: a pattern (`*.example.com`, `*`) opens
/// the whole IDE's allowlist to more than what was asked about, so it is
/// refused here rather than written. The user can still add one themselves on
/// the allowlist page, where the pattern is what they typed.
pub(crate) fn add_consented_hosts(hosts: Vec<String>, cx: &App) -> Task<anyhow::Result<()>> {
    if let Some(pattern) = hosts.iter().find(|host| host.contains('*')) {
        let pattern = pattern.clone();
        return Task::ready(Err(anyhow::anyhow!(
            "ワイルドカードを含むホスト ({pattern}) は、同意からは許可リストに追加できません。"
        )));
    }
    let fs = <dyn Fs>::global(cx);
    let path = paths::network_allowlist_file().clone();
    cx.background_spawn(async move {
        egress_policy::add_allowed_hosts(&fs, &path, &hosts).await?;
        Ok(())
    })
}

/// How long a consent waits for the allowlist watcher to take in what it just
/// wrote before it gives up on it.
pub(crate) const ALLOWLIST_PICKUP_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) const ALLOWLIST_NOT_APPLIED: &str = "許可リストには追加しましたが、まだ反映されていません。しばらく待ってからもう一度お試しください。";

/// Shown while the consent's hosts are on their way from the file into the
/// running policy.
pub(crate) const ALLOWLIST_PICKUP_PENDING: &str = "許可リストへの反映を待っています…";

/// Why a consent could not be carried through. The two are worth telling
/// apart: nothing was added in the first case, and everything was added in
/// the second, so saying "could not be added" for a slow watcher sends the
/// user looking for a problem that is not there.
pub(crate) enum ConsentFailure {
    /// `network_allowlist.json` could not be written.
    Write(anyhow::Error),
    /// The hosts are in the file, but the running egress policy has not taken
    /// them in within [`ALLOWLIST_PICKUP_TIMEOUT`].
    NotPickedUp,
}

impl ConsentFailure {
    pub(crate) fn message(&self) -> SharedString {
        match self {
            Self::Write(error) => allowlist_write_error(error),
            Self::NotPickedUp => ALLOWLIST_NOT_APPLIED.into(),
        }
    }
}

/// [`add_consented_hosts`], and then waits until the running egress policy
/// actually allows `hosts`.
///
/// The allowlist file is watched, so the policy the rest of the IDE reads -
/// and, on Windows, the ceiling the WSL sandbox snapshots for its own proxy
/// when it runs `npm install` (ADR-F49 決定 13) - only catches up a moment
/// after the write lands. Whatever a consent leads to is refused with 511
/// until it has, so every consent waits here rather than racing the watcher.
pub(crate) fn add_consented_hosts_and_await_pickup(
    hosts: Vec<String>,
    cx: &App,
) -> Task<Result<(), ConsentFailure>> {
    let policy = cx
        .try_global::<egress_policy::EgressAllowlistGlobal>()
        .map(|global| global.0.clone());
    // Subscribed before the write so that the watcher's update cannot slip in
    // between the write finishing and the wait starting.
    let changes = policy.as_ref().map(EgressPolicyHandle::subscribe);
    let write = add_consented_hosts(hosts.clone(), cx);
    let executor = cx.background_executor().clone();
    cx.background_spawn(async move {
        write.await.map_err(ConsentFailure::Write)?;
        let (Some(policy), Some(mut changes)) = (policy, changes) else {
            // No policy in this process: nothing gates what follows either.
            return Ok(());
        };
        await_allowlist_pickup(&hosts, &policy, &mut changes, &executor)
            .await
            .map_err(|_| ConsentFailure::NotPickedUp)
    })
}

/// Resolves once `policy` allows every host in `hosts`, or fails with
/// [`ALLOWLIST_NOT_APPLIED`] after [`ALLOWLIST_PICKUP_TIMEOUT`].
pub(crate) async fn await_allowlist_pickup(
    hosts: &[String],
    policy: &EgressPolicyHandle,
    changes: &mut UnboundedReceiver<()>,
    executor: &BackgroundExecutor,
) -> anyhow::Result<()> {
    let mut timeout = executor.timer(ALLOWLIST_PICKUP_TIMEOUT).fuse();
    loop {
        if hosts
            .iter()
            .all(|host| RequestHost::parse(host).is_some_and(|host| policy.allows(&host)))
        {
            return Ok(());
        }
        futures::select_biased! {
            change = changes.next() => {
                if change.is_none() {
                    anyhow::bail!(ALLOWLIST_NOT_APPLIED);
                }
            }
            _ = timeout => anyhow::bail!(ALLOWLIST_NOT_APPLIED),
        }
    }
}

pub(crate) fn allowlist_write_error(error: &anyhow::Error) -> SharedString {
    format!("許可リストへの追加に失敗しました: {error:#}").into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegistryFilter {
    All,
    Installed,
    NotInstalled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegistryInstallStatus {
    NotInstalled,
    InstalledRegistry,
    InstalledCustom,
}

impl RegistryInstallStatus {
    fn is_installed(self) -> bool {
        matches!(self, Self::InstalledRegistry | Self::InstalledCustom)
    }
}

/// ADR-F55: what this fork has to add to a registry entry's own description.
/// The registry's text is the vendor's and says nothing of how this route
/// is authenticated, so the one entry that has something to say says it
/// next to it.
fn fork_agent_note(agent_id: &str) -> Option<&'static str> {
    (agent_id == agent_servers::CLAUDE_AGENT_ID).then_some(
        "サブスクリプションまたは API キー: サインインは Claude Code 自身の\
         画面で行われ、Zed は認証情報を扱いません。設定でサブスクリプションを\
         無効にできます。",
    )
}

/// What the registry page can say about updating an installed agent (F2-5
/// follow-up).
#[derive(Debug, Clone, PartialEq)]
enum UpdateStatus {
    Unknown,
    UpToDate,
    /// npm resolves an `npx` agent's version range at launch, so there is no
    /// one installed version to compare against.
    UnknownForNpx,
    Available {
        installed: Option<SharedString>,
        latest: SharedString,
    },
}

/// `cached_on_disk` is the host's archive cache, most recently written first
/// ([`project::agent_server_store::installed_registry_archive_versions`]);
/// `reported_by_connection` is a newer version a running connection announced
/// during this session.
fn update_status(
    is_npx: bool,
    registry_version: &SharedString,
    cached_on_disk: &[String],
    reported_by_connection: Option<&SharedString>,
) -> UpdateStatus {
    if is_npx {
        return UpdateStatus::UnknownForNpx;
    }
    // The cache wins over the connection's report: that report is never
    // withdrawn, so once a new thread has installed the version it announced,
    // only the cache knows the update is done.
    if let Some(newest_cached) = cached_on_disk.first() {
        if cached_on_disk
            .iter()
            .any(|cached| registry_archive_version_matches(cached, registry_version))
        {
            return UpdateStatus::UpToDate;
        }
        return UpdateStatus::Available {
            installed: Some(SharedString::from(newest_cached.clone())),
            latest: registry_version.clone(),
        };
    }
    match reported_by_connection {
        Some(latest) => UpdateStatus::Available {
            installed: None,
            latest: latest.clone(),
        },
        None => UpdateStatus::Unknown,
    }
}

fn update_badge_label(installed: Option<&SharedString>, latest: &SharedString) -> String {
    match installed {
        Some(installed) => format!("更新あり: v{installed} → v{latest}"),
        None if latest.is_empty() => "更新あり".to_string(),
        None => format!("更新あり: v{latest}"),
    }
}

/// Marks an installed agent that has a newer version. It never updates
/// anything itself: replacing an agent underneath a live session desyncs it,
/// so a click only hands off to a thread with that agent ("受け渡し"), whose
/// connection starts the newest version, or whose update banner does.
fn update_available_badge(
    agent_id: &str,
    installed: Option<&SharedString>,
    latest: &SharedString,
) -> impl IntoElement {
    let agent_id_for_click = agent_id.to_string();
    h_flex()
        .id(SharedString::from(format!("agent-update-badge-{agent_id}")))
        .gap_1()
        .cursor_pointer()
        .child(
            Icon::new(IconName::Download)
                .size(IconSize::XSmall)
                .color(Color::Accent),
        )
        .child(
            Label::new(update_badge_label(installed, latest))
                .size(LabelSize::Small)
                .color(Color::Accent),
        )
        .tooltip(Tooltip::text(
            "クリックするとこのエージェントのスレッドを開きます。ここからは更新せず、\
             実行中のセッションも止めません。新しく接続したスレッドは最新版で起動し、\
             実行中のスレッドには更新バナーが出ます。",
        ))
        .on_click(move |_, window, cx| {
            window.dispatch_action(
                Box::new(zed_actions::agent::SelectAgent {
                    agent: agent_id_for_click.clone(),
                }),
                cx,
            );
        })
}

/// An install the user has to confirm before it proceeds (F2-5): the "Install"
/// button was pressed for an agent that needs a host added to the network
/// allowlist, or that is distributed via `npx` (whose package fetch bypasses
/// the allowlist entirely). Kept per agent id so confirming or cancelling one
/// card never disturbs another the user may also have open.
pub(crate) struct PendingInstallConsent {
    pub(crate) hosts: Vec<SharedString>,
    pub(crate) is_npx: bool,
    /// Set when the last attempt to add `hosts` to `network_allowlist.json`
    /// failed, so the banner can report it instead of silently retrying.
    pub(crate) error: Option<SharedString>,
    /// The consent was confirmed and its hosts are being written and waited
    /// for. Both buttons are dead while it is set: a second confirm would
    /// start a second install, and a cancel would leave the hosts added with
    /// nothing installed.
    pub(crate) waiting: bool,
}

impl PendingInstallConsent {
    pub(crate) fn new(requirement: InstallRequirement) -> Self {
        Self {
            hosts: requirement.hosts,
            is_npx: requirement.is_npx,
            error: None,
            waiting: false,
        }
    }
}

/// Which of the two moments a consent banner is gating. The decision the user
/// makes is the same one either way — "may this download reach these hosts" —
/// so only the verb in the confirm button differs; every other line of the
/// banner is shared so an update never reads as a different kind of prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallConsentAction {
    Install,
    Update,
}

impl InstallConsentAction {
    /// `nothing_to_add` distinguishes the npx-only banner (all hosts already
    /// allowed, we are only surfacing the warning) from the one that will
    /// actually write `network_allowlist.json`.
    fn confirm_label(self, nothing_to_add: bool) -> &'static str {
        match (self, nothing_to_add) {
            (Self::Install, false) => "許可リストに追加してインストール",
            (Self::Install, true) => "了承してインストール",
            (Self::Update, false) => "許可リストに追加して更新",
            (Self::Update, true) => "了承して更新",
        }
    }
}

/// The banner shown in place of a settled "Install" / "Update" click while it
/// needs the user to confirm a network allowlist addition, an npx warning, or
/// both (F2-5, DESIGN.md §4.12, in the style of ADR-F10's consent banners).
///
/// Shared by the registry page and the thread's "new version" callout so a
/// host that was removed from the allowlist after the install is met with the
/// same prompt as the install was.
#[derive(IntoElement)]
pub(crate) struct InstallConsentBanner {
    id: SharedString,
    hosts: Vec<SharedString>,
    is_npx: bool,
    error: Option<SharedString>,
    waiting: bool,
    action: InstallConsentAction,
    on_confirm: Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>,
    on_cancel: Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>,
}

impl InstallConsentBanner {
    pub(crate) fn new(
        id: impl Into<SharedString>,
        consent: &PendingInstallConsent,
        action: InstallConsentAction,
        on_confirm: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_cancel: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            hosts: consent.hosts.clone(),
            is_npx: consent.is_npx,
            error: consent.error.clone(),
            waiting: consent.waiting,
            action,
            on_confirm: Box::new(on_confirm),
            on_cancel: Box::new(on_cancel),
        }
    }
}

impl RenderOnce for InstallConsentBanner {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let all_allowed = missing_hosts(&self.hosts, cx).is_empty();
        let hosts_label = self
            .hosts
            .iter()
            .map(SharedString::to_string)
            .collect::<Vec<_>>()
            .join("、");
        let confirm_label = self.action.confirm_label(self.is_npx && all_allowed);
        let id = self.id;
        let waiting = self.waiting;
        let on_confirm = self.on_confirm;
        let on_cancel = self.on_cancel;

        Banner::new()
            .severity(Severity::Warning)
            .child(
                v_flex()
                    .gap_2()
                    .when(self.is_npx, |this| {
                        this.child(
                            v_flex()
                                .gap_0p5()
                                .child(Label::new(
                                    "このエージェントは npx (npm) 経由でインストールされます",
                                ))
                                .child(
                                    Label::new(
                                        "npm によるパッケージの取得と、インストール後の実行時通信は子プロセスプロキシ経由で許可リストの対象になります。ただし、パッケージのインストールスクリプトが追加で必要とするホストは事前に列挙できず、許可リストに無いと遮断されて失敗します。可能であれば Binary 型のエージェントを優先してください。",
                                    )
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                ),
                        )
                    })
                    .when(!self.hosts.is_empty(), |this| {
                        this.child(
                            h_flex()
                                .flex_wrap()
                                .gap_1p5()
                                .child(
                                    Label::new("許可が必要なホスト:")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(Label::new(hosts_label).size(LabelSize::Small).buffer_font(cx))
                                .child(
                                    Label::new(if all_allowed { "(登録済み)" } else { "(未登録)" })
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                    })
                    .when_some(self.error, |this, error| {
                        this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    })
                    .when(waiting, |this| {
                        this.child(
                            h_flex()
                                .gap_1p5()
                                .child(
                                    Icon::new(IconName::LoadCircle)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted)
                                        .with_rotate_animation(2),
                                )
                                .child(
                                    Label::new(ALLOWLIST_PICKUP_PENDING)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(
                                    SharedString::from(format!("install-consent-confirm-{id}")),
                                    confirm_label,
                                )
                                .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                                .label_size(LabelSize::Small)
                                .disabled(waiting)
                                .on_click(on_confirm),
                            )
                            .child(
                                Button::new(
                                    SharedString::from(format!("install-consent-cancel-{id}")),
                                    "追加せず中止",
                                )
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .disabled(waiting)
                                .on_click(on_cancel),
                            ),
                    ),
            )
    }
}

#[derive(IntoElement)]
struct AgentRegistryCard {
    children: Vec<AnyElement>,
}

impl AgentRegistryCard {
    fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }
}

impl ParentElement for AgentRegistryCard {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for AgentRegistryCard {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        div().w_full().child(
            v_flex()
                .p_3()
                .mt_4()
                .w_full()
                .min_h(rems_from_px(86_f32))
                .gap_2()
                .bg(cx.theme().colors().elevated_surface_background.opacity(0.5))
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .rounded_md()
                .children(self.children),
        )
    }
}

pub struct AgentRegistryPage {
    registry_store: Entity<AgentRegistryStore>,
    /// Read-only source for the "update available" badge. The update check
    /// itself belongs to the running agent connection (F2-5); this page only
    /// reports what that check already found.
    connection_store: Option<WeakEntity<AgentConnectionStore>>,
    /// Registry id -> the versions the host's archive cache holds, as
    /// [`project::agent_server_store::installed_registry_archive_versions`]
    /// lists them.
    installed_archive_versions: HashMap<String, Vec<String>>,
    _installed_archive_versions_scan: Task<()>,
    list: UniformListScrollHandle,
    registry_agents: Vec<RegistryAgent>,
    filtered_registry_indices: Vec<usize>,
    installed_statuses: HashMap<String, RegistryInstallStatus>,
    pending_install_consent: HashMap<String, PendingInstallConsent>,
    query_editor: Entity<Editor>,
    filter: RegistryFilter,
    _subscriptions: Vec<gpui::Subscription>,
}

impl AgentRegistryPage {
    pub fn new(
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let connection_store = workspace
            .panel::<AgentPanel>(cx)
            .map(|panel| panel.read(cx).connection_store().downgrade());
        cx.new(|cx| {
            let registry_store = AgentRegistryStore::global(cx);
            let query_editor = cx.new(|cx| {
                let mut input = Editor::single_line(window, cx);
                input.set_placeholder_text("エージェントを検索...", window, cx);
                input
            });
            cx.subscribe(&query_editor, Self::on_query_change).detach();

            let mut subscriptions = Vec::new();
            subscriptions.push(cx.observe(&registry_store, |this, _, cx| {
                this.reload_registry_agents(cx);
            }));
            subscriptions.push(cx.observe_global::<SettingsStore>(|this, cx| {
                this.filter_registry_agents(cx);
                this.scan_installed_archive_versions(cx);
            }));

            if let Some(connection_store) = connection_store
                .as_ref()
                .and_then(|connection_store| connection_store.upgrade())
            {
                // A connection that just started may have installed a new
                // version, so the cache is read again as well.
                subscriptions.push(cx.observe(&connection_store, |this, _store, cx| {
                    this.scan_installed_archive_versions(cx);
                    cx.notify();
                }));
            }

            let mut this = Self {
                registry_store,
                connection_store,
                installed_archive_versions: HashMap::default(),
                _installed_archive_versions_scan: Task::ready(()),
                list: UniformListScrollHandle::new(),
                registry_agents: Vec::new(),
                filtered_registry_indices: Vec::new(),
                installed_statuses: HashMap::default(),
                pending_install_consent: HashMap::default(),
                query_editor,
                filter: RegistryFilter::All,
                _subscriptions: subscriptions,
            };

            this.reload_registry_agents(cx);
            this.registry_store
                .update(cx, |store, cx| store.refresh(cx));

            this
        })
    }

    fn reload_registry_agents(&mut self, cx: &mut Context<Self>) {
        self.registry_agents = self.registry_store.read(cx).agents().to_vec();
        self.registry_agents.sort_by(|left, right| {
            left.name()
                .as_ref()
                .to_lowercase()
                .cmp(&right.name().as_ref().to_lowercase())
                .then_with(|| {
                    left.id()
                        .as_ref()
                        .to_lowercase()
                        .cmp(&right.id().as_ref().to_lowercase())
                })
        });
        self.filter_registry_agents(cx);
        self.scan_installed_archive_versions(cx);
    }

    fn refresh_installed_statuses(&mut self, cx: &mut Context<Self>) {
        let settings = cx
            .global::<SettingsStore>()
            .get::<AllAgentServersSettings>(None);
        self.installed_statuses.clear();
        for (id, settings) in settings.iter() {
            let status = match settings {
                CustomAgentServerSettings::Registry { .. } => {
                    RegistryInstallStatus::InstalledRegistry
                }
                CustomAgentServerSettings::Custom { .. } => RegistryInstallStatus::InstalledCustom,
            };
            self.installed_statuses.insert(id.clone(), status);
        }
    }

    fn install_status(&self, id: &str) -> RegistryInstallStatus {
        self.installed_statuses
            .get(id)
            .copied()
            .unwrap_or(RegistryInstallStatus::NotInstalled)
    }

    fn update_status(&self, agent: &RegistryAgent, cx: &App) -> UpdateStatus {
        let id = agent.id().as_ref();
        if !self.install_status(id).is_installed() {
            return UpdateStatus::Unknown;
        }
        let reported_by_connection = self
            .connection_store
            .as_ref()
            .and_then(|connection_store| connection_store.upgrade())
            .and_then(|connection_store| {
                connection_store
                    .read(cx)
                    .new_version_available(&Agent::from(AgentId::new(id.to_string())))
                    .cloned()
            });
        update_status(
            matches!(agent, RegistryAgent::Npx(_)),
            agent.version(),
            self.installed_archive_versions
                .get(id)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            reported_by_connection.as_ref(),
        )
    }

    /// Reads which version of each installed archive agent the host has cached,
    /// for [`Self::update_status`]. Nothing is fetched: the registry's latest
    /// version is already in memory.
    fn scan_installed_archive_versions(&mut self, cx: &mut Context<Self>) {
        let registry_ids = self
            .registry_agents
            .iter()
            .filter(|agent| {
                matches!(agent, RegistryAgent::Binary(_))
                    && self.install_status(agent.id().as_ref())
                        == RegistryInstallStatus::InstalledRegistry
            })
            .map(|agent| agent.id().to_string())
            .collect::<Vec<_>>();
        let fs = <dyn Fs>::global(cx);
        self._installed_archive_versions_scan = cx.spawn(async move |this, cx| {
            let mut versions = HashMap::default();
            for registry_id in registry_ids {
                let cached = installed_registry_archive_versions(fs.as_ref(), &registry_id).await;
                if !cached.is_empty() {
                    versions.insert(registry_id, cached);
                }
            }
            this.update(cx, |this, cx| {
                if this.installed_archive_versions != versions {
                    this.installed_archive_versions = versions;
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn search_query(&self, cx: &mut App) -> Option<String> {
        let search = self.query_editor.read(cx).text(cx);
        if search.trim().is_empty() {
            None
        } else {
            Some(search)
        }
    }

    fn filter_registry_agents(&mut self, cx: &mut Context<Self>) {
        self.refresh_installed_statuses(cx);
        let search = self.search_query(cx).map(|search| search.to_lowercase());
        let filter = self.filter;
        let installed_statuses = self.installed_statuses.clone();

        let filtered_indices = self
            .registry_agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| {
                let matches_search = search.as_ref().is_none_or(|query| {
                    let query = query.as_str();
                    agent.id().as_ref().to_lowercase().contains(query)
                        || agent.name().as_ref().to_lowercase().contains(query)
                        || agent.description().as_ref().to_lowercase().contains(query)
                });

                let install_status = installed_statuses
                    .get(agent.id().as_ref())
                    .copied()
                    .unwrap_or(RegistryInstallStatus::NotInstalled);
                let matches_filter = match filter {
                    RegistryFilter::All => true,
                    RegistryFilter::Installed => {
                        install_status != RegistryInstallStatus::NotInstalled
                    }
                    RegistryFilter::NotInstalled => {
                        install_status == RegistryInstallStatus::NotInstalled
                    }
                };

                matches_search && matches_filter
            })
            .map(|(index, _)| index)
            .collect();

        self.filtered_registry_indices = filtered_indices;

        cx.notify();
    }

    fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.list.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn on_query_change(
        &mut self,
        _: Entity<Editor>,
        event: &editor::EditorEvent,
        cx: &mut Context<Self>,
    ) {
        if let editor::EditorEvent::Edited { .. } = event {
            self.filter_registry_agents(cx);
            self.scroll_to_top(cx);
        }
    }

    fn render_search(&self, cx: &mut Context<Self>) -> Div {
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("BufferSearchBar");

        h_flex()
            .key_context(key_context)
            .h_8()
            .min_w(rems_from_px(384_f32))
            .flex_1()
            .pl_1p5()
            .pr_2()
            .gap_2()
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(Icon::new(IconName::MagnifyingGlass).color(Color::Muted))
            .child(self.render_text_input(&self.query_editor, cx))
    }

    fn render_text_input(
        &self,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: if editor.read(cx).read_only(cx) {
                cx.theme().colors().text_disabled
            } else {
                cx.theme().colors().text
            },
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features.clone(),
            font_fallbacks: settings.ui_font.fallbacks.clone(),
            font_size: rems(0.875).into(),
            font_weight: settings.ui_font.weight,
            line_height: relative(1.3),
            ..Default::default()
        };

        EditorElement::new(
            editor,
            EditorStyle {
                background: cx.theme().colors().editor_background,
                local_player: cx.theme().players().local(),
                text: text_style,
                ..Default::default()
            },
        )
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_search = self.search_query(cx).is_some();
        let registry_store = self.registry_store.read(cx);
        let is_fetching = registry_store.is_fetching();
        let fetch_error = registry_store.fetch_error();

        let message = if is_fetching {
            "レジストリを読み込んでいます..."
        } else if fetch_error.is_some() {
            "エージェントレジストリの読み込みに失敗しました。接続を確認して再試行してください。"
        } else {
            match self.filter {
                RegistryFilter::All => {
                    if has_search {
                        "検索条件に一致するエージェントはありません。"
                    } else {
                        "利用できるエージェントはありません。"
                    }
                }
                RegistryFilter::Installed => {
                    if has_search {
                        "検索条件に一致するインストール済みのエージェントはありません。"
                    } else {
                        "インストール済みのエージェントはありません。"
                    }
                }
                RegistryFilter::NotInstalled => {
                    if has_search {
                        "検索条件に一致する未インストールのエージェントはありません。"
                    } else {
                        "未インストールのエージェントはありません。"
                    }
                }
            }
        };

        h_flex()
            .py_4()
            .min_w_0()
            .w_full()
            .gap_1p5()
            .items_start()
            .when(fetch_error.is_some(), |this| {
                this.child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
            })
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .child(Label::new(message))
                    .when_some(fetch_error.clone(), |this, fetch_error| {
                        this.child(
                            Label::new(fetch_error)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when_some(fetch_error, |this, _| {
                let registry_store = self.registry_store.clone();
                this.child(
                    Button::new("retry-agent-registry", "再試行")
                        .style(ButtonStyle::Outlined)
                        .size(ButtonSize::Compact)
                        .on_click(move |_, _, cx| {
                            registry_store.update(cx, |store, cx| store.refresh(cx));
                        }),
                )
            })
    }

    fn render_agents(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AgentRegistryCard> {
        range
            .map(|index| {
                let Some(agent_index) = self.filtered_registry_indices.get(index).copied() else {
                    return self.render_missing_agent();
                };
                let Some(agent) = self.registry_agents.get(agent_index) else {
                    return self.render_missing_agent();
                };
                self.render_registry_agent(agent, cx)
            })
            .collect()
    }

    fn render_missing_agent(&self) -> AgentRegistryCard {
        AgentRegistryCard::new().child(
            Label::new("レジストリの項目が見つかりません。")
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
    }

    fn render_registry_agent(
        &self,
        agent: &RegistryAgent,
        cx: &mut Context<Self>,
    ) -> AgentRegistryCard {
        let install_status = self.install_status(agent.id().as_ref());
        let supports_current_platform = agent.supports_current_platform();
        let pending_consent = self.pending_install_consent.get(agent.id().as_ref());
        let update_status = self.update_status(agent, cx);

        let icon = match agent.icon_path() {
            Some(icon_path) => Icon::from_external_svg(icon_path.clone()),
            None => Icon::new(IconName::Sparkle),
        }
        .size(IconSize::Medium)
        .color(Color::Muted);

        let install_button = self.install_button(
            agent,
            install_status,
            supports_current_platform,
            pending_consent.is_some(),
            cx,
        );

        let repository_button = agent.repository().map(|repository| {
            let repository_for_tooltip = repository.clone();
            let repository_for_click = repository.to_string();

            IconButton::new(
                SharedString::from(format!("agent-repo-{}", agent.id())),
                IconName::Github,
            )
            .icon_size(IconSize::Small)
            .tooltip(move |_, cx| {
                Tooltip::with_meta(
                    "エージェントのリポジトリを開く",
                    None,
                    repository_for_tooltip.clone(),
                    cx,
                )
            })
            .on_click(move |_, _, cx| {
                cx.open_url(&repository_for_click);
            })
        });

        let website_button = agent.website().map(|website| {
            let website = website.clone();
            let website_for_click = website.clone();
            IconButton::new(
                SharedString::from(format!("agent-website-{}", agent.id())),
                IconName::Link,
            )
            .icon_size(IconSize::Small)
            .tooltip(move |_, cx| {
                Tooltip::with_meta("エージェントの Web サイトを開く", None, website.clone(), cx)
            })
            .on_click(move |_, _, cx| {
                cx.open_url(&website_for_click);
            })
        });

        let license_button = agent.license_url().map(|license_url| {
            let license_url = license_url.clone();
            let license_url_for_click = license_url.clone();
            IconButton::new(
                SharedString::from(format!("agent-license-{}", agent.id())),
                IconName::FileTextOutlined,
            )
            .icon_size(IconSize::Small)
            .tooltip(move |_, cx| {
                Tooltip::with_meta(
                    i18n::t!("View Agent License or Terms of Service"),
                    None,
                    license_url.clone(),
                    cx,
                )
            })
            .on_click(move |_, _, cx| {
                cx.open_url(&license_url_for_click);
            })
        });

        let card = AgentRegistryCard::new()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(icon)
                            .child(Headline::new(agent.name().clone()).size(HeadlineSize::Small))
                            .child(
                                div()
                                    .id(SharedString::from(format!("agent-version-{}", agent.id())))
                                    .child(
                                        Label::new(format!("v{}", agent.version()))
                                            .color(Color::Muted),
                                    )
                                    .when(update_status == UpdateStatus::UnknownForNpx, |this| {
                                        this.tooltip(Tooltip::text(
                                            "npx で配布されるエージェントは起動時に npm が版を\
                                             決めるため、インストール済みの版を特定できません。\
                                             更新の有無はここには表示しません。",
                                        ))
                                    }),
                            )
                            .map(|this| match &update_status {
                                UpdateStatus::Available { installed, latest } => {
                                    this.child(update_available_badge(
                                        agent.id().as_ref(),
                                        installed.as_ref(),
                                        latest,
                                    ))
                                }
                                _ => this,
                            })
                            .when(!supports_current_platform, |this| {
                                this.child(
                                    Label::new("このプラットフォームには対応していません")
                                        .size(LabelSize::Small)
                                        .color(Color::Warning),
                                )
                            }),
                    )
                    .child(install_button),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                Label::new(agent.description().clone())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .children(fork_agent_note(agent.id().as_ref()).map(|note| {
                                Label::new(note).size(LabelSize::Small).color(Color::Muted)
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(format!("ID: {}", agent.id()))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .when_some(repository_button, |this, button| this.child(button))
                            .when_some(website_button, |this, button| this.child(button))
                            .when_some(license_button, |this, button| this.child(button)),
                    ),
            );

        match pending_consent {
            Some(consent) => {
                card.child(self.render_pending_consent(agent.id().as_ref(), consent, cx))
            }
            None => card,
        }
    }

    fn render_pending_consent(
        &self,
        agent_id: &str,
        consent: &PendingInstallConsent,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let agent_id_for_confirm = agent_id.to_string();
        let agent_id_for_cancel = agent_id.to_string();

        InstallConsentBanner::new(
            agent_id.to_string(),
            consent,
            InstallConsentAction::Install,
            cx.listener(move |this, _, window, cx| {
                this.confirm_install(&agent_id_for_confirm, window, cx);
            }),
            cx.listener(move |this, _, _, cx| {
                // Cancelling once the hosts are on their way would leave them
                // added with nothing installed, so it only stands until then.
                if this
                    .pending_install_consent
                    .get(&agent_id_for_cancel)
                    .is_some_and(|consent| consent.waiting)
                {
                    return;
                }
                this.pending_install_consent.remove(&agent_id_for_cancel);
                cx.notify();
            }),
        )
        .into_any_element()
    }

    /// Handles an "Install" click: proceeds immediately when nothing new is
    /// needed, otherwise opens the confirmation banner instead of writing
    /// settings right away.
    fn begin_install(&mut self, agent_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self
            .registry_agents
            .iter()
            .find(|agent| agent.id().as_ref() == agent_id)
        else {
            return;
        };
        let requirement = install_requirement(agent);
        let missing = missing_hosts(&requirement.hosts, cx);

        if missing.is_empty() && !requirement.is_npx {
            install_registry_agent(agent_id, window, cx);
            return;
        }

        self.pending_install_consent.insert(
            agent_id.to_string(),
            PendingInstallConsent::new(requirement),
        );
        cx.notify();
    }

    /// Adds whatever hosts the pending consent still needs, then proceeds
    /// with the install. A failed allowlist write leaves the banner up with
    /// an error instead of installing anyway.
    ///
    /// The install only starts once the running egress policy has the hosts:
    /// it reads the allowlist for itself (the npx install's proxy, and on
    /// Windows the ceiling the WSL sandbox snapshots), so starting it on the
    /// write alone loses the race with the allowlist's watcher and the
    /// install is refused with 511.
    fn confirm_install(&mut self, agent_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(consent) = self.pending_install_consent.get_mut(agent_id) else {
            return;
        };
        // A confirm that is already running owns this consent until it is
        // done; a second one would add the hosts twice and install twice.
        if consent.waiting {
            return;
        }
        let hosts: Vec<String> = consent.hosts.iter().map(SharedString::to_string).collect();

        if hosts.is_empty() {
            self.pending_install_consent.remove(agent_id);
            install_registry_agent(agent_id, window, cx);
            return;
        }
        consent.waiting = true;
        consent.error = None;

        let agent_id = agent_id.to_string();
        let write_hosts = add_consented_hosts_and_await_pickup(hosts, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = write_hosts.await;
            this.update_in(cx, |this, window, cx| {
                // The card may have been re-rendered away while the write was
                // in flight; only act on it if the consent this task was for
                // is still the one pending.
                if !this.pending_install_consent.contains_key(&agent_id) {
                    return;
                }
                match result {
                    Ok(()) => {
                        this.pending_install_consent.remove(&agent_id);
                        install_registry_agent(&agent_id, window, cx);
                    }
                    Err(failure) => {
                        if let Some(consent) = this.pending_install_consent.get_mut(&agent_id) {
                            consent.waiting = false;
                            consent.error = Some(failure.message());
                        }
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn install_button(
        &self,
        agent: &RegistryAgent,
        install_status: RegistryInstallStatus,
        supports_current_platform: bool,
        has_pending_consent: bool,
        cx: &mut Context<Self>,
    ) -> Button {
        let button_id = SharedString::from(format!("install-agent-{}", agent.id()));

        if !supports_current_platform {
            return Button::new(button_id, "利用不可")
                .style(ButtonStyle::OutlinedGhost)
                .disabled(true);
        }

        match install_status {
            RegistryInstallStatus::NotInstalled => {
                let agent_id = agent.id().to_string();
                Button::new(button_id, "インストール")
                    .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                    .start_icon(
                        Icon::new(IconName::Download)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    // A pending consent banner is already open below this
                    // button (F2-5); disabling it here avoids opening a
                    // second one for the same agent from a stray click.
                    .disabled(has_pending_consent)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.begin_install(&agent_id, window, cx);
                    }))
            }
            RegistryInstallStatus::InstalledRegistry => {
                let fs = <dyn Fs>::global(cx);
                let agent_id = agent.id().to_string();
                Button::new(button_id, "削除")
                    .style(ButtonStyle::OutlinedGhost)
                    .on_click(move |_, _, cx| {
                        let agent_id = agent_id.clone();
                        update_settings_file(fs.clone(), cx, move |settings, _| {
                            let Some(agent_servers) = settings.agent_servers.as_mut() else {
                                return;
                            };
                            if let Some(entry) = agent_servers.get(agent_id.as_str())
                                && matches!(
                                    entry,
                                    settings::CustomAgentServerSettings::Registry { .. }
                                )
                            {
                                agent_servers.remove(agent_id.as_str());
                            }
                        });
                    })
            }
            RegistryInstallStatus::InstalledCustom => Button::new(button_id, "インストール済み")
                .style(ButtonStyle::OutlinedGhost)
                .disabled(true),
        }
    }
}

impl Render for AgentRegistryPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .p_4()
                    .gap_4()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .justify_between()
                            .child(Headline::new("ACP レジストリ").size(HeadlineSize::Large))
                            .child(
                                Button::new("learn-more", "詳細")
                                    .style(ButtonStyle::Outlined)
                                    .size(ButtonSize::Medium)
                                    .end_icon(
                                        Icon::new(IconName::ArrowUpRight)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .on_click(move |_, _, cx| {
                                        cx.open_url(&zed_urls::acp_registry_blog(cx))
                                    }),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .gap_2()
                            .child(self.render_search(cx))
                            .child(
                                div().child(
                                    ToggleButtonGroup::single_row(
                                        "registry-filter-buttons",
                                        [
                                            ToggleButtonSimple::new(
                                                "すべて",
                                                cx.listener(|this, _event, _, cx| {
                                                    this.filter = RegistryFilter::All;
                                                    this.filter_registry_agents(cx);
                                                    this.scroll_to_top(cx);
                                                }),
                                            ),
                                            ToggleButtonSimple::new(
                                                "インストール済み",
                                                cx.listener(|this, _event, _, cx| {
                                                    this.filter = RegistryFilter::Installed;
                                                    this.filter_registry_agents(cx);
                                                    this.scroll_to_top(cx);
                                                }),
                                            ),
                                            ToggleButtonSimple::new(
                                                "未インストール",
                                                cx.listener(|this, _event, _, cx| {
                                                    this.filter = RegistryFilter::NotInstalled;
                                                    this.filter_registry_agents(cx);
                                                    this.scroll_to_top(cx);
                                                }),
                                            ),
                                        ],
                                    )
                                    .style(ToggleButtonGroupStyle::Outlined)
                                    .size(ToggleButtonGroupSize::Custom(rems_from_px(30_f32)))
                                    .label_size(LabelSize::Default)
                                    .auto_width()
                                    .selected_index(match self.filter {
                                        RegistryFilter::All => 0,
                                        RegistryFilter::Installed => 1,
                                        RegistryFilter::NotInstalled => 2,
                                    })
                                    .into_any_element(),
                                ),
                            ),
                    ),
            )
            .child(v_flex().px_4().size_full().overflow_y_hidden().map(|this| {
                let count = self.filtered_registry_indices.len();
                if count == 0 {
                    this.child(self.render_empty_state(cx)).into_any_element()
                } else {
                    let scroll_handle = &self.list;
                    this.child(
                        uniform_list("registry-entries", count, cx.processor(Self::render_agents))
                            .flex_grow_1()
                            .pb_4()
                            .track_scroll(scroll_handle),
                    )
                    .vertical_scrollbar_for(scroll_handle, window, cx)
                    .into_any_element()
                }
            }))
    }
}

impl EventEmitter<ItemEvent> for AgentRegistryPage {}

impl Focusable for AgentRegistryPage {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.query_editor.read(cx).focus_handle(cx)
    }
}

impl Item for AgentRegistryPage {
    type Event = ItemEvent;

    fn added_to_workspace(
        &mut self,
        _workspace: &mut workspace::Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An editor's focus listeners belong to the window they were
        // registered against, so one inside a tab moved to another window has
        // to register them again.
        self.query_editor.update(cx, |editor, cx| {
            editor.rebind_window_focus_listeners(window, cx);
        });
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "ACP レジストリ".into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("ACP Registry Page Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(workspace::item::ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use http_client::EgressPolicy;
    use project::AgentId;
    use project::agent_registry_store::{
        RegistryAgentMetadata, RegistryBinaryAgent, RegistryNpxAgent, RegistryTargetConfig,
    };

    fn metadata(id: &str) -> RegistryAgentMetadata {
        RegistryAgentMetadata {
            id: AgentId::new(id.to_string()),
            name: id.to_string().into(),
            description: SharedString::from(""),
            version: SharedString::from("1.0.0"),
            repository: None,
            website: None,
            license_url: None,
            icon_path: None,
        }
    }

    fn target(archive: &str) -> RegistryTargetConfig {
        RegistryTargetConfig {
            archive: archive.to_string(),
            cmd: "./agent".to_string(),
            args: Vec::new(),
            sha256: None,
            env: HashMap::default(),
        }
    }

    /// A GitHub release asset redirects to a second host, and the egress
    /// decorator re-checks every hop, so consenting to `github.com` alone
    /// would have the download refused after the user already said yes.
    #[test]
    fn binary_agent_needs_every_hop_of_a_github_release_download() {
        let platform =
            current_platform_key().expect("the test host should be a supported platform");
        let agent = RegistryAgent::Binary(RegistryBinaryAgent {
            metadata: metadata("example-agent"),
            targets: HashMap::from_iter([(
                platform.to_string(),
                target("https://github.com/example/agent/releases/download/v1/agent.tar.gz"),
            )]),
            supports_current_platform: true,
        });

        let requirement = install_requirement(&agent);
        assert!(!requirement.is_npx);
        assert_eq!(
            requirement.hosts,
            vec![
                SharedString::from("github.com"),
                SharedString::from("release-assets.githubusercontent.com"),
                SharedString::from("objects.githubusercontent.com"),
            ]
        );
    }

    /// A different distribution host per agent is the whole point: F2-5's
    /// consent flow has to read it from the manifest, not assume a fixed one.
    #[test]
    fn binary_agent_host_follows_its_own_archive_url() {
        let platform =
            current_platform_key().expect("the test host should be a supported platform");
        let agent = RegistryAgent::Binary(RegistryBinaryAgent {
            metadata: metadata("agy"),
            targets: HashMap::from_iter([(
                platform.to_string(),
                target("https://dl.google.com/agy/agy_acp_server.exe"),
            )]),
            supports_current_platform: true,
        });

        let requirement = install_requirement(&agent);
        assert_eq!(requirement.hosts, vec![SharedString::from("dl.google.com")]);
    }

    #[test]
    fn binary_agent_with_no_target_for_this_platform_needs_nothing() {
        let agent = RegistryAgent::Binary(RegistryBinaryAgent {
            metadata: metadata("example-agent"),
            targets: HashMap::from_iter([(
                "not-a-real-platform".to_string(),
                target("https://example.com/agent.tar.gz"),
            )]),
            supports_current_platform: false,
        });

        let requirement = install_requirement(&agent);
        assert!(!requirement.is_npx);
        assert!(requirement.hosts.is_empty());
    }

    #[test]
    fn npx_agent_lists_the_node_download_and_the_npm_registry() {
        let agent = RegistryAgent::Npx(RegistryNpxAgent {
            metadata: metadata("claude-acp"),
            package: SharedString::from("@agentclientprotocol/claude-agent-acp"),
            args: Vec::new(),
            env: HashMap::default(),
        });

        let requirement = install_requirement(&agent);
        assert!(requirement.is_npx);
        assert_eq!(
            requirement.hosts,
            vec![
                SharedString::from("nodejs.org"),
                SharedString::from("registry.npmjs.org"),
            ]
        );
    }

    /// The update flow reuses the install flow's banner, so an update must not
    /// read as a different kind of prompt - only the verb changes.
    #[test]
    fn consent_confirm_labels_differ_only_in_the_verb() {
        assert_eq!(
            InstallConsentAction::Install.confirm_label(false),
            "許可リストに追加してインストール"
        );
        assert_eq!(
            InstallConsentAction::Install.confirm_label(true),
            "了承してインストール"
        );
        assert_eq!(
            InstallConsentAction::Update.confirm_label(false),
            "許可リストに追加して更新"
        );
        assert_eq!(
            InstallConsentAction::Update.confirm_label(true),
            "了承して更新"
        );
    }

    fn cached(versions: &[&str]) -> Vec<String> {
        versions.iter().map(|version| version.to_string()).collect()
    }

    #[test]
    fn update_badge_follows_the_cached_version() {
        let latest = SharedString::from("1.3.0");

        assert_eq!(
            update_status(false, &latest, &cached(&["1.2.3"]), None),
            UpdateStatus::Available {
                installed: Some(SharedString::from("1.2.3")),
                latest: latest.clone(),
            }
        );
        assert_eq!(
            update_badge_label(Some(&SharedString::from("1.2.3")), &latest),
            "更新あり: v1.2.3 → v1.3.0"
        );

        assert_eq!(
            update_status(false, &latest, &cached(&["1.3.0"]), None),
            UpdateStatus::UpToDate
        );
        // The cache spells a version the way a directory name can hold it.
        assert_eq!(
            update_status(
                false,
                &SharedString::from("2.0.0+build_7"),
                &cached(&["2.0.0-build_7"]),
                None
            ),
            UpdateStatus::UpToDate
        );
        assert_eq!(
            update_status(false, &latest, &[], None),
            UpdateStatus::Unknown
        );
    }

    /// An older build a running session held open survives the sweep with the
    /// newest mtime; the latest version sitting beside it is still installed.
    #[test]
    fn update_badge_is_not_shown_when_the_latest_is_cached_behind_an_older_one() {
        let latest = SharedString::from("1.3.0");
        assert_eq!(
            update_status(false, &latest, &cached(&["1.2.3", "1.3.0"]), None),
            UpdateStatus::UpToDate
        );
        assert_eq!(
            update_status(false, &latest, &cached(&["1.2.3", "1.1.0"]), None),
            UpdateStatus::Available {
                installed: Some(SharedString::from("1.2.3")),
                latest: latest.clone(),
            }
        );
    }

    #[test]
    fn update_badge_is_never_shown_for_npx_agents() {
        let latest = SharedString::from("1.3.0");
        assert_eq!(
            update_status(true, &latest, &cached(&["1.2.3"]), Some(&latest)),
            UpdateStatus::UnknownForNpx
        );
        assert_eq!(
            update_status(true, &latest, &[], None),
            UpdateStatus::UnknownForNpx
        );
    }

    /// A running connection's report still counts when the host cache has
    /// nothing to say, e.g. for an agent that only ever ran in the WSL sandbox.
    #[test]
    fn update_badge_falls_back_to_the_running_connection() {
        let latest = SharedString::from("1.3.0");
        let reported = SharedString::from("1.4.0");
        assert_eq!(
            update_status(false, &latest, &[], Some(&reported)),
            UpdateStatus::Available {
                installed: None,
                latest: reported.clone(),
            }
        );
        assert_eq!(update_badge_label(None, &reported), "更新あり: v1.4.0");
        assert_eq!(
            update_status(false, &latest, &cached(&["1.2.3"]), Some(&reported)),
            UpdateStatus::Available {
                installed: Some(SharedString::from("1.2.3")),
                latest: latest.clone(),
            }
        );
    }

    /// The connection's report is never withdrawn, so after a new thread has
    /// installed the announced version the badge must not linger.
    #[test]
    fn update_badge_clears_once_the_reported_version_is_cached() {
        let latest = SharedString::from("1.3.0");
        assert_eq!(
            update_status(false, &latest, &cached(&["1.3.0"]), Some(&latest)),
            UpdateStatus::UpToDate
        );
    }

    /// The thread's update callout re-derives the hosts from the registry
    /// rather than assuming the install's consent still covers them, so the
    /// lookup has to answer for an agent the registry lists and stay silent
    /// for one it cannot describe.
    #[gpui::test]
    fn update_consent_reads_its_hosts_from_the_registry(cx: &mut gpui::TestAppContext) {
        let platform =
            current_platform_key().expect("the test host should be a supported platform");
        let agent = RegistryAgent::Binary(RegistryBinaryAgent {
            metadata: metadata("agy"),
            targets: HashMap::from_iter([(
                platform.to_string(),
                target("https://dl.google.com/agy/agy_acp_server.exe"),
            )]),
            supports_current_platform: true,
        });

        cx.update(|cx| {
            AgentRegistryStore::init_test_global(cx, vec![agent]);

            let requirement = registry_install_requirement(&AgentId::new("agy"), cx)
                .expect("the registry should describe an agent it lists");
            assert_eq!(requirement.hosts, vec![SharedString::from("dl.google.com")]);

            // No egress policy has been published in this test, so every host
            // reads as missing - the deliberate fail-safe direction.
            assert_eq!(missing_hosts(&requirement.hosts, cx), requirement.hosts);

            // A custom or built-in agent, or a registry that never loaded:
            // no consent step, the download stays gated either way.
            assert!(registry_install_requirement(&AgentId::new("not-in-registry"), cx).is_none());
        });
    }

    /// ADR-F49 MEDIUM-5: a consent names hosts. A pattern would open the
    /// IDE-wide allowlist to more than the user was asked about, so it is
    /// refused rather than written.
    #[gpui::test]
    async fn a_wildcard_is_never_added_by_a_consent(cx: &mut gpui::TestAppContext) {
        let refused = cx
            .update(|cx| {
                add_consented_hosts(
                    vec!["api.anthropic.com".to_string(), "*.example.com".to_string()],
                    cx,
                )
            })
            .await
            .expect_err("a pattern is refused");
        assert!(
            format!("{refused:#}").contains("*.example.com"),
            "{refused:#}"
        );

        let refused = cx
            .update(|cx| add_consented_hosts(vec!["*".to_string()], cx))
            .await
            .expect_err("so is everything");
        assert!(format!("{refused:#}").contains('*'), "{refused:#}");
    }

    fn denying_policy() -> EgressPolicyHandle {
        EgressPolicyHandle::new(EgressPolicy::deny_all())
    }

    fn policy_allowing(hosts: &[&str]) -> EgressPolicy {
        let (policy, errors) = EgressPolicy::from_host_strings(hosts);
        assert!(errors.is_empty(), "{errors:?}");
        policy
    }

    /// The write to `network_allowlist.json` is taken in by a watcher, so
    /// whatever a consent leads to would be refused with 511 if it started on
    /// the write alone.
    #[gpui::test]
    async fn a_consent_waits_until_the_running_policy_has_the_host(cx: &mut gpui::TestAppContext) {
        let policy = denying_policy();
        let mut changes = policy.subscribe();
        let executor = cx.executor();
        let finished = Arc::new(AtomicBool::new(false));
        let wait = cx.background_executor.spawn({
            let policy = policy.clone();
            let executor = executor.clone();
            let finished = finished.clone();
            async move {
                let result = await_allowlist_pickup(
                    &["registry.npmjs.org".to_string()],
                    &policy,
                    &mut changes,
                    &executor,
                )
                .await;
                finished.store(true, Ordering::SeqCst);
                result
            }
        });

        cx.run_until_parked();
        assert!(
            !finished.load(Ordering::SeqCst),
            "nothing may proceed while the policy still denies the host"
        );

        // A reload that brings something else in leaves the wait where it was.
        policy.set(policy_allowing(&["example.com"]));
        cx.run_until_parked();
        assert!(!finished.load(Ordering::SeqCst));

        policy.set(policy_allowing(&["example.com", "registry.npmjs.org"]));
        wait.await
            .expect("the wait ends once the policy allows the host");
    }

    #[gpui::test]
    async fn a_consent_gives_up_when_the_policy_never_takes_the_host(
        cx: &mut gpui::TestAppContext,
    ) {
        let policy = denying_policy();
        let mut changes = policy.subscribe();
        let executor = cx.executor();
        let wait = cx.background_executor.spawn({
            let policy = policy.clone();
            let executor = executor.clone();
            async move {
                await_allowlist_pickup(
                    &["registry.npmjs.org".to_string()],
                    &policy,
                    &mut changes,
                    &executor,
                )
                .await
            }
        });

        cx.executor()
            .advance_clock(ALLOWLIST_PICKUP_TIMEOUT + Duration::from_secs(1));
        let error = wait
            .await
            .expect_err("a policy that never takes the host is not waited on forever");
        assert!(
            format!("{error:#}").contains(ALLOWLIST_NOT_APPLIED),
            "{error:#}"
        );
    }

    /// The whole point of the wait: the install is what reads the running
    /// policy (the npx install's proxy, and on Windows the ceiling the WSL
    /// sandbox snapshots), so it must not start on the write alone.
    #[gpui::test]
    async fn confirm_install_starts_nothing_until_the_policy_has_the_hosts(
        cx: &mut gpui::TestAppContext,
    ) {
        let (page, policy, mut cx) = registry_page_with_a_pending_consent(cx).await;
        let cx = &mut cx;

        page.update_in(cx, |page, window, cx| {
            page.confirm_install(NPX_AGENT_ID, window, cx)
        });
        cx.run_until_parked();
        let waiting = page.read_with(cx, |page, _| {
            page.pending_install_consent
                .get(NPX_AGENT_ID)
                .map(|consent| (consent.waiting, consent.error.clone()))
        });
        assert_eq!(
            waiting,
            Some((true, None)),
            "the consent is still on screen, locked, while the policy denies the hosts"
        );

        // Neither button does anything while the consent is being carried out:
        // a second confirm would install twice, and a cancel would leave the
        // hosts added with nothing installed.
        page.update_in(cx, |page, window, cx| {
            page.confirm_install(NPX_AGENT_ID, window, cx)
        });
        cx.run_until_parked();
        assert!(
            page.read_with(cx, |page, _| page
                .pending_install_consent
                .contains_key(NPX_AGENT_ID)),
            "a second confirm is ignored rather than starting another install"
        );

        policy.set(policy_allowing(&["nodejs.org", "registry.npmjs.org"]));
        cx.run_until_parked();
        assert!(
            page.read_with(cx, |page, _| page.pending_install_consent.is_empty()),
            "once the policy has the hosts the install goes ahead"
        );
    }

    /// A watcher that never takes the write in says so - the hosts *were*
    /// added, and saying they could not be sends the user looking for a
    /// problem that is not there.
    #[gpui::test]
    async fn confirm_install_says_the_hosts_were_added_when_nothing_picks_them_up(
        cx: &mut gpui::TestAppContext,
    ) {
        let (page, _policy, mut cx) = registry_page_with_a_pending_consent(cx).await;
        let cx = &mut cx;

        page.update_in(cx, |page, window, cx| {
            page.confirm_install(NPX_AGENT_ID, window, cx)
        });
        cx.run_until_parked();
        cx.executor()
            .advance_clock(ALLOWLIST_PICKUP_TIMEOUT + Duration::from_secs(1));
        cx.run_until_parked();

        let consent = page.read_with(cx, |page, _| {
            page.pending_install_consent
                .get(NPX_AGENT_ID)
                .map(|consent| (consent.waiting, consent.error.clone()))
        });
        assert_eq!(
            consent,
            Some((false, Some(ALLOWLIST_NOT_APPLIED.into()))),
            "the banner is usable again and says the hosts are in the file"
        );
    }

    const NPX_AGENT_ID: &str = "claude-acp";

    /// A registry page showing the consent banner for an npx agent, with an
    /// egress policy that allows nothing yet.
    async fn registry_page_with_a_pending_consent(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<AgentRegistryPage>,
        EgressPolicyHandle,
        gpui::VisualTestContext,
    ) {
        crate::test_support::init_test(cx);
        let fs = fs::FakeFs::new(cx.executor());
        let policy = EgressPolicyHandle::new(EgressPolicy::deny_all());
        cx.update(|cx| {
            <dyn Fs>::set_global(fs.clone(), cx);
            cx.set_global(egress_policy::EgressAllowlistGlobal(policy.clone()));
            AgentRegistryStore::init_test_global(
                cx,
                vec![RegistryAgent::Npx(RegistryNpxAgent {
                    metadata: metadata(NPX_AGENT_ID),
                    package: SharedString::from("@agentclientprotocol/claude-agent-acp"),
                    args: Vec::new(),
                    env: HashMap::default(),
                })],
            );
        });
        // The consent writes into this file, which has no directory yet in a
        // fake file system.
        let allowlist = paths::network_allowlist_file().clone();
        if let Some(directory) = allowlist.parent() {
            fs.create_dir(directory)
                .await
                .expect("the fake file system should take the allowlist's directory");
        }

        let project = project::Project::test(fs, [], cx).await;
        let window =
            cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window should be open");
        let mut visual_cx = gpui::VisualTestContext::from_window(window.into(), cx);
        let cx = &mut visual_cx;
        let page = workspace.update_in(cx, |workspace, window, cx| {
            AgentRegistryPage::new(workspace, window, cx)
        });
        cx.run_until_parked();

        page.update_in(cx, |page, window, cx| {
            page.begin_install(NPX_AGENT_ID, window, cx)
        });
        assert!(
            page.read_with(cx, |page, _| page
                .pending_install_consent
                .contains_key(NPX_AGENT_ID)),
            "an npx agent always asks first"
        );
        (page, policy, visual_cx)
    }
}
