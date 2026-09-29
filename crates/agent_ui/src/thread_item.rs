//! エージェントスレッドをワークスペースのタブとして開くためのアイテム (ADR-F36)。
//!
//! エージェントパネルが同時に見せられるスレッドは 1 本きりだが、スレッド自体は
//! 以前から裏で並行に走っている。足りないのは「表示枠」だけなので、
//! [`ConversationView`] をパネルから**取り出して**普通の [`Item`] に載せる。
//! 載ってしまえば ADR-F33 / ADR-F34 のタブ機構 (分割・別パネルへのドラッグ・
//! 窓外へ引き出して新しいウィンドウ) がそのまま効く。
//!
//! ## 1 つの `ConversationView` は 1 か所にしか出さない
//!
//! 同じエンティティを 2 か所で描くとフォーカスもスクロールも壊れるので、
//! タブへ移す操作は必ず「パネルから取り上げる」= [`AgentPanel`] の `base_view` /
//! `draft_thread` / `retained_threads` のどこにも残さない。逆に
//! `agent::MoveThreadToPanel` はタブを閉じてからパネルへ渡す。すでにタブで
//! 開いているスレッドをパネルが開こうとしたときは、奪い返さずそのタブを前に
//! 出す ([`ThreadItem::activate_existing`])。
//!
//! この判定はウィンドウをまたぐうえ、パネルの更新中 (= ワークスペースが
//! リース中かもしれない) から呼ばれるので、ワークスペースを読みに行かずに
//! 済むグローバルの索引 [`OpenThreadItems`] で持つ。

use std::collections::HashMap;

use acp_thread::AcpThreadEvent;
use anyhow::{Result, anyhow};
use db::kvp::KeyValueStore;
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Global, KeyContext, SharedString, Subscription, Task, WeakEntity,
    Window,
};
use project::Project;
use serde::{Deserialize, Serialize};
use ui::{IconButton, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    MultiWorkspace, Pane, SplitDirection, Workspace, WorkspaceId,
    item::{Item, ItemEvent},
};

use crate::conversation_view::RootThreadUpdated;
use crate::thread_metadata_store::ThreadId;
use crate::{Agent, AgentPanel, ConversationView, MoveThreadToPanel};

/// 永続化に使う KVP のスコープ。実際の名前空間はここにワークスペース ID を
/// 足したもので、その中はタブ 1 枚 = 1 レコード
/// ([`thread_item_namespace`])。旧形式 (このスコープ直下に、ワークスペース ID
/// を鍵として全タブを 1 行へ詰めたもの) は読むだけ。
const THREAD_ITEM_KEY: &str = "agent_thread_items";

/// いまタブとして開かれているスレッドの索引。
#[derive(Default)]
struct OpenThreadItems {
    by_thread: HashMap<ThreadId, OpenThreadItem>,
}

struct OpenThreadItem {
    item: WeakEntity<ThreadItem>,
    window: AnyWindowHandle,
}

impl Global for OpenThreadItems {}

/// スレッドの返し先として見つかったエージェントパネルと、それが載っている場所。
struct HostingPanel {
    window: AnyWindowHandle,
    workspace: Entity<Workspace>,
    panel: Entity<AgentPanel>,
}

/// このアイテムがどのタブ列に載っているか。エージェントパネルの中では
/// タイトルとツールバーをパネル側が出すので、アイテムの見出しは重複になる
/// (ADR-F36 Phase B)。`BrowserItem` の `BrowserHost` と同じ考え方で、載った
/// 先が実行時に教える。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadItemHost {
    /// エディター領域 / ターミナルパネルなど、パネルの外のタブ列。
    EditorTab,
    /// エージェントパネルの中のペイン。
    Panel,
}

/// 1 タブ = 1 本のエージェントスレッド。
pub struct ThreadItem {
    conversation_view: Entity<ConversationView>,
    workspace: WeakEntity<Workspace>,
    host: ThreadItemHost,
    /// このアイテムを最初に載せたワークスペース。別ウィンドウへ移されると
    /// `workspace` は移った先を指すが、切り離しウィンドウにはエージェント
    /// パネルが無い ([`Workspace::suppress_dock_auto_open`] がドックを閉じた
    /// まま作る) ので、スレッドの返し先はこちらも当たる。
    origin_workspace: WeakEntity<Workspace>,
    /// `origin_workspace` が載っていたウィンドウ。
    origin_window: AnyWindowHandle,
    weak_self: WeakEntity<Self>,
    /// このアイテムが載っているウィンドウ。別ウィンドウへ移されたら
    /// [`Item::added_to_workspace`] で張り替える。
    window: AnyWindowHandle,
    thread_id: ThreadId,
    /// スレッドをパネルへ返し終えたか。返す経路が複数あるので二重に渡さない。
    handed_back: bool,
    /// このアイテムが載っているペインが変わるたびに増える。[`Item::on_removed`]
    /// は「閉じた」ときも「別のペインへ移した」ときも呼ばれるので、外された
    /// あとにこの値が動いたかどうかで両者を見分ける。
    pane_generation: u32,
    /// 「外されたのは閉じたからか、移されたからか」の判定タスク。
    /// [`Item::added_to_workspace`] が落として取り消す。
    pending_hand_back: Option<Task<()>>,
    /// 直近タブに出した名前。スレッドは些細なことでも `notify` するので、
    /// 名前が変わったときだけ [`ItemEvent::UpdateTab`] を出すために持つ。
    tab_title: SharedString,
    /// いまタイトル更新を購読しているルートスレッド。
    watched_thread: Option<gpui::EntityId>,
    _thread_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl ThreadItem {
    fn new(
        conversation_view: Entity<ConversationView>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let thread_id = conversation_view.read(cx).thread_id;
        let tab_title = conversation_view.read(cx).tab_title(cx);

        let subscriptions = vec![
            cx.observe(&conversation_view, |this, conversation_view, cx| {
                this.watch_root_thread(&conversation_view, cx);
                this.refresh_tab_title(&conversation_view, cx);
            }),
            cx.subscribe(
                &conversation_view,
                |this, conversation_view, _: &RootThreadUpdated, cx| {
                    this.watch_root_thread(&conversation_view, cx);
                    this.refresh_tab_title(&conversation_view, cx);
                },
            ),
        ];

        // 取りこぼしの受け皿。ウィンドウごと閉じた等で [`Item::on_removed`] を
        // 通らずにエンティティが落ちたときも、走っているスレッドを捨てない。
        cx.on_release(|this, cx| {
            this.hand_back_to_panel(cx);
        })
        .detach();

        let mut this = Self {
            conversation_view: conversation_view.clone(),
            origin_workspace: workspace.clone(),
            origin_window: window.window_handle(),
            workspace,
            host: ThreadItemHost::EditorTab,
            weak_self: cx.weak_entity(),
            window: window.window_handle(),
            thread_id,
            handed_back: false,
            pane_generation: 0,
            pending_hand_back: None,
            tab_title,
            watched_thread: None,
            _thread_subscription: None,
            _subscriptions: subscriptions,
        };
        this.watch_root_thread(&conversation_view, cx);
        this
    }

    /// タイトルが変わったらタブ名を打ち直す。ルートスレッドは接続が確立して
    /// から生えるので、生え変わったときだけ張り替える。
    fn watch_root_thread(
        &mut self,
        conversation_view: &Entity<ConversationView>,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = conversation_view.read(cx).root_thread(cx) else {
            return;
        };
        if self.watched_thread == Some(thread.entity_id()) {
            return;
        }
        self.watched_thread = Some(thread.entity_id());
        self._thread_subscription = Some(cx.subscribe(
            &thread,
            |this, _thread, event: &AcpThreadEvent, cx| {
                if matches!(event, AcpThreadEvent::TitleUpdated) {
                    let conversation_view = this.conversation_view.clone();
                    this.refresh_tab_title(&conversation_view, cx);
                }
            },
        ));
    }

    fn refresh_tab_title(
        &mut self,
        conversation_view: &Entity<ConversationView>,
        cx: &mut Context<Self>,
    ) {
        let title = conversation_view.read(cx).tab_title(cx);
        if self.tab_title == title {
            return;
        }
        self.tab_title = title;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    pub fn conversation_view(&self) -> &Entity<ConversationView> {
        &self.conversation_view
    }

    pub fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    /// このアイテムはもうスレッドの持ち主ではない、と宣言する。呼び出し側が
    /// `ConversationView` を引き取ってからタブを外すときに使う: これをしないと
    /// [`Item::on_removed`] がスレッドをパネルの保持枠へ返し、同じ
    /// `ConversationView` を 2 か所が持つ。
    pub(crate) fn give_up_thread(&mut self, cx: &mut Context<Self>) {
        self.handed_back = true;
        self.pending_hand_back = None;
        Self::unregister(self.thread_id, &self.weak_self.clone(), cx);
    }

    /// 載った先のタブ列を教える。[`Item::added_to_workspace`] が既定の
    /// 「エディター領域」に戻すので、パネルは `AddItem` のたびに上書きする
    /// ([`crate::AgentPanel::handle_pane_event`])。
    pub(crate) fn set_host(&mut self, host: ThreadItemHost, cx: &mut Context<Self>) {
        if self.host == host {
            return;
        }
        self.host = host;
        cx.notify();
    }

    /// エージェントパネルが自分のペインへ載せるためのアイテムを 1 枚作る。
    /// 索引 ([`OpenThreadItems`]) への登録までここで済ませる。
    pub(crate) fn for_panel(
        conversation_view: Entity<ConversationView>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let item = cx.new(|cx| {
            let mut item = Self::new(conversation_view, workspace, window, cx);
            item.host = ThreadItemHost::Panel;
            item
        });
        let (thread_id, item_window) = {
            let item = item.read(cx);
            (item.thread_id, item.window)
        };
        Self::register(thread_id, item.downgrade(), item_window, cx);
        item
    }

    /// このタブが見ているスレッドを 1 行書く。
    ///
    /// ワークスペースの直列化キューからも呼ばれる ([`SerializableItem::serialize`])
    /// が、そちらは 200 ms 刻みで流れるのに対し、このタブを名指しする側 (中央の
    /// ペインの保存、パネルのレイアウト) はもっと早く書かれる。その間に終了すると
    /// 「レイアウトが指すタブのレコードが無い」状態が残るので、ペインに載った
    /// 時点でも 1 回書く ([`Item::added_to_workspace`])。中身はタブの一生を通じて
    /// 変わらないので、何度書いても同じ。
    fn write_record(
        &self,
        workspace_id: WorkspaceId,
        item_id: workspace::ItemId,
        cx: &App,
    ) -> Task<Result<()>> {
        let entry = SerializedThreadItem {
            thread_id: self.thread_id,
            agent: self.conversation_view.read(cx).agent_key().clone(),
        };
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            let payload = serde_json::to_string(&entry)?;
            kvp.scoped(&thread_item_namespace(workspace_id))
                .write(item_id.to_string(), payload)
                .await
        })
    }

