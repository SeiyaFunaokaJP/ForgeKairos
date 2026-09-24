//! ステータスバーの「IDE サーバー」項目と、ワークスペースに登録するアクション。
//!
//! 表示するのはサーバーが有効なあいだ (ADR-F09「AI 機能は既定オフ」) と、サンドボックスの
//! 中の CLI が接続しているあいだだけ。有効化は設定 GUI の「外部通信と AI」から行い、
//! ここからは無効化と、外部エージェント向けの設定断片のコピー・ロックファイルの場所を
//! 開く操作を出す。

use std::path::Path;
use std::sync::Arc;

use editor::Editor;
use gpui::{Anchor, App, ClipboardItem, Empty, Entity, Subscription, WeakEntity};
use settings::{Settings as _, update_settings_file};
use ui::{ContextMenu, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*};
use workspace::{
    StatusItemView, Toast, Workspace, item::ItemHandle, notifications::NotificationId,
};
use zed_actions::ide_server::{
    CopyMcpConfiguration, RevealLockFile, SendSelectionToClaudeCode, ToggleIdeServer,
};

use crate::{
    IdeServer, IdeServerSettings, sandbox_client, server_for, server_for_workspace_id, tools,
};

/// トーストはどの操作でも 1 つに畳む。連打しても積み上がらない。
struct IdeServerNotification;

pub(crate) fn register_workspace_actions(workspace: &mut Workspace) {
    workspace
        .register_action(|workspace, _: &ToggleIdeServer, _window, cx| {
            let enabled = IdeServerSettings::get_global(cx).enabled;
            let fs = workspace.app_state().fs.clone();
            update_settings_file(fs, cx, move |settings, _| {
                settings.ide_server.get_or_insert_default().enabled = Some(!enabled);
            });
        })
        .register_action(|workspace, _: &CopyMcpConfiguration, _window, cx| {
            copy_mcp_configuration(workspace, cx);
        })
        .register_action(|workspace, _: &RevealLockFile, _window, cx| {
            let server = server_for(&cx.entity(), cx);
            let path = server
                .as_ref()
                .and_then(|server| server.read(cx).lock_file_path().map(Path::to_path_buf));
            match path {
                Some(path) => cx.reveal_path(&path),
                None if server.is_some_and(|server| server.read(cx).port().is_some()) => notify(
                    workspace,
                    "リモートのワークスペースではロックファイルを書きません。\
                     CLI からは「MCP 設定をコピー」で貼れる HTTP のエンドポイントを使ってください",
                    cx,
                ),
                None => notify(workspace, "IDE サーバーは動いていません", cx),
            }
        })
        .register_action(|workspace, _: &SendSelectionToClaudeCode, _window, cx| {
            send_selection_to_claude_code(workspace, cx);
        });
}

fn copy_mcp_configuration(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let endpoint = server_for(&cx.entity(), cx).and_then(|server| server.read(cx).mcp_endpoint());
    let Some(endpoint) = endpoint else {
        notify(workspace, "IDE サーバーは動いていません", cx);
        return;
    };
    match endpoint.to_configuration_json() {
        Ok(json) => {
            cx.write_to_clipboard(ClipboardItem::new_string(json));
            notify(
                workspace,
                "MCP 設定をコピーしました。認証トークンは Zed を起動するたびに変わるので、\
                 貼り付け先も毎回貼り直してください",
                cx,
            );
        }
        Err(error) => notify(
            workspace,
            format!("MCP 設定を作れませんでした: {error:#}"),
            cx,
        ),
    }
}

fn send_selection_to_claude_code(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(server) = server_for(&cx.entity(), cx) else {
        notify(workspace, "IDE サーバーは動いていません", cx);
        return;
    };
    let editor = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx));
    let Some(editor) = editor else {
        notify(workspace, "送る対象のエディターがありません", cx);
        return;
    };
    // 選択が無ければ `at_mention_range` はカーソル行を開始 = 終了として返す。
    let Some((file_path, start_line, end_line)) = tools::at_mention_range(&editor, cx) else {
        notify(
            workspace,
            "保存されていないバッファーは Claude Code に送れません",
            cx,
        );
        return;
    };
    let mut sent = server.update(cx, |server, cx| {
        server.send_at_mention(&file_path, start_line, end_line, cx)
    });
    // サンドボックスの中の CLI にも送る。ワークツリーの外のファイルはセッションが断る。
    for session in sandbox_client::sessions_for_workspace(cx.entity_id(), cx) {
        sent |= session.update(cx, |session, cx| {
            session.send_at_mention(&file_path, start_line, end_line, cx)
        });
    }
    if !sent {
        notify(workspace, "Claude Code が接続していません", cx);
    }
}

