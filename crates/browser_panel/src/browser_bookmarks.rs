//! 内蔵ブラウザーのブックマーク (ADR-F19)。
//!
//! ブックマークは UI から追加・編集・削除・並べ替えをするユーザーデータであって
//! 宣言的な設定ではないので、settings.json ではなく専用の JSON ファイル
//! (`config_dir()/browser_bookmarks.json`) に置く。読み込み (コメント許容) と
//! 書き込み (atomic_write) と watch の作りは `terminal_sessions.json` と同じ。
//!
//! 木構造の位置は根からの添字列 (`BookmarkPath`) で指す。空の添字列は根フォルダー。

use std::{cmp::Ordering, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use serde::{Deserialize, Serialize};

/// 木の中の位置。根からの添字列で、空なら根フォルダーそのものを指す。
pub type BookmarkPath = Vec<usize>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BookmarkNode {
    Bookmark {
        name: String,
        url: String,
    },
    Folder {
        name: String,
        #[serde(default)]
        items: Vec<BookmarkNode>,
    },
}

impl BookmarkNode {
    pub fn bookmark(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self::Bookmark {
            name: name.into(),
            url: url.into(),
        }
    }

    pub fn folder(name: impl Into<String>) -> Self {
        Self::Folder {
            name: name.into(),
            items: Vec::new(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Bookmark { name, .. } | Self::Folder { name, .. } => name,
        }
    }

    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Bookmark { url, .. } => Some(url),
            Self::Folder { .. } => None,
        }
    }

    pub fn is_folder(&self) -> bool {
        matches!(self, Self::Folder { .. })
    }

    pub fn children(&self) -> &[BookmarkNode] {
        match self {
            Self::Folder { items, .. } => items,
            Self::Bookmark { .. } => &[],
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarksFile {
    #[serde(default)]
    pub items: Vec<BookmarkNode>,
}

/// 木を一列に潰した 1 行。折りたたまれたフォルダーの中は含まれない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookmarkRow {
    pub path: BookmarkPath,
    pub depth: usize,
    pub is_folder: bool,
    pub expanded: bool,
    pub name: String,
    pub url: Option<String>,
    /// 兄弟の中での位置。上へ / 下へボタンの有効・無効に使う。
    pub index: usize,
    pub sibling_count: usize,
}

impl BookmarksFile {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn node(&self, path: &[usize]) -> Option<&BookmarkNode> {
        let (&index, parent) = path.split_last()?;
        self.items_in(parent)?.get(index)
    }

    /// `folder` が指すフォルダーの中身。空の添字列なら根。
    /// フォルダー以外や存在しない位置を指していたら `None`。
    fn items_in(&self, folder: &[usize]) -> Option<&Vec<BookmarkNode>> {
        let mut items = &self.items;
        for &index in folder {
            match items.get(index)? {
                BookmarkNode::Folder {
                    items: children, ..
                } => items = children,
                BookmarkNode::Bookmark { .. } => return None,
            }
        }
        Some(items)
    }

    fn items_in_mut(&mut self, folder: &[usize]) -> Option<&mut Vec<BookmarkNode>> {
        let mut items = &mut self.items;
        for &index in folder {
            let node = items.get_mut(index)?;
            let BookmarkNode::Folder {
                items: children, ..
            } = node
            else {
                return None;
            };
            items = children;
        }
        Some(items)
    }

    pub fn children(&self, folder: &[usize]) -> &[BookmarkNode] {
        self.items_in(folder).map_or(&[], Vec::as_slice)
    }

    /// `folder` の末尾に足す。戻り値は追加された位置。
    pub fn insert(&mut self, folder: &[usize], node: BookmarkNode) -> Option<BookmarkPath> {
        let items = self.items_in_mut(folder)?;
        items.push(node);
        let mut path = folder.to_vec();
        path.push(items.len() - 1);
        Some(path)
    }

    pub fn remove(&mut self, path: &[usize]) -> Option<BookmarkNode> {
        let (&index, parent) = path.split_last()?;
        let items = self.items_in_mut(parent)?;
        (index < items.len()).then(|| items.remove(index))
    }

    /// 名前を差し替える。ブックマークなら URL も。
    /// `url` はフォルダーに対しては無視される。
    pub fn rename(&mut self, path: &[usize], name: impl Into<String>, url: Option<&str>) -> bool {
        let Some((&index, parent)) = path.split_last() else {
            return false;
        };
        let Some(items) = self.items_in_mut(parent) else {
            return false;
        };
        let Some(node) = items.get_mut(index) else {
            return false;
        };
        match node {
            BookmarkNode::Bookmark {
                name: node_name,
                url: node_url,
            } => {
                *node_name = name.into();
                if let Some(url) = url {
                    *node_url = url.to_string();
                }
            }
            BookmarkNode::Folder {
                name: node_name, ..
            } => *node_name = name.into(),
        }
        true
    }

    /// 兄弟の中で `delta` だけずらす。端を越える移動は行わない。
    pub fn move_within_parent(&mut self, path: &[usize], delta: i32) -> Option<BookmarkPath> {
        let (&index, parent) = path.split_last()?;
        let items = self.items_in_mut(parent)?;
        let target = index.checked_add_signed(delta as isize)?;
        if target >= items.len() || index >= items.len() {
            return None;
        }
        items.swap(index, target);
        let mut moved = parent.to_vec();
        moved.push(target);
        Some(moved)
    }

    /// 兄弟の中で `to_index` の**手前**へ動かす (ドラッグ＆ドロップの挿入位置)。
    /// `to_index` は取り除く前の添字で数えるので、自分より後ろを指したときは
    /// 取り除いた分だけ 1 つ手前に補正する。動かない位置を指しても失敗にはしない。
    pub fn reorder_within_parent(
        &mut self,
        path: &[usize],
        to_index: usize,
    ) -> Option<BookmarkPath> {
        let (&index, parent) = path.split_last()?;
        let items = self.items_in_mut(parent)?;
        if index >= items.len() {
            return None;
        }
        let target = to_index.min(items.len());
        let target = if target > index { target - 1 } else { target };
        if target != index {
            let node = items.remove(index);
            items.insert(target, node);
        }
        let mut moved = parent.to_vec();
        moved.push(target);
        Some(moved)
    }

    /// 別のフォルダー (空の添字列 = 根) の末尾へ移す。
    /// 自分自身や自分の子孫へは移せない。
    pub fn move_to_folder(&mut self, path: &[usize], folder: &[usize]) -> Option<BookmarkPath> {
        if path.is_empty() || folder.starts_with(path) {
            return None;
        }
        let (_, parent) = path.split_last()?;
        if parent == folder {
            return Some(path.to_vec());
        }
        // 先に足してから消す。末尾追加は既存の添字を動かさないので、
        // 途中で失敗しても木からノードが消えることがない。
        let node = self.node(path)?.clone();
        let inserted = self.insert(folder, node)?;
        if self.remove(path).is_none() {
            return Some(inserted);
        }
        path_after_removal(&inserted, path)
    }

    /// フォルダーだけを (位置, 深さ, 名前) で列挙する。移動先ピッカー用。
    pub fn folders(&self) -> Vec<(BookmarkPath, usize, String)> {
        fn walk(
            items: &[BookmarkNode],
            prefix: &[usize],
            depth: usize,
            out: &mut Vec<(BookmarkPath, usize, String)>,
        ) {
            for (index, node) in items.iter().enumerate() {
                if let BookmarkNode::Folder { name, items } = node {
                    let mut path = prefix.to_vec();
                    path.push(index);
                    out.push((path.clone(), depth, name.clone()));
                    walk(items, &path, depth + 1, out);
                }
            }
        }

        let mut folders = Vec::new();
        walk(&self.items, &[], 0, &mut folders);
        folders
    }

    /// 表示用に木を一列へ潰す。`expanded` が false を返したフォルダーの中は畳む。
    pub fn visible_rows(&self, expanded: &dyn Fn(&[usize]) -> bool) -> Vec<BookmarkRow> {
        fn walk(
            items: &[BookmarkNode],
            prefix: &[usize],
            depth: usize,
            expanded: &dyn Fn(&[usize]) -> bool,
            out: &mut Vec<BookmarkRow>,
        ) {
            for (index, node) in items.iter().enumerate() {
                let mut path = prefix.to_vec();
                path.push(index);
                let is_expanded = node.is_folder() && expanded(&path);
                out.push(BookmarkRow {
                    path: path.clone(),
                    depth,
                    is_folder: node.is_folder(),
                    expanded: is_expanded,
                    name: node.name().to_string(),
                    url: node.url().map(str::to_string),
                    index,
                    sibling_count: items.len(),
                });
                if is_expanded {
                    walk(node.children(), &path, depth + 1, expanded, out);
                }
            }
        }

        let mut rows = Vec::new();
        walk(&self.items, &[], 0, expanded, &mut rows);
        rows
    }
}

/// `removed` を取り除いた後の `target` の位置。取り除いた枝の中を指していたら `None`。
fn path_after_removal(target: &[usize], removed: &[usize]) -> Option<BookmarkPath> {
    let (&removed_index, removed_parent) = removed.split_last()?;
    let depth = removed_parent.len();
    if target.len() <= depth || !target.starts_with(removed_parent) {
        return Some(target.to_vec());
    }
    let mut target = target.to_vec();
    match target[depth].cmp(&removed_index) {
        Ordering::Less => Some(target),
        Ordering::Equal => None,
        Ordering::Greater => {
            target[depth] -= 1;
            Some(target)
        }
    }
}

/// ブックマークファイルがまだ無いときに書き込む初期値。
pub fn default_bookmarks() -> BookmarksFile {
    BookmarksFile {
        items: vec![
            BookmarkNode::bookmark("Microsoft 365 Copilot", "https://m365.cloud.microsoft/chat"),
            BookmarkNode::bookmark("Claude", "https://claude.ai/new"),
            BookmarkNode::bookmark("ChatGPT", "https://chatgpt.com/"),
        ],
    }
}

/// 既定のブックマークを書き込んでよいか。**パースした結果ではなくファイルの生の
/// テキストで判断する**: 利用者がブックマークを全部消した後の `{"items": []}` を
/// 「まだ無い」と扱うと、消したはずの既定が起動のたびに復活してしまう。
pub fn should_seed_default_bookmarks(contents: &str) -> bool {
    contents.trim().is_empty()
}

/// ブックマークファイルを読む。壊れていても失敗させない (空として扱う):
/// 手書きの取りこぼしでブックマーク UI ごと消えるのは割に合わない。
pub fn parse_bookmarks_file(contents: &str) -> BookmarksFile {
    if contents.trim().is_empty() {
        return BookmarksFile::default();
    }
    match settings_json::parse_json_with_comments::<BookmarksFile>(contents) {
        Ok(file) => file,
        Err(error) => {
            log::warn!(
                "browser_bookmarks.json could not be parsed ({error:#}); \
                 continuing with no bookmarks"
            );
            BookmarksFile::default()
        }
    }
}

pub fn serialize_bookmarks_file(file: &BookmarksFile) -> Result<String> {
    let mut text = serde_json::to_string_pretty(file)?;
    text.push('\n');
    Ok(text)
}

/// `browser_bookmarks.json` のメモリ上の写し。ウィンドウ間で 1 つを共有する。
pub struct BookmarkStore {
    file: BookmarksFile,
    /// 最初の読み込みが届いたかどうか。届くまでの `file` は「ユーザーが空にした」
    /// のではなく「まだ読めていない」なので、書き戻すと実ファイルを潰してしまう。
    loaded: bool,
    fs: Arc<dyn Fs>,
    path: PathBuf,
    _watch: Task<()>,
}

struct BookmarkStoreGlobal(Entity<BookmarkStore>);

impl Global for BookmarkStoreGlobal {}

impl BookmarkStore {
    /// 共有のストア。最初に呼ばれたときに作って watch を始める
    /// (`browser_panel::init` の時点では `Fs` が無いので遅延生成)。
    pub fn global(fs: Arc<dyn Fs>, cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<BookmarkStoreGlobal>() {
            return global.0.clone();
        }
        let path = paths::browser_bookmarks_file().clone();
        let store = cx.new(|cx| Self::new(fs, path, cx));
        cx.set_global(BookmarkStoreGlobal(store.clone()));
        store
    }

    pub fn new(fs: Arc<dyn Fs>, path: PathBuf, cx: &mut Context<Self>) -> Self {
        let (mut contents, watcher) =
            settings::watch_config_file(cx.background_executor(), fs.clone(), path.clone());
        let watch = cx.spawn({
            let fs = fs.clone();
            let path = path.clone();
            async move |this, cx| {
                // `watch_config_file` の watch はこのタスクが生きている間だけ有効。
                let _watcher = watcher;
                // 既定を書き込むのは最初の 1 回だけ。書いた直後に watch がその内容を
                // 返してくるので、判定を毎回やると書き込みの往復で無限に回りうる。
                let mut may_seed_defaults = true;
                while let Some(contents) = contents.next().await {
                    let file = parse_bookmarks_file(&contents);
                    // `watch_config_file` は読み込みに失敗しても空文字列を流すので、
                    // 「空 = まだ無い」と決めつけずにファイルの有無まで確かめる。
                    // 読めなかっただけのファイルを既定で潰すのは取り返しがつかない。
                    let seed = std::mem::take(&mut may_seed_defaults)
                        && should_seed_default_bookmarks(&contents)
                        && !fs.is_file(&path).await;
                    if this
                        .update(cx, |this, cx| {
                            if !this.loaded || this.file != file {
                                this.loaded = true;
                                this.file = file;
                                cx.notify();
                            }
                            if seed {
                                this.write_default_bookmarks(cx);
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });

        Self {
            file: BookmarksFile::default(),
            loaded: false,
            fs,
            path,
            _watch: watch,
        }
    }

    pub fn file(&self) -> &BookmarksFile {
        &self.file
    }

    /// 一度でもファイルを読めたかどうか。書き戻す UI はこれが true になるまで
    /// 書いてはいけない。
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// 既定のブックマークを書き込む。ファイルがまだ無いと分かったときだけ呼ぶ。
    fn write_default_bookmarks(&mut self, cx: &mut Context<Self>) {
        let write = self.write(default_bookmarks(), cx);
        cx.spawn(async move |_, _| {
            if let Err(error) = write.await {
                log::error!("failed to write the default browser bookmarks: {error:#}");
            }
        })
        .detach();
    }

    /// 木全体を差し替えて書き戻す。watch の往復を待たずに済むよう、
    /// メモリ上の写しはすぐ更新する。
    pub fn write(&mut self, file: BookmarksFile, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.file = file;
        self.loaded = true;
        cx.notify();
        let text = match serialize_bookmarks_file(&self.file) {
            Ok(text) => text,
            Err(error) => return Task::ready(Err(error)),
        };
        let fs = self.fs.clone();
        let path = self.path.clone();
        cx.background_spawn(async move {
            if let Some(parent) = path.parent() {
                fs.create_dir(parent)
                    .await
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let target = fs
                .canonicalize(&path)
                .await
                .unwrap_or_else(|_| path.clone());
            fs.atomic_write(target, text)
                .await
                .context("couldn't write browser_bookmarks.json")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BookmarksFile {
        BookmarksFile {
            items: vec![
                BookmarkNode::bookmark("Google", "https://www.google.com/"),
                BookmarkNode::Folder {
                    name: "仕事".into(),
                    items: vec![
                        BookmarkNode::bookmark("Copilot", "https://github.com/copilot"),
                        BookmarkNode::Folder {
                            name: "社内".into(),
                            items: vec![BookmarkNode::bookmark(
                                "Wiki",
                                "https://wiki.example.com/",
                            )],
                        },
                    ],
                },
            ],
        }
    }

    #[test]
    fn nodes_are_addressed_by_index_path() {
        let file = sample();
        assert_eq!(file.node(&[0]).map(BookmarkNode::name), Some("Google"));
        assert_eq!(file.node(&[1, 1, 0]).map(BookmarkNode::name), Some("Wiki"));
        assert_eq!(
            file.node(&[1, 1, 0]).and_then(BookmarkNode::url),
            Some("https://wiki.example.com/")
        );
        assert_eq!(file.node(&[2]), None);
        // ブックマークの「中」は指せない。
        assert_eq!(file.node(&[0, 0]), None);
        assert_eq!(file.children(&[1]).len(), 2);
        assert!(file.children(&[0]).is_empty());
    }

    #[test]
    fn bookmarks_and_folders_can_be_added() {
        let mut file = BookmarksFile::default();
        assert_eq!(
            file.insert(&[], BookmarkNode::folder("仕事")).as_deref(),
            Some([0].as_slice())
        );
        assert_eq!(
            file.insert(
                &[0],
                BookmarkNode::bookmark("Copilot", "https://github.com/copilot")
            )
            .as_deref(),
            Some([0, 0].as_slice())
        );
        // ブックマークの中には入れられない。
        assert_eq!(file.insert(&[0, 0], BookmarkNode::folder("だめ")), None);
        assert_eq!(file.insert(&[9], BookmarkNode::folder("だめ")), None);
        assert_eq!(file.children(&[0]).len(), 1);
    }

    #[test]
    fn renaming_keeps_the_url_when_it_is_not_given() {
        let mut file = sample();
        assert!(file.rename(&[0], "検索", None));
        assert_eq!(file.node(&[0]).map(BookmarkNode::name), Some("検索"));
        assert_eq!(
            file.node(&[0]).and_then(BookmarkNode::url),
            Some("https://www.google.com/")
        );
        assert!(file.rename(&[0], "検索", Some("https://duckduckgo.com/")));
        assert_eq!(
            file.node(&[0]).and_then(BookmarkNode::url),
            Some("https://duckduckgo.com/")
        );
        // フォルダーは名前だけ。
        assert!(file.rename(&[1], "しごと", Some("https://example.com/")));
        assert_eq!(file.node(&[1]).map(BookmarkNode::name), Some("しごと"));
        assert!(file.node(&[1]).and_then(BookmarkNode::url).is_none());
        assert!(!file.rename(&[], "根", None));
        assert!(!file.rename(&[9], "無い", None));
    }

    #[test]
    fn removing_a_folder_takes_its_contents() {
        let mut file = sample();
        let removed = file.remove(&[1]).expect("the folder can be removed");
        assert_eq!(removed.children().len(), 2);
        assert_eq!(file.items.len(), 1);
        assert_eq!(file.remove(&[5]), None);
        assert_eq!(file.remove(&[]), None);
    }

    #[test]
    fn siblings_can_be_reordered() {
        let mut file = sample();
        assert_eq!(
            file.move_within_parent(&[1], -1).as_deref(),
            Some([0].as_slice())
        );
        assert_eq!(file.node(&[0]).map(BookmarkNode::name), Some("仕事"));
        assert_eq!(file.node(&[1]).map(BookmarkNode::name), Some("Google"));
        // 入れ子でも兄弟の中だけで動く。
        assert_eq!(
            file.move_within_parent(&[0, 0], 1).as_deref(),
            Some([0, 1].as_slice())
        );
        assert_eq!(file.node(&[0, 0]).map(BookmarkNode::name), Some("社内"));
        // 端は越えない。ブックマークの中にも入らない。
        assert_eq!(file.move_within_parent(&[0], -1), None);
        assert_eq!(file.move_within_parent(&[1], 1), None);
        assert_eq!(file.move_within_parent(&[1, 0], 1), None);
    }

    #[test]
    fn siblings_can_be_dropped_at_an_insertion_point() {
        let mut file = sample();
        // 末尾 (兄弟の数と同じ添字) に落とすと最後尾へ。
        assert_eq!(
            file.reorder_within_parent(&[0], 2).as_deref(),
            Some([1].as_slice())
        );
        assert_eq!(file.node(&[1]).map(BookmarkNode::name), Some("Google"));
        // 先頭へ戻す。
        assert_eq!(
            file.reorder_within_parent(&[1], 0).as_deref(),
            Some([0].as_slice())
        );
        assert_eq!(file, sample());
        // 自分自身の直前 / 直後は「動かない位置」。失敗にはしない。
        assert_eq!(
            file.reorder_within_parent(&[0], 0).as_deref(),
            Some([0].as_slice())
        );
        assert_eq!(
            file.reorder_within_parent(&[0], 1).as_deref(),
            Some([0].as_slice())
        );
        assert_eq!(file, sample());
    }

    #[test]
    fn dropping_outside_the_bar_does_not_move_anything() {
        let mut file = sample();
        // 兄弟の数を超える挿入位置は末尾に丸める。
        assert_eq!(
            file.reorder_within_parent(&[0], 99).as_deref(),
            Some([1].as_slice())
        );
        // 根そのもの・存在しない位置・ブックマークの中は動かせない。
        assert_eq!(file.reorder_within_parent(&[], 0), None);
        assert_eq!(file.reorder_within_parent(&[9], 0), None);
        assert_eq!(file.reorder_within_parent(&[1, 0], 0), None);
        // 入れ子の中でも兄弟の並びだけが変わる。
        assert_eq!(
            file.reorder_within_parent(&[0, 1], 0).as_deref(),
            Some([0, 0].as_slice())
        );
        assert_eq!(file.node(&[0, 0]).map(BookmarkNode::name), Some("社内"));
    }

    #[test]
    fn nodes_can_be_moved_between_folders() {
        let mut file = sample();
        // 根の Google を「仕事」へ。
        assert_eq!(
            file.move_to_folder(&[0], &[1]).as_deref(),
            Some([0, 2].as_slice())
        );
        assert_eq!(file.items.len(), 1);
        assert_eq!(file.node(&[0, 2]).map(BookmarkNode::name), Some("Google"));

        // 「社内」を根へ戻す。移動元が移動先より前にあっても位置が壊れない。
        assert_eq!(
            file.move_to_folder(&[0, 1], &[]).as_deref(),
            Some([1].as_slice())
        );
        assert_eq!(file.node(&[1]).map(BookmarkNode::name), Some("社内"));
        assert_eq!(file.children(&[0]).len(), 2);
    }

    #[test]
    fn a_folder_cannot_be_moved_into_itself() {
        let mut file = sample();
        // 自分自身へも、自分の子孫へも入れられない。根も動かせない。
        assert_eq!(file.move_to_folder(&[1], &[1]), None);
        assert_eq!(file.move_to_folder(&[1], &[1, 1]), None);
        assert_eq!(file.move_to_folder(&[], &[1]), None);
        assert_eq!(file, sample());
        // 既に同じフォルダーにいるなら、何もせず今の位置を返す。
        assert_eq!(
            file.move_to_folder(&[1, 0], &[1]).as_deref(),
            Some([1, 0].as_slice())
        );
        assert_eq!(file, sample());
    }

    #[test]
    fn folders_are_listed_with_their_depth() {
        assert_eq!(
            sample().folders(),
            vec![
                (vec![1], 0, "仕事".to_string()),
                (vec![1, 1], 1, "社内".to_string()),
            ]
        );
    }

    #[test]
    fn collapsed_folders_hide_their_contents() {
        let file = sample();
        let rows = file.visible_rows(&|_| false);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].name, "仕事");
        assert!(rows[1].is_folder);
        assert!(!rows[1].expanded);
        assert_eq!(rows[0].sibling_count, 2);

        let rows = file.visible_rows(&|path| path == [1]);
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["Google", "仕事", "Copilot", "社内"]
        );
        assert_eq!(rows[2].depth, 1);
        assert_eq!(rows[3].path, vec![1, 1]);
    }

    #[test]
    fn the_file_round_trips_through_json() {
        let file = sample();
        let text = serialize_bookmarks_file(&file).expect("the file serializes");
        assert_eq!(parse_bookmarks_file(&text), file);
        assert!(text.contains("\"type\": \"bookmark\""));
        assert!(text.contains("\"type\": \"folder\""));
    }

    #[test]
    fn a_broken_file_is_read_as_empty() {
        assert_eq!(parse_bookmarks_file(""), BookmarksFile::default());
        assert_eq!(parse_bookmarks_file("   \n"), BookmarksFile::default());
        assert_eq!(parse_bookmarks_file("{{{"), BookmarksFile::default());
        // コメントは許容する。
        assert_eq!(
            parse_bookmarks_file(
                r#"{
                    // ブックマーク
                    "items": [{ "type": "bookmark", "name": "Google", "url": "https://www.google.com/" }],
                }"#
            ),
            BookmarksFile {
                items: vec![BookmarkNode::bookmark("Google", "https://www.google.com/")],
            }
        );
    }

    #[test]
    fn the_defaults_are_written_only_when_the_file_is_absent_or_blank() {
        // `watch_config_file` はファイルが無いと空文字列を流す。
        assert!(should_seed_default_bookmarks(""));
        assert!(should_seed_default_bookmarks("  \n\t"));
        // 利用者が全部消した後のファイルは「既に存在する」ので触らない。
        assert!(!should_seed_default_bookmarks(r#"{ "items": [] }"#));
        assert!(!should_seed_default_bookmarks(
            &serialize_bookmarks_file(&sample()).expect("the file serializes")
        ));
    }

    #[test]
    fn the_default_bookmarks_are_three_chat_services() {
        let file = default_bookmarks();
        assert_eq!(
            file.items
                .iter()
                .map(|node| (node.name(), node.url().unwrap_or_default()))
                .collect::<Vec<_>>(),
            vec![
                ("Microsoft 365 Copilot", "https://m365.cloud.microsoft/chat"),
                ("Claude", "https://claude.ai/new"),
                ("ChatGPT", "https://chatgpt.com/"),
            ]
        );
        // 書き出した既定をもう一度読んでも同じ木になること (フォルダー無しの平坦な木)。
        let text = serialize_bookmarks_file(&file).expect("the file serializes");
        assert_eq!(parse_bookmarks_file(&text), file);
        assert!(file.folders().is_empty());
    }

    #[test]
    fn folders_without_items_are_accepted() {
        assert_eq!(
            parse_bookmarks_file(r#"{ "items": [{ "type": "folder", "name": "空" }] }"#),
            BookmarksFile {
                items: vec![BookmarkNode::folder("空")],
            }
        );
    }
}