    fn register(
        thread_id: ThreadId,
        item: WeakEntity<Self>,
        window: AnyWindowHandle,
        cx: &mut App,
    ) {
        cx.default_global::<OpenThreadItems>()
            .by_thread
            .insert(thread_id, OpenThreadItem { item, window });
    }

    fn unregister(thread_id: ThreadId, item: &WeakEntity<Self>, cx: &mut App) {
        let Some(open) = cx.try_global::<OpenThreadItems>() else {
            return;
        };
        if open
            .by_thread
            .get(&thread_id)
            .is_some_and(|entry| entry.item.entity_id() == item.entity_id())
        {
            cx.default_global::<OpenThreadItems>()
                .by_thread
                .remove(&thread_id);
        }
    }

    /// そのスレッドがどこか (中央 / パネル / 別ウィンドウ) のタブに載っているか。
    /// 載っているスレッドはパネルの `retained_threads` に預けない
    /// (預けると同じ `ConversationView` を 2 か所が持つ)。
    pub(crate) fn is_open_in_a_tab(thread_id: ThreadId, cx: &App) -> bool {
        cx.try_global::<OpenThreadItems>().is_some_and(|open| {
            open.by_thread
                .get(&thread_id)
                .is_some_and(|entry| entry.item.upgrade().is_some())
        })
    }

    /// そのスレッドのタブが、このワークスペースのものか。同じウィンドウでも
    /// 別のプロジェクトのワークスペースなら、同じスレッド ID の下書きが別々に
    /// 生きていてよい (それぞれのパネルが自分のコピーを持つ)。
    pub(crate) fn is_open_in_workspace(
        thread_id: ThreadId,
        workspace: &WeakEntity<Workspace>,
        cx: &App,
    ) -> bool {
        let Some((item, _window)) = Self::registered_item(thread_id, cx) else {
            return false;
        };
        item.read(cx).workspace.entity_id() == workspace.entity_id()
    }

    /// その `ConversationView` を載せているタブのアイテム ID。
    ///
    /// スレッド ID だけで引かないのは、同じスレッドの `ConversationView` が
    /// 2 つ生きうるため (別ワークスペースのパネルがそれぞれ自分のコピーを持つ)。
    /// 別のコピーを載せたタブを見て「自分のは他所へ移った」と誤解しない。
    pub(crate) fn item_showing_view(
        thread_id: ThreadId,
        conversation_view: gpui::EntityId,
        cx: &App,
    ) -> Option<gpui::EntityId> {
        let (item, _window) = Self::registered_item(thread_id, cx)?;
        let item = item.read(cx);
        (item.conversation_view.entity_id() == conversation_view)
            .then(|| item.weak_self.entity_id())
    }