/// 設定の「AI」ページの同じ項目と揃えた文言。
const SHARE_ACTIVE_FILE_LABEL: &str = "開いているファイルを Claude Code に伝える";

fn set_share_active_file(fs: Arc<dyn fs::Fs>, share_active_file: bool, cx: &App) {
    update_settings_file(fs, cx, move |settings, _| {
        settings
            .ide_server
            .get_or_insert_default()
            .share_active_file = Some(share_active_file);
    });
}

/// チェックの状態はメニューを開いたときの、切り替え先は押したときの設定から決める。描画のときに
/// 読んだ値を持ち回ると、設定の AI ページで切り替えたあとに押しても何も変わらないことがある。
fn share_active_file_entry(menu: ContextMenu, fs: Arc<dyn fs::Fs>, cx: &App) -> ContextMenu {
    menu.toggleable_entry(
        SHARE_ACTIVE_FILE_LABEL,
        IdeServerSettings::get_global(cx).share_active_file,
        IconPosition::Start,
        None,
        move |_window, cx| {
            let share_active_file = IdeServerSettings::get_global(cx).share_active_file;
            set_share_active_file(fs.clone(), !share_active_file, cx);
        },
    )
}

fn notify(workspace: &mut Workspace, message: impl Into<String>, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(
            NotificationId::unique::<IdeServerNotification>(),
            message.into(),
        ),
        cx,
    );
}

pub struct IdeServerStatusItem {
    server: Option<Entity<IdeServer>>,
    fs: Arc<dyn fs::Fs>,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    subscriptions: Vec<Subscription>,
}

impl IdeServerStatusItem {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let workspace_id = workspace.weak_handle().entity_id();
        // `initialize_workspace` は Workspace をリースしたまま走るので、サーバーの
        // 購読はリースの外に出す。ここで `IdeServer` を読むと二重リースになる。
        let status_item: WeakEntity<Self> = cx.weak_entity();
        cx.defer(move |cx| {
            let Some(status_item) = status_item.upgrade() else {
                return;
            };
            let Some(server) = server_for_workspace_id(workspace_id, cx) else {
                return;
            };
            status_item.update(cx, |this, cx| {
                this.subscriptions
                    .push(cx.observe(&server, |_, _, cx| cx.notify()));
                this.server = Some(server);
                cx.notify();
            });
        });
        Self {
            server: None,
            fs: workspace.app_state().fs.clone(),
            menu_handle: PopoverMenuHandle::default(),
            subscriptions: Vec::new(),
        }
    }

    pub(crate) fn shown(&self, cx: &App) -> Option<Shown> {
        if IdeServerSettings::get_global(cx).enabled {
            Some(Shown::HostServer)
        } else if self.claude_code_connected(cx) {
            Some(Shown::SandboxConnection)
        } else {
            None
        }
    }

    /// ホストのサーバーとサンドボックスのセッションを合わせて、CLI が 1 本でも繋がっているか。
    fn claude_code_connected(&self, cx: &App) -> bool {
        self.server.as_ref().is_some_and(|server| {
            zed_actions::ide_server::is_claude_code_connected(server.read(cx).project_id, cx)
        })
    }

    /// 繋がっている CLI (ホストの WebSocket・サンドボックスのセッション) と、ホストのサーバーを
    /// 使うアシストのクライアント (ホストで起動した CLI のタブ・ACP の接続) の数。
    pub(crate) fn connection_count(&self, cx: &App) -> usize {
        self.server.as_ref().map_or(0, |server| {
            let server = server.read(cx);
            server.connected_client_count()
                + server.assist_client_count()
                + sandbox_client::connected_session_count(server.project_id, cx)
        })
    }
}