    /// そのスレッドがすでにタブで開かれているなら、そのタブを前に出して `true`。
    ///
    /// ワークスペースを読まないので、パネルの更新中からでも安全に呼べる。実際に
    /// タブを出すのは次の機会に回す。
    pub fn activate_existing(thread_id: ThreadId, cx: &mut App) -> bool {
        let Some(entry) = cx
            .try_global::<OpenThreadItems>()
            .and_then(|open| open.by_thread.get(&thread_id))
        else {
            return false;
        };
        let window = entry.window;
        let weak_item = entry.item.clone();
        let Some(item) = weak_item.upgrade() else {
            Self::unregister(thread_id, &weak_item, cx);
            return false;
        };
        let workspace = item.read(cx).workspace.clone();

        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    let activated = workspace
                        .update(cx, |workspace, cx| {
                            workspace.activate_item(&item, true, true, window, cx)
                        })
                        .unwrap_or(false);
                    if activated {
                        window.activate_window();
                    }
                })
                .log_err();
        });
        true
    }

    /// そのスレッドを載せているアイテムと、それがあるウィンドウ。
    fn registered_item(thread_id: ThreadId, cx: &App) -> Option<(Entity<Self>, AnyWindowHandle)> {
        let entry = cx
            .try_global::<OpenThreadItems>()?
            .by_thread
            .get(&thread_id)?;
        Some((entry.item.upgrade()?, entry.window))
    }

    /// パネルがいま見せているスレッドを中央のタブへ**移す** (ADR-F36 Phase B)。
    /// パネルもペインを持つようになったので、`ConversationView` を取り出して
    /// 包み直すのではなく、アイテムをそのままペイン間で移す。
    pub fn open_active_panel_thread(
        workspace: &mut Workspace,
        split: Option<SplitDirection>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
            return;
        };
        let Some((source_pane, item_id)) = panel.read(cx).active_thread_item(cx) else {
            return;
        };
        Self::move_item_into_center(workspace, &source_pane, item_id, split, window, cx);
    }

    /// すでにアイテムになっているスレッドを中央のペインへ移す。別のウィンドウに
    /// あるときは移さずにそのタブを前に出す (窓をまたぐ移動はタブのドラッグに任せる)。
    /// 移した / 前に出したときだけ `true`。
    fn move_existing_into_center(
        workspace: &mut Workspace,
        thread_id: ThreadId,
        split: Option<SplitDirection>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some((item, item_window)) = Self::registered_item(thread_id, cx) else {
            return false;
        };
        if item_window.window_id() != window.window_handle().window_id() {
            return Self::activate_existing(thread_id, cx);
        }
        let item_id = item.entity_id();
        let Some(source_pane) = workspace
            .all_panes(cx)
            .into_iter()
            .find(|pane| pane.read(cx).items().any(|item| item.item_id() == item_id))
        else {
            return false;
        };
        Self::move_item_into_center(workspace, &source_pane, item_id, split, window, cx);
        true
    }

    fn move_item_into_center(
        workspace: &mut Workspace,
        source_pane: &Entity<Pane>,
        item_id: gpui::EntityId,
        split: Option<SplitDirection>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(direction) = split {
            let item = source_pane.update(cx, |pane, cx| {
                let item = pane
                    .items()
                    .find(|item| item.item_id() == item_id)?
                    .boxed_clone();
                pane.remove_item(item_id, false, false, window, cx);
                Some(item)
            });
            if let Some(item) = item {
                workspace.split_item(direction, item, window, cx);
            }
            return;
        }

        let destination = workspace.active_pane().clone();
        if &destination == source_pane {
            destination.update(cx, |pane, cx| {
                let index = pane.items().position(|item| item.item_id() == item_id);
                if let Some(index) = index {
                    pane.activate_item(index, true, true, window, cx);
                }
            });
            return;
        }
        let destination_index = destination.read(cx).items_len();
        workspace::move_item(
            source_pane,
            &destination,
            item_id,
            destination_index,
            true,
            window,
            cx,
        );
    }

    /// 指定のスレッドをタブで開く。すでにアイテムになっていれば移すだけ、
    /// パネルが `ConversationView` だけ持っていれば取り上げ、どちらでも
    /// なければ読み込む。パネルは開かない (閉じたままなら閉じたまま)。
    pub fn open_thread(
        workspace: &mut Workspace,
        agent: Agent,
        thread_id: ThreadId,
        split: Option<SplitDirection>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if Self::move_existing_into_center(workspace, thread_id, split, window, cx) {
            return;
        }
        let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
            return;
        };
        let conversation_view = panel.update(cx, |panel, cx| {
            panel.detach_thread_for_tab(agent, thread_id, window, cx)
        });
        Self::add(workspace, conversation_view, split, window, cx);
    }

    fn add(
        workspace: &mut Workspace,
        conversation_view: Entity<ConversationView>,
        split: Option<SplitDirection>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let workspace_handle = workspace.weak_handle();
        let item = cx.new(|cx| ThreadItem::new(conversation_view, workspace_handle, window, cx));
        let (thread_id, item_window) = {
            let item = item.read(cx);
            (item.thread_id, item.window)
        };
        Self::register(thread_id, item.downgrade(), item_window, cx);
        match split {
            Some(direction) => workspace.split_item(direction, Box::new(item), window, cx),
            None => workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx),
        }
    }

    /// スレッドの返し先になるエージェントパネルを探す。
    ///
    /// 窓外へ引き出したタブのウィンドウにはパネルが無い
    /// (`suppress_dock_auto_open` がドックを閉じたまま作り、パネルは後から
    /// 足される) ので、いまのワークスペース → 最初に載っていたワークスペース →
    /// 同じプロジェクトを開いている他のウィンドウ、の順に当たる。
    /// どこにも無ければ `None` = スレッドを手放してはいけない。
    ///
    /// 最初の 2 つはワークスペースのエンティティを読むだけで済ませる。
    /// [`Self::move_to_panel`] はウィンドウの更新中から呼ばれ、そのあいだ
    /// 当のウィンドウは `App` から抜き取られているので、ルートビューを読む
    /// ([`gpui::WindowHandle::read`]) と必ず失敗するため。
    fn find_panel(&self, cx: &mut App) -> Option<HostingPanel> {
        for (window, workspace) in [
            (self.window, &self.workspace),
            (self.origin_window, &self.origin_workspace),
        ] {
            if let Some(workspace) = workspace.upgrade()
                && let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx)
            {
                return Some(HostingPanel {
                    window,
                    workspace,
                    panel,
                });
            }
        }

        let project = self
            .workspace
            .upgrade()
            .or_else(|| self.origin_workspace.upgrade())
            .map(|workspace| workspace.read(cx).project().entity_id())?;

        for window in cx.windows() {
            let Some(multi_workspace) = window.downcast::<MultiWorkspace>() else {
                continue;
            };
            // 更新中のウィンドウはここでは読めない。上の 2 つで拾えているので
            // 読めなかったぶんは飛ばす。
            let Ok(workspaces) = multi_workspace
                .read(cx)
                .map(|multi_workspace| multi_workspace.workspaces().cloned().collect::<Vec<_>>())
            else {
                continue;
            };
            for workspace in workspaces {
                if workspace.read(cx).project().entity_id() != project {
                    continue;
                }
                if let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                    return Some(HostingPanel {
                        window,
                        workspace,
                        panel,
                    });
                }
            }
        }

        None
    }

    fn move_to_panel(
        &mut self,
        _: &MoveThreadToPanel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.handed_back {
            return;
        }
        // パネルを先に探す。閉じてから探すと、見つからなかったときに生成中の
        // スレッドごと消える。
        let Some(target) = self.find_panel(cx) else {
            self.report_missing_panel(cx);
            return;
        };

        // 同じウィンドウなら、`ConversationView` を渡し直すのではなくアイテム
        // ごとパネルのペインへ移す (ADR-F36 Phase B)。スクロール位置も購読も
        // そのまま付いていく。
        if target.window.window_id() == self.window.window_id() {
            self.move_item_to_panel_pane(target, cx);
            return;
        }

        self.handed_back = true;
        Self::unregister(self.thread_id, &self.weak_self.clone(), cx);

        let conversation_view = self.conversation_view.clone();
        let HostingPanel {
            window: target_window,
            workspace,
            ..
        } = target;
        let workspace = workspace.downgrade();
        // パネルが別のウィンドウにあるとき (切り離しウィンドウから戻すとき) だけ
        // そちらを前に出す。同じウィンドウなら余計に叩かない。
        let another_window = target_window.window_id() != self.window.window_id();
        cx.emit(ItemEvent::CloseItem);
        // このアイテムを抱えているペインは、いままさにこのアイテムを更新中。
        // パネルへ渡すのは次の機会に回す。パネルが別のウィンドウにあることも
        // あるので (切り離しウィンドウから戻すとき)、そちらの `Window` で走らせる。
        cx.defer(move |cx: &mut App| {
            target_window
                .update(cx, |_, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                                log::warn!(
                                    "agent panel disappeared before the thread could be moved back"
                                );
                                return;
                            };
                            panel.update(cx, |panel, cx| {
                                panel.show_detached_thread(conversation_view, window, cx);
                            });
                            workspace.focus_panel::<AgentPanel>(window, cx);
                        })
                        .log_err();
                    if another_window {
                        window.activate_window();
                    }
                })
                .log_err();
        });
    }

    /// アイテムをそのままパネルのアクティブなペインへ移す (同じウィンドウのとき)。
    ///
    /// 呼ばれた時点でこのアイテムを抱えているペインは更新中 (メニューや
    /// ボタンのハンドラーは `pane.update` の中で走る) なので、実際の移動は
    /// `defer` で抜けてから。
    fn move_item_to_panel_pane(&mut self, target: HostingPanel, cx: &mut Context<Self>) {
        let item_id = self.weak_self.entity_id();
        let workspace = target.workspace.downgrade();
        let target_window = target.window;
        cx.defer(move |cx: &mut App| {
            target_window
                .update(cx, |_, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                                log::warn!(
                                    "agent panel disappeared before the thread could be moved back"
                                );
                                return;
                            };
                            let destination = panel.read(cx).active_pane().clone();
                            let source = workspace.all_panes(cx).into_iter().find(|pane| {
                                pane.read(cx).items().any(|item| item.item_id() == item_id)
                            });
                            if let Some(source) = source
                                && source != destination
                            {
                                let destination_index = destination.read(cx).items_len();
                                workspace::move_item(
                                    &source,
                                    &destination,
                                    item_id,
                                    destination_index,
                                    true,
                                    window,
                                    cx,
                                );
                            }
                            workspace.focus_panel::<AgentPanel>(window, cx);
                        })
                        .log_err();
                })
                .log_err();
        });
    }

    /// パネルが見つからないときに、タブを閉じずに理由を知らせる。
    fn report_missing_panel(&self, cx: &mut Context<Self>) {
        log::warn!("no agent panel to move this thread back to; keeping the tab open");
        let Some(workspace) = self
            .workspace
            .upgrade()
            .or_else(|| self.origin_workspace.upgrade())
        else {
            return;
        };
        cx.defer(move |cx: &mut App| {
            workspace.update(cx, |workspace, cx| {
                struct MissingAgentPanelToast;
                workspace.show_toast(
                    workspace::Toast::new(
                        workspace::notifications::NotificationId::unique::<MissingAgentPanelToast>(
                        ),
                        i18n::t!(
                            "The thread cannot be moved back because the agent panel was not found."
                        ),
                    )
                    .autohide(),
                    cx,
                );
            });
        });
    }

    /// ペインから外されたあと、「閉じた」のか「移した」のかを見極めてから
    /// パネルへ返す。詳しい理由は [`Item::on_removed`] のコメント参照。
    fn schedule_hand_back(&mut self, cx: &mut Context<Self>) {
        let generation = self.pane_generation;
        // 自分への強参照を判定が済むまで握る。ペインから外れたアイテムは
        // この効果サイクルのうちに解放されてしまい、次のティックまで生き
        // 残らない。フューチャーは一巡で終わるので循環は残らない。
        let this = cx.entity();
        self.pending_hand_back = Some(cx.spawn(async move |_, cx| {
            this.update(cx, |this, cx| {
                // 移動なら、ここまでに移動先の `add_item` が済んでいて
                // `pane_changed` が動いているか、少なくともどこかのペインが
                // このアイテムを抱えている。
                if this.pane_generation != generation || this.is_hosted_by_a_pane(cx) {
                    return;
                }
                this.hand_back_to_panel(cx);
            });
        }));
    }

    /// このアイテムを、いまどこかのワークスペースのペインが抱えているか。
    /// 別ウィンドウへ移されたあとは `workspace` が移動先を指している
    /// ([`Item::added_to_workspace`] が張り替える) ので、そこを見れば足りる。
    ///
    /// 見るのは `Workspace::all_panes` = 中央の分割**とドックのパネルが抱えて
    /// いるペイン**。エージェントパネル自身がペインを持つようになった
    /// (ADR-F36 Phase B) ので、中央だけを見ていると「パネルへ移した」を
    /// 「閉じた」と誤判定して、同じスレッドがタブとパネルの保持枠に二重で
    /// 載る。
    fn is_hosted_by_a_pane(&self, cx: &App) -> bool {
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        let item_id = self.weak_self.entity_id();
        workspace
            .read(cx)
            .all_panes(cx)
            .into_iter()
            .any(|pane| pane.read(cx).items().any(|item| item.item_id() == item_id))
    }

    /// タブが閉じられたときの後始末。走っているスレッドをその場で捨てると
    /// 生成が止まるので、パネルの保持枠へ返す。
    fn hand_back_to_panel(&mut self, cx: &mut App) {
        let weak_self = self.weak_self.clone();
        Self::unregister(self.thread_id, &weak_self, cx);
        if self.handed_back {
            return;
        }

        let Some(target) = self.find_panel(cx) else {
            // 返し先が無い。ここで捨てると生成中のスレッドが黙って止まるが、
            // タブはもう外れているので抱え直す先も無い。せめて黙って消えない
            // ように残す。ワークスペースごと消えているとき (= 終了時) は
            // 正常な経路なので騒がない。
            if self.workspace.upgrade().is_some() || self.origin_workspace.upgrade().is_some() {
                log::warn!(
                    "closing an agent thread tab with no agent panel to hand the thread back to"
                );
            }
            return;
        };
        self.handed_back = true;

        let conversation_view = self.conversation_view.clone();
        target.panel.update(cx, |panel, cx| {
            panel.reclaim_detached_thread(conversation_view, cx);
        });
    }

    /// パネルではツールバーが持っているタイトル欄を、タブでは自前で出す。
    /// それ以外のツールバー要素 (モデル / モードなど) はスレッド本体の中に
    /// あるので持ち込まない。
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let conversation_view = self.conversation_view.clone();
        let title_editor = conversation_view
            .read(cx)
            .root_thread_view()
            .map(|thread_view| thread_view.read(cx).title_editor.clone());
        let focus_handle = conversation_view.read(cx).focus_handle(cx);

        h_flex()
            .flex_none()
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(match title_editor {
                Some(title_editor) => div()
                    .flex_1()
                    .min_w_0()
                    .on_action({
                        let conversation_view = conversation_view.downgrade();
                        move |_: &menu::Confirm, window, cx| {
                            if let Some(conversation_view) = conversation_view.upgrade() {
                                conversation_view
                                    .read(cx)
                                    .activation_focus_handle(cx)
                                    .focus(window, cx);
                            }
                        }
                    })
                    .on_action({
                        let conversation_view = conversation_view.downgrade();
                        move |_: &editor::actions::Cancel, window, cx| {
                            if let Some(conversation_view) = conversation_view.upgrade() {
                                conversation_view
                                    .read(cx)
                                    .activation_focus_handle(cx)
                                    .focus(window, cx);
                            }
                        }
                    })
                    .child(title_editor)
                    .into_any_element(),
                None => div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(conversation_view.read(cx).title(cx))
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .into_any_element(),
            })
            .child(
                IconButton::new("move-thread-to-panel", IconName::Minimize)
                    .icon_size(IconSize::Small)
                    .tooltip(move |_window, cx| {
                        Tooltip::for_action_in(
                            i18n::t!("Move Back to Panel"),
                            &MoveThreadToPanel,
                            &focus_handle,
                            cx,
                        )
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.move_to_panel(&MoveThreadToPanel, window, cx);
                    })),
            )
    }
}

impl EventEmitter<ItemEvent> for ThreadItem {}

impl Focusable for ThreadItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.conversation_view.read(cx).focus_handle(cx)
    }
}

impl Render for ThreadItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // キーマップの `AgentPanel` 文脈 (と `AgentPanel > Markdown` の
        // ctrl-c) を中央のタブでも立てる。スレッド本体の `AcpThread` /
        // `MessageEditor` 文脈は `ThreadView` 側が張るので、送信・改行・Esc は
        // そのまま効く。`AgentThreadTab` は「パネルではなくタブ」を後から
        // キーマップで区別するための札 (ctrl-tab をタブ送りに戻す等)。
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("AgentPanel");
        // `AgentThreadTab` は「パネルではなく外のタブ列」の札 (ctrl-tab を
        // `tab_switcher::Toggle` に戻す)。パネルの中のタブに付けると、パネルの
        // ctrl-tab (スレッド切り替え) が中央のタブ送りに化けるので付けない。
        if self.host == ThreadItemHost::EditorTab {
            key_context.add("AgentThreadTab");
        }

        // パネルの中ではタイトル欄もツールバーもパネルが 1 本だけ出すので、
        // アイテムの見出しは重複になる (タブ名がスレッド名を兼ねる)。
        let shows_header = self.host == ThreadItemHost::EditorTab;

        v_flex()
            .key_context(key_context)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(Self::move_to_panel))
            .when(shows_header, |this| this.child(self.render_header(cx)))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .size_full()
                    .child(self.conversation_view.clone()),
            )
    }
}