/// ADR-F56: エージェントに渡している IDE ツールの一覧 (ツールチップ・メニューに出す)。
pub(crate) fn assist_tools_summary(cx: &App) -> &'static str {
    if IdeServerSettings::get_global(cx).assist_tools {
        "IDE ツール: ハイライト・注釈・ツアー・定義・参照・シンボル検索"
    } else {
        "IDE ツール: 無効 (設定の「AI」で切り替え)"
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Shown {
    /// ホストのサーバーと、その操作メニュー。
    HostServer,
    /// サーバーは無効だが、サンドボックスの中の CLI が繋がっている。サンドボックスの
    /// セッションは `ide_server.enabled` と無関係に動くので (ADR-F49)、接続中であることと、
    /// 開いているファイルを伝えるかどうかの切り替えだけは出す。
    SandboxConnection,
}

impl Render for IdeServerStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(shown) = self.shown(cx) else {
            return Empty.into_any_element();
        };
        let assist_summary = assist_tools_summary(cx);
        let connection_count = self.connection_count(cx);
        let trigger = match shown {
            Shown::SandboxConnection => {
                let tooltip: SharedString = format!(
                    "サンドボックスの中の Claude Code がこのプロジェクトに接続しています \
                     (接続 {connection_count})。クリックで操作メニューを開きます\n{assist_summary}"
                )
                .into();
                Button::new(
                    "ide-server-status-trigger",
                    "Claude Code (サンドボックス): 接続中",
                )
                .label_size(LabelSize::Small)
                .tab_index(0isize)
                .start_icon(
                    Icon::new(IconName::Server)
                        .size(IconSize::Small)
                        .color(Color::Accent),
                )
                .tooltip(Tooltip::text(tooltip))
            }
            Shown::HostServer => {
                let claude_code_connected = self.claude_code_connected(cx);
                let (port, connected_clients, has_lock_file) = self.host_server_state(cx);
                let label: SharedString = match port {
                    Some(port) if connected_clients > 0 => {
                        format!("IDE サーバー: {port} (接続中 {connected_clients})").into()
                    }
                    Some(port) => format!("IDE サーバー: {port}").into(),
                    None => "IDE サーバー: 起動中…".into(),
                };
                let tooltip: SharedString = match port {
                    Some(port) if !has_lock_file => format!(
                        "127.0.0.1:{port} で待ち受け中 (接続 {connection_count})。リモートのワークスペース\
                         なので、CLI が自動で見つけるためのロックファイルは書いていません \
                         (「MCP 設定をコピー」で貼れば繋がります)\n{assist_summary}"
                    )
                    .into(),
                    Some(port) => format!(
                        "127.0.0.1:{port} で待ち受け中 (接続 {connection_count})。\
                         クリックで操作メニューを開きます\n{assist_summary}"
                    )
                    .into(),
                    None => "待ち受けポートの確保を待っています".into(),
                };
                Button::new("ide-server-status-trigger", label)
                    .label_size(LabelSize::Small)
                    .tab_index(0isize)
                    .start_icon(Icon::new(IconName::Server).size(IconSize::Small).color(
                        if connected_clients > 0 || claude_code_connected {
                            Color::Accent
                        } else {
                            Color::Muted
                        },
                    ))
                    .tooltip(Tooltip::text(tooltip))
            }
        };

        PopoverMenu::new("ide-server-status")
            .trigger(trigger)
            .anchor(Anchor::BottomRight)
            .with_handle(self.menu_handle.clone())
            .menu(self.menu_builder(shown, cx))
            .into_any_element()
    }
}

impl IdeServerStatusItem {
    /// ホストのサーバーの (IDE の口の待ち受けポート, 繋がっている CLI の数, ロックファイルを書いたか)。
    fn host_server_state(&self, cx: &App) -> (Option<u16>, usize, bool) {
        match self.server.as_ref() {
            Some(server) => {
                let server = server.read(cx);
                (
                    server.port(),
                    server.connected_client_count(),
                    server.lock_file_path().is_some(),
                )
            }
            None => (None, 0, false),
        }
    }

    /// 項目を押したときのメニュー。サンドボックスの CLI だけが繋がっているときも、開いている
    /// ファイルを伝えるかどうかはここで切り替えられるようにする (サーバーの操作は出さない)。
    pub(crate) fn menu_builder(
        &self,
        shown: Shown,
        cx: &App,
    ) -> impl Fn(&mut Window, &mut App) -> Option<Entity<ContextMenu>> + use<> {
        let header: SharedString = format!(
            "{} (接続 {})",
            assist_tools_summary(cx),
            self.connection_count(cx)
        )
        .into();
        let (port, _, has_lock_file) = self.host_server_state(cx);
        let is_running = port.is_some();
        let fs = self.fs.clone();
        move |window, cx| {
            let header = header.clone();
            let fs = fs.clone();
            Some(ContextMenu::build(window, cx, move |menu, _window, cx| {
                let menu = menu.header(header);
                let menu = match shown {
                    Shown::HostServer => menu
                        .action("無効にする", Box::new(ToggleIdeServer))
                        .action_disabled_when(
                            !is_running,
                            "MCP 設定をコピー",
                            Box::new(CopyMcpConfiguration),
                        )
                        .action_disabled_when(
                            !is_running || !has_lock_file,
                            "ロックファイルの場所を開く",
                            Box::new(RevealLockFile),
                        )
                        .separator(),
                    Shown::SandboxConnection => menu,
                };
                share_active_file_entry(menu, fs, cx)
            }))
        }
    }
}

impl StatusItemView for IdeServerStatusItem {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    /// 出し入れは `ide_server.enabled` と接続の有無だけで決まるので、独立した非表示設定は持たない。
    fn hide_setting(&self, _cx: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}