impl Item for ThreadItem {
    type Event = ItemEvent;

    fn include_in_nav_history() -> bool {
        false
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.conversation_view.read(cx).tab_title(cx)
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let agent = self.conversation_view.read(cx).agent_key().clone();
        Some(Icon::new(agent.icon().unwrap_or(IconName::ZedAgent)))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    /// パンくずも検索バーも無いので、ツールバーごと消す。
    fn show_toolbar(&self) -> bool {
        false
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // ここまで来た = このアイテムはまたペインに載っている。直前の
        // [`Item::on_removed`] は「閉じた」ではなく「移した」だったので、
        // 予約してあったパネルへの返却を取り消す。
        self.pending_hand_back = None;
        self.handed_back = false;
        // 既定は「パネルの外」。パネルへ入れられたときは、パネルが
        // `AddItem` でこの直後に上書きする。
        self.host = ThreadItemHost::EditorTab;

        // 別ウィンドウへ移されるとワークスペースごと変わる。旧ウィンドウを
        // 参照したままだと、パネルに戻す操作も索引も消えた窓を見に行く。
        let window_changed = self.window.window_id() != window.window_handle().window_id();
        self.window = window.window_handle();

        let handle = workspace.weak_handle();
        let workspace_changed = self.workspace.entity_id() != handle.entity_id();
        if workspace_changed {
            self.workspace = handle.clone();
            self.conversation_view.update(cx, |conversation_view, cx| {
                conversation_view.set_workspace(handle.clone(), cx);
            });
        }

        if window_changed || workspace_changed {
            // フォーカスリスナーも通知の購読も登録先のウィンドウに紐づくので、
            // 窓をまたいだら張り直す。ワークスペースだけ変わった場合も、通知が
            // 見に行くエージェントパネルが変わるので張り直す。
            self.conversation_view.update(cx, |conversation_view, cx| {
                conversation_view.rebind_window_focus_listeners(window, cx);
            });
        }

        let weak_self = cx.weak_entity();
        Self::register(self.thread_id, weak_self, self.window, cx);

        if let Some(workspace_id) = workspace.database_id() {
            self.write_record(workspace_id, cx.entity_id().as_u64(), cx)
                .detach_and_log_err(cx);
        }
    }

    fn pane_changed(&mut self, _new_pane_id: gpui::EntityId, _cx: &mut Context<Self>) {
        self.pane_generation = self.pane_generation.wrapping_add(1);
    }

    /// タブが閉じられたらスレッドをパネルへ返す。走っているスレッドをその場で
    /// 捨てると生成が止まるので、待機中のものと同じ保持枠に預ける。
    ///
    /// ここは**ペイン間の移動でも**呼ばれる (`workspace::move_item` は「元の
    /// ペインから外す → 移動先へ足す」なので)。判定は移動が終わってから
    /// でないとできない。
    ///
    /// `cx.defer` を重ねるのでは足りない: `Workspace::move_item_to_window` /
    /// `move_item_to_new_window` は「元のペインから外す」を
    /// `source_pane.update_in(...)` で行うので、そのアプリ更新の
    /// `flush_effects` が (入れ子の分も含めて) `Defer` を全部吐き出してしまい、
    /// 移動先へ足す前に判定が走る。非同期タスクはこの `update_in` の中では
    /// 進まないので、判定は `cx.spawn` に載せて 1 ポール分だけ後ろへ回す。
    ///
    /// エンティティの解放 (`on_release`) では判定できない: ウィンドウは直前の
    /// フレームの要素ツリーを持ち続けるので、タブを閉じても次の描画までは
    /// アイテムが生きている。
    fn on_removed(&self, cx: &mut Context<Self>) {
        let weak_self = self.weak_self.clone();
        cx.defer(move |cx: &mut App| {
            let Some(this) = weak_self.upgrade() else {
                return;
            };
            this.update(cx, |this, cx| {
                this.schedule_hand_back(cx);
            });
        });
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedThreadItem {
    thread_id: ThreadId,
    agent: Agent,
}

/// 旧形式 (ワークスペースにつき 1 行へ全タブを詰めたもの)。読むだけ。
#[derive(Debug, Default, Serialize, Deserialize)]
struct SerializedThreadItems {
    /// アイテム ID (文字列) → そのタブが見ているスレッド。
    items: HashMap<String, SerializedThreadItem>,
}

/// タブ 1 枚分のレコードを置く名前空間。ワークスペースごとに分ける。
fn thread_item_namespace(workspace_id: WorkspaceId) -> String {
    format!("{THREAD_ITEM_KEY}/{}", i64::from(workspace_id))
}

fn read_legacy_thread_items(
    workspace_id: WorkspaceId,
    kvp: &KeyValueStore,
) -> SerializedThreadItems {
    kvp.scoped(THREAD_ITEM_KEY)
        .read(&i64::from(workspace_id).to_string())
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<SerializedThreadItems>(&json).log_err())
        .unwrap_or_default()
}

fn read_thread_item(
    workspace_id: WorkspaceId,
    item_id: workspace::ItemId,
    kvp: &KeyValueStore,
) -> Option<SerializedThreadItem> {
    let key = item_id.to_string();
    let entry = kvp
        .scoped(&thread_item_namespace(workspace_id))
        .read(&key)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<SerializedThreadItem>(&json).log_err());
    if entry.is_some() {
        return entry;
    }
    read_legacy_thread_items(workspace_id, kvp)
        .items
        .remove(&key)
}

impl workspace::SerializableItem for ThreadItem {
    fn serialized_item_kind() -> &'static str {
        "AgentThread"
    }

    /// 掃除は復元の直後 = このワークスペースのタブがまだ自分のレコードを書いて
    /// いる最中にも走る。「読んで、名前空間ごと消して、書き戻す」だと読んだ後に
    /// 着いた書き込みを巻き込んで消してしまい、そのタブは次の起動で復元できなく
    /// なる ([`ScopedKeyValueStore::retain`] のコメント)。
    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            kvp.scoped(&thread_item_namespace(workspace_id))
                .retain(alive_items.iter().map(|id| id.to_string()).collect())
                .await
        })
    }

    /// 1 タブ = 1 レコード。ワークスペースにつき 1 行へ全タブを詰めていた頃は、
    /// `Workspace::serialize_items` が並行に走らせる保存タスクと `cleanup` が
    /// 同じ行を read-modify-write して、後に書いた方が相手の分を消していた。
    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        Some(self.write_record(workspace.database_id()?, item_id, cx))
    }

    /// 保存する中身 (スレッド id とエージェント) はタブの一生を通じて変わら
    /// ないので、ワークスペースに載った時点の 1 回きりで足りる。イベントごとに
    /// 積むと、ワークスペースの直列化キューがアイテムの強参照を握り続けて、
    /// 閉じたタブが解放されなくなる (= スレッドをパネルへ返せなくなる)。
    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }

    fn deserialize(
        _project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let kvp = KeyValueStore::global(cx);
        let Some(entry) = read_thread_item(workspace_id, item_id, &kvp) else {
            return Task::ready(Err(anyhow!("no serialized agent thread tab")));
        };

        window.spawn(cx, async move |cx| {
            // エージェントパネルはドックの復元と一緒に後から生えるので、中央
            // ペインの復元より遅い。生えるまで待ってから借りる。
            let panel = wait_for_agent_panel(&workspace, cx).await?;
            cx.update(|window, cx| {
                if Self::is_open_in_a_tab(entry.thread_id, cx) {
                    // 同じスレッドを 2 枚のタブに復元しない。
                    return Err(anyhow!("agent thread is already open in another tab"));
                }
                let conversation_view = panel.update(cx, |panel, cx| {
                    panel.detach_thread_for_tab(entry.agent.clone(), entry.thread_id, window, cx)
                });
                let item =
                    cx.new(|cx| ThreadItem::new(conversation_view, workspace.clone(), window, cx));
                let (thread_id, item_window) = {
                    let item = item.read(cx);
                    (item.thread_id, item.window)
                };
                Self::register(thread_id, item.downgrade(), item_window, cx);
                Ok(item)
            })?
        })
    }
}

/// パネルが `workspace` に載るまで待つ。復元の順番はワークスペース側の都合で
/// 決まる (中央ペイン → ドック) ので、素直に待つ。
async fn wait_for_agent_panel(
    workspace: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<AgentPanel>> {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
    const MAX_POLLS: usize = 200;

    for _ in 0..MAX_POLLS {
        // パネル自身が自分のタブ列を復元しているあいだは、まだワークスペースに
        // 載っていない (`load` が返るのが先)。そのときだけ「読み込み中の
        // パネル」から借りる — 待つと相互待ちで固まる (ADR-F36 Phase B)。
        let panel = workspace.update(cx, |workspace, cx| {
            workspace
                .panel::<AgentPanel>(cx)
                .or_else(|| AgentPanel::loading_for_workspace(&workspace.weak_handle(), cx))
        })?;
        if let Some(panel) = panel {
            return Ok(panel);
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
    Err(anyhow!("agent panel was never added to this workspace"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentThreadSource;
    use crate::conversation_view::tests::init_test;
    use acp_thread::StubAgentConnection;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use serde_json::json;
    use std::path::Path;

    async fn setup_workspace(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, gpui::WindowHandle<MultiWorkspace>) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        (workspace, multi_workspace)
    }

    /// パネルを載せ、スタブのエージェントで下書きスレッドを 1 本開いた状態にする。
    fn setup_panel(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<AgentPanel> {
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        crate::test_support::set_stub_agent_connection(StubAgentConnection::new());
        panel.update_in(cx, |panel, window, cx| {
            panel.test_set_selected_agent(Agent::Stub);
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();
        panel
    }

    fn thread_items(
        workspace: &Entity<Workspace>,
        cx: &VisualTestContext,
    ) -> Vec<Entity<ThreadItem>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<ThreadItem>(cx).collect()
        })
    }

    fn active_view(panel: &Entity<AgentPanel>, cx: &VisualTestContext) -> Entity<ConversationView> {
        panel.read_with(cx, |panel, _cx| {
            panel
                .active_conversation_view()
                .expect("the panel should be showing a thread")
                .clone()
        })
    }

    #[gpui::test]
    async fn test_opening_a_thread_in_a_tab_moves_the_view_out_of_the_panel(
        cx: &mut TestAppContext,
    ) {
        let (workspace, window) = setup_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = setup_panel(&workspace, cx);

        let moved_view = active_view(&panel, cx);
        let moved_thread_id = moved_view.read_with(cx, |view, _cx| view.thread_id);

        workspace.update_in(cx, |workspace, window, cx| {
            ThreadItem::open_active_panel_thread(workspace, None, window, cx);
        });
        cx.run_until_parked();

        let items = thread_items(&workspace, cx);
        assert_eq!(items.len(), 1, "the thread should now live in a tab");
        items[0].read_with(cx, |item, _cx| {
            assert_eq!(
                item.conversation_view().entity_id(),
                moved_view.entity_id(),
                "the tab must show the very same ConversationView, not a copy"
            );
            assert_eq!(item.thread_id(), moved_thread_id);
        });

        panel.read_with(cx, |panel, cx| {
            let active = panel
                .active_conversation_view()
                .expect("the panel should fall back to a draft");
            assert_ne!(
                active.entity_id(),
                moved_view.entity_id(),
                "the panel must not keep rendering the thread it handed to the tab"
            );
            assert!(
                panel.active_thread_is_draft(cx),
                "the panel should be left on an empty draft"
            );
        });
    }

    #[gpui::test]
    async fn test_opening_a_thread_that_is_already_a_tab_activates_that_tab(
        cx: &mut TestAppContext,
    ) {
        let (workspace, window) = setup_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = setup_panel(&workspace, cx);

        let moved_view = active_view(&panel, cx);
        let (thread_id, agent) =
            moved_view.read_with(cx, |view, _cx| (view.thread_id, view.agent_key().clone()));

        workspace.update_in(cx, |workspace, window, cx| {
            ThreadItem::open_active_panel_thread(workspace, None, window, cx);
        });
        cx.run_until_parked();

        // Asking for the same thread again must not build a second view.
        workspace.update_in(cx, |workspace, window, cx| {
            ThreadItem::open_thread(workspace, agent.clone(), thread_id, None, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(thread_items(&workspace, cx).len(), 1);

        // And neither must the panel steal it back.
        panel.update_in(cx, |panel, window, cx| {
            panel.load_agent_thread(
                agent,
                thread_id,
                None,
                None,
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(thread_items(&workspace, cx).len(), 1);
        panel.read_with(cx, |panel, _cx| {
            assert_ne!(
                panel
                    .active_conversation_view()
                    .expect("the panel should still show its draft")
                    .entity_id(),
                moved_view.entity_id(),
                "the panel must activate the tab instead of taking the thread back"
            );
        });
    }

    #[gpui::test]
    async fn test_moving_a_thread_back_to_the_panel(cx: &mut TestAppContext) {
        let (workspace, window) = setup_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = setup_panel(&workspace, cx);

        let moved_view = active_view(&panel, cx);

        workspace.update_in(cx, |workspace, window, cx| {
            ThreadItem::open_active_panel_thread(workspace, None, window, cx);
        });
        cx.run_until_parked();

        let item = thread_items(&workspace, cx)
            .pop()
            .expect("the thread should be in a tab");
        item.update_in(cx, |item, window, cx| {
            item.move_to_panel(&MoveThreadToPanel, window, cx);
        });
        cx.run_until_parked();

        assert!(
            thread_items(&workspace, cx).is_empty(),
            "the tab should be closed once the thread is back in the panel"
        );
        panel.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel
                    .active_conversation_view()
                    .expect("the panel should show a thread again")
                    .entity_id(),
                moved_view.entity_id(),
                "the panel should show the same ConversationView it handed out"
            );
        });
    }

    /// 窓をまたいでタブを移しても、スレッドはタブに残る (パネルへ返らない)。
    ///
    /// `Workspace::move_item_to_window` は「元のペインから外す」を
    /// `source_pane.update_in(...)` で行う。そのアプリ更新の `flush_effects` が
    /// 入れ子の `Defer` まで同じループで吐き出してしまうので、返却の判定を
    /// `cx.defer` に載せていた頃は移動先へ足す前に判定が走り、パネルが
    /// `ConversationView` を抱え込んだまま別ウィンドウのタブがそれを描く、と
    /// いう二重描画になっていた。
    #[gpui::test]
    async fn test_moving_a_thread_tab_to_another_window_keeps_it_in_the_tab(
        cx: &mut TestAppContext,
    ) {
        let (workspace, window) = setup_workspace(cx).await;
        // 2 枚目のウィンドウは 1 枚目の `AppState` を使い回す。`test_new` は
        // 毎回 `WorkspaceStore` を作るので、同じ proto ハンドラーを二度登録して
        // パニックする。
        let (project, app_state) = workspace.read_with(cx, |workspace, _cx| {
            (workspace.project().clone(), workspace.app_state().clone())
        });
        let target_window = cx.add_window(|window, cx| {
            let target =
                cx.new(|cx| Workspace::new(Default::default(), project, app_state, window, cx));
            MultiWorkspace::new(target, window, cx)
        });
        let (target_workspace, target_pane) = target_window
            .update(cx, |multi_workspace, _window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let pane = workspace.read(cx).active_pane().clone();
                (workspace, pane)
            })
            .expect("the window that was just opened is open");

        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = setup_panel(&workspace, cx);

        let moved_view = active_view(&panel, cx);
        let moved_thread_id = moved_view.read_with(cx, |view, _cx| view.thread_id);
        workspace.update_in(cx, |workspace, window, cx| {
            ThreadItem::open_active_panel_thread(workspace, None, window, cx);
        });
        cx.run_until_parked();

        // 最後のタブが出ていくと `close_if_emptied_detached_window` が元の窓ごと
        // 閉じてしまうので、居残り用のタブをもう 1 枚置いておく。
        workspace.update_in(cx, |workspace, window, cx| {
            ThreadItem::open_active_panel_thread(workspace, None, window, cx);
        });
        cx.run_until_parked();

        let item = thread_items(&workspace, cx)
            .into_iter()
            .find(|item| {
                item.read_with(cx, |item, _cx| {
                    item.conversation_view().entity_id() == moved_view.entity_id()
                })
            })
            .expect("the thread should be in a tab");
        let item_id = item.entity_id();
        let source_pane = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panes()
                .iter()
                .find(|pane| pane.read(cx).items_of_type::<ThreadItem>().next().is_some())
                .cloned()
                .expect("the thread tabs should be in a pane")
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.move_item_to_window(
                source_pane.clone(),
                item_id,
                target_window,
                Some(target_pane.clone()),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _cx| {
            assert!(
                !panel.test_retains_thread(moved_thread_id),
                "a tab moved to another window must not park its thread in the panel"
            );
        });
        assert!(
            !source_pane.read_with(cx, |pane, _cx| pane
                .items_of_type::<ThreadItem>()
                .any(|hosted| hosted.entity_id() == item_id)),
            "the moved tab is gone from the pane it came from"
        );

        let hosted = target_pane.read_with(cx, |pane, _cx| {
            pane.items_of_type::<ThreadItem>().collect::<Vec<_>>()
        });
        assert_eq!(
            hosted.len(),
            1,
            "the tab should live in the other window now"
        );
        assert_eq!(hosted[0].entity_id(), item_id);
        hosted[0].read_with(cx, |item, _cx| {
            assert_eq!(
                item.conversation_view().entity_id(),
                moved_view.entity_id(),
                "the moved tab must still render the very same ConversationView"
            );
            assert!(
                !item.handed_back,
                "the thread was never handed back, so \"Move Back to Panel\" must still work"
            );
            assert_eq!(
                item.workspace.entity_id(),
                target_workspace.entity_id(),
                "the item should now belong to the workspace it was moved into"
            );
        });
    }

    #[gpui::test]
    async fn test_opening_a_thread_in_a_split_creates_a_second_pane(cx: &mut TestAppContext) {
        let (workspace, window) = setup_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let _panel = setup_panel(&workspace, cx);

        let (panes_before, first_pane) = workspace.read_with(cx, |workspace, _cx| {
            (workspace.panes().len(), workspace.panes()[0].clone())
        });

        workspace.update_in(cx, |workspace, window, cx| {
            ThreadItem::open_active_panel_thread(
                workspace,
                Some(SplitDirection::Right),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.panes().len(),
                panes_before + 1,
                "opening in a split should add a pane"
            );
            let hosting_pane = workspace
                .panes()
                .iter()
                .find(|pane| pane.read(cx).items_of_type::<ThreadItem>().next().is_some())
                .cloned()
                .expect("one pane should hold the thread tab");
            assert_ne!(
                hosting_pane.entity_id(),
                first_pane.entity_id(),
                "the thread should land in the new pane"
            );
        });
    }

    #[gpui::test]
    async fn test_closing_a_tab_hands_the_thread_back_to_the_panel(cx: &mut TestAppContext) {
        let (workspace, window) = setup_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = setup_panel(&workspace, cx);

        let moved_view = active_view(&panel, cx);
        let (thread_id, agent) =
            moved_view.read_with(cx, |view, _cx| (view.thread_id, view.agent_key().clone()));

        workspace.update_in(cx, |workspace, window, cx| {
            ThreadItem::open_active_panel_thread(workspace, None, window, cx);
        });
        cx.run_until_parked();

        let item_id = thread_items(&workspace, cx)
            .pop()
            .expect("the thread should be in a tab")
            .entity_id();
        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace
                .panes()
                .iter()
                .find(|pane| pane.read(cx).items_of_type::<ThreadItem>().next().is_some())
                .cloned()
                .expect("the thread tab should be in a pane");
            pane.update(cx, |pane, cx| {
                pane.remove_item(item_id, false, true, window, cx);
            });
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _cx| {
            assert!(
                panel.test_retains_thread(thread_id),
                "the closed tab's thread should be parked in the panel, not dropped"
            );
        });

        // The panel keeps the handed-back view alive, so re-opening the thread
        // reuses it instead of resuming a fresh session.
        panel.update_in(cx, |panel, window, cx| {
            panel.load_agent_thread(
                agent,
                thread_id,
                None,
                None,
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel
                    .active_conversation_view()
                    .expect("the panel should show the reclaimed thread")
                    .entity_id(),
                moved_view.entity_id(),
                "closing the tab should hand the running thread back to the panel"
            );
        });
    }

    #[test]
    fn serialized_thread_item_round_trip() {
        let thread_id = ThreadId::new();
        let entry = SerializedThreadItem {
            thread_id,
            agent: Agent::NativeAgent,
        };

        let json = serde_json::to_string(&entry).expect("the entry should serialize");
        let restored: SerializedThreadItem =
            serde_json::from_str(&json).expect("the entry should survive");
        assert_eq!(restored.thread_id, thread_id);
        assert_eq!(restored.agent, Agent::NativeAgent);
    }

    /// 旧形式 (ワークスペースにつき 1 行へ全タブを詰めたもの) を 1 世代だけ
    /// 読めること。
    #[test]
    fn legacy_serialized_thread_items_round_trip() {
        let thread_id = ThreadId::new();
        let mut items = SerializedThreadItems::default();
        items.items.insert(
            "17".to_string(),
            SerializedThreadItem {
                thread_id,
                agent: Agent::NativeAgent,
            },
        );

        let json = serde_json::to_string(&items).expect("the row should serialize");
        let restored: SerializedThreadItems =
            serde_json::from_str(&json).expect("the row should survive");
        let restored = restored.items.get("17").expect("the entry should survive");
        assert_eq!(restored.thread_id, thread_id);
        assert_eq!(restored.agent, Agent::NativeAgent);
    }

    // -- ADR-F36 Phase B: パネル自身がタブ列を持つ ---------------------------

    fn panel_items(panel: &Entity<AgentPanel>, cx: &VisualTestContext) -> Vec<Entity<ThreadItem>> {
        panel.read_with(cx, |panel, cx| {
            panel
                .active_pane()
                .read(cx)
                .items_of_type::<ThreadItem>()
                .collect()
        })
    }

    /// パネルが出しているスレッドは、パネルの中のペインのタブになっている。
    #[gpui::test]
    async fn test_the_panel_shows_its_thread_as_a_tab(cx: &mut TestAppContext) {
        let (workspace, window) = setup_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = setup_panel(&workspace, cx);

        let view = active_view(&panel, cx);
        let items = panel_items(&panel, cx);
        assert_eq!(items.len(), 1, "the panel's thread should be a tab");
        items[0].read_with(cx, |item, _cx| {
            assert_eq!(item.conversation_view().entity_id(), view.entity_id());
            assert_eq!(
                item.host,
                ThreadItemHost::Panel,
                "a tab inside the panel must know it is not an editor tab"
            );
        });
        assert!(
            thread_items(&workspace, cx).is_empty(),
            "the panel's tab does not belong to the center"
        );
    }

    /// パネル → 中央 → パネル。同じアイテムが行って戻り、返却の判定
    /// (`on_removed` → `schedule_hand_back`) が誤爆しない。
    #[gpui::test]
    async fn test_moving_a_thread_from_the_panel_to_the_center_and_back(cx: &mut TestAppContext) {
        let (workspace, window) = setup_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = setup_panel(&workspace, cx);

        let moved_view = active_view(&panel, cx);
        let moved_thread_id = moved_view.read_with(cx, |view, _cx| view.thread_id);
        let panel_item_id = panel_items(&panel, cx)[0].entity_id();

        workspace.update_in(cx, |workspace, window, cx| {
            ThreadItem::open_active_panel_thread(workspace, None, window, cx);
        });
        cx.run_until_parked();

        let center_items = thread_items(&workspace, cx);
        assert_eq!(center_items.len(), 1);
        assert_eq!(
            center_items[0].entity_id(),
            panel_item_id,
            "the very same item should have moved, not a copy"
        );
        panel.read_with(cx, |panel, _cx| {
            assert!(
                !panel.test_retains_thread(moved_thread_id),
                "a thread that still has a tab must not be parked in the panel"
            );
        });

        center_items[0].update_in(cx, |item, window, cx| {
            item.move_to_panel(&MoveThreadToPanel, window, cx);
        });
        cx.run_until_parked();

        assert!(
            thread_items(&workspace, cx).is_empty(),
            "the center should have given the tab back"
        );
        let items = panel_items(&panel, cx);
        assert_eq!(
            items.len(),
            2,
            "the panel keeps the draft it made and takes the thread back beside it"
        );
        assert!(
            items.iter().any(|item| item.entity_id() == panel_item_id),
            "the same item should be back in the panel"
        );
        items
            .iter()
            .find(|item| item.entity_id() == panel_item_id)
            .expect("the moved item is back")
            .read_with(cx, |item, _cx| {
                assert!(!item.handed_back, "the item still owns its thread");
                assert_eq!(item.host, ThreadItemHost::Panel);
            });
        panel.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel
                    .active_conversation_view()
                    .expect("the panel should show the thread it took back")
                    .entity_id(),
                moved_view.entity_id()
            );
        });
    }

    /// パネルの中で分割すると、2 枚目のペインができてスレッドがそちらへ移る。
    #[gpui::test]
    async fn test_splitting_inside_the_panel_makes_a_second_pane(cx: &mut TestAppContext) {
        let (workspace, window) = setup_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = setup_panel(&workspace, cx);

        let first_pane = panel.read_with(cx, |panel, _cx| panel.active_pane().clone());
        let first_thread = active_view(&panel, cx);

        panel.update_in(cx, |panel, window, cx| {
            panel.split_active_thread_right(window, cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert_eq!(
                workspace::dock::Panel::panes(panel).len(),
                2,
                "the panel should now hold two panes"
            );
            let active_pane = panel.active_pane();
            assert_ne!(
                active_pane.entity_id(),
                first_pane.entity_id(),
                "the new pane should be the one the next action goes to"
            );
            assert_eq!(
                active_pane.read(cx).items_len(),
                1,
                "the new pane gets a thread of its own"
            );
            assert_eq!(
                first_pane.read(cx).items_len(),
                1,
                "the thread that was already there stays where it was"
            );
            assert_ne!(
                panel
                    .active_conversation_view()
                    .expect("the new pane shows a thread")
                    .entity_id(),
                first_thread.entity_id(),
                "splitting an only tab opens a second thread, it does not move the first"
            );
        });
    }

    /// パネルの最後のタブを閉じてもパネルは畳まず、新しい下書きに戻る。
    #[gpui::test]
    async fn test_closing_the_last_panel_tab_falls_back_to_a_draft(cx: &mut TestAppContext) {
        let (workspace, window) = setup_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = setup_panel(&workspace, cx);

        let first_item_id = panel_items(&panel, cx)[0].entity_id();
        panel.update_in(cx, |panel, window, cx| {
            let pane = panel.active_pane().clone();
            pane.update(cx, |pane, cx| {
                pane.remove_item(first_item_id, false, true, window, cx);
            });
        });
        cx.run_until_parked();

        let items = panel_items(&panel, cx);
        assert_eq!(items.len(), 1, "the panel opens a fresh draft tab");
        assert_ne!(items[0].entity_id(), first_item_id);
        panel.read_with(cx, |panel, cx| {
            assert!(
                panel.active_thread_is_draft(cx),
                "the panel should be left on an empty draft"
            );
        });
    }

    /// 下ドックに置けるようになった (ADR-F36 Phase B)。
    #[gpui::test]
    async fn test_the_panel_can_be_docked_at_the_bottom(cx: &mut TestAppContext) {
        let (workspace, window) = setup_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = setup_panel(&workspace, cx);

        panel.read_with(cx, |panel, _cx| {
            for position in [
                workspace::dock::DockPosition::Left,
                workspace::dock::DockPosition::Right,
                workspace::dock::DockPosition::Bottom,
            ] {
                assert!(
                    workspace::dock::Panel::position_is_valid(panel, position),
                    "{position:?} should be a valid dock for the agent panel"
                );
            }
        });
    }
}
