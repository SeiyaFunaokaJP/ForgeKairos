use std::time::Instant;

use collections::HashMap;
use gpui::SharedString;

use crate::call_source::{CallChildren, CallSymbol};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(usize);

impl NodeId {
    pub fn index(self) -> usize {
        self.0
    }
}

#[derive(Debug)]
pub enum ChildrenState {
    NotLoaded,
    Loading,
    Loaded {
        shown: Vec<NodeId>,
        /// Children beyond the display limit, turned into nodes by "Show more".
        hidden: Vec<CallSymbol>,
        /// Callees the settings exclude, turned into nodes on request.
        excluded: Vec<CallSymbol>,
        /// The symbol index had more calls than it answered with.
        truncated: bool,
    },
    Failed(SharedString),
}

#[derive(Debug)]
pub struct CallNode {
    pub symbol: CallSymbol,
    pub parent: Option<NodeId>,
    pub depth: usize,
    pub expanded: bool,
    /// An ancestor is the same function, so expanding would only repeat the path.
    pub recursive: bool,
    pub children: ChildrenState,
    /// Counts the fetches of the node's children, so that the answer of one
    /// that was replaced or abandoned is not applied.
    pub fetch_generation: u64,
    /// The children shown are being asked for again, or one source has not
    /// answered yet.
    pub refreshing: bool,
    /// The language server answered with no children, which it may do while
    /// it indexes.
    pub language_server_empty: bool,
    /// The symbol index was still being built when it answered.
    pub index_partial: bool,
    /// How many times the children were asked for again because a source
    /// reported finished work.
    pub empty_retries: u8,
    /// A source finished some work since the children were last asked for again.
    pub retry_owed: bool,
    /// When the children may be asked for again, so that a source that keeps
    /// reporting finished work does not get the same question each time.
    pub retry_not_before: Option<Instant>,
}

impl CallNode {
    pub fn can_expand(&self) -> bool {
        !self.recursive && self.symbol.can_expand()
    }

    pub fn is_loading(&self) -> bool {
        self.refreshing || matches!(self.children, ChildrenState::Loading)
    }
}

/// One line of the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Node {
        id: NodeId,
        depth: usize,
    },
    /// The node's children came back empty.
    Empty {
        parent: NodeId,
        depth: usize,
    },
    /// Asking for the node's children failed.
    Error {
        parent: NodeId,
        depth: usize,
    },
    /// The node has more children than are shown.
    ShowMore {
        parent: NodeId,
        depth: usize,
        remaining: usize,
    },
    /// Callees the settings exclude.
    Excluded {
        parent: NodeId,
        depth: usize,
        count: usize,
    },
    /// The symbol index answered with only the first calls.
    Truncated {
        parent: NodeId,
        depth: usize,
    },
}

impl Row {
    pub fn depth(&self) -> usize {
        match self {
            Row::Node { depth, .. }
            | Row::Empty { depth, .. }
            | Row::Error { depth, .. }
            | Row::ShowMore { depth, .. }
            | Row::Excluded { depth, .. }
            | Row::Truncated { depth, .. } => *depth,
        }
    }

    /// The node the row is, or belongs to.
    pub fn node(&self) -> NodeId {
        match self {
            Row::Node { id, .. } => *id,
            Row::Empty { parent, .. }
            | Row::Error { parent, .. }
            | Row::ShowMore { parent, .. }
            | Row::Excluded { parent, .. }
            | Row::Truncated { parent, .. } => *parent,
        }
    }
}

/// The expanded part of a call graph. Collapsing a node drops its subtree, so
/// the tree only ever holds what is on screen.
#[derive(Debug, Default)]
pub struct CallTree {
    nodes: HashMap<NodeId, CallNode>,
    root: Option<NodeId>,
    next_id: usize,
}

impl CallTree {
    pub fn root(&self) -> Option<NodeId> {
        self.root
    }

    pub fn node(&self, id: NodeId) -> Option<&CallNode> {
        self.nodes.get(&id)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut CallNode> {
        self.nodes.get_mut(&id)
    }

    pub fn node_ids(&self) -> Vec<NodeId> {
        self.nodes.keys().copied().collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.root = None;
    }

    pub fn set_root(&mut self, symbol: CallSymbol) -> NodeId {
        self.clear();
        let id = self.insert(symbol, None, 0, false);
        self.root = Some(id);
        id
    }

    fn insert(
        &mut self,
        symbol: CallSymbol,
        parent: Option<NodeId>,
        depth: usize,
        recursive: bool,
    ) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        self.nodes.insert(
            id,
            CallNode {
                symbol,
                parent,
                depth,
                expanded: false,
                recursive,
                children: ChildrenState::NotLoaded,
                fetch_generation: 0,
                refreshing: false,
                language_server_empty: false,
                index_partial: false,
                empty_retries: 0,
                retry_owed: false,
                retry_not_before: None,
            },
        );
        id
    }

    /// Whether `symbol` is `node` or one of its ancestors.
    pub fn is_on_path(&self, node: NodeId, symbol: &CallSymbol) -> bool {
        let mut current = Some(node);
        while let Some(id) = current {
            let Some(ancestor) = self.nodes.get(&id) else {
                return false;
            };
            if ancestor.symbol.same_symbol(symbol) {
                return true;
            }
            current = ancestor.parent;
        }
        false
    }

    /// Stores an answer for `parent`'s children, keeping the first `limit` (or
    /// as many as were shown already) as nodes and the rest for "Show more".
    /// A child that was shown before stays the same node, with its subtree,
    /// so that a later or fuller answer does not fold what the user opened.
    pub fn set_children(
        &mut self,
        parent: NodeId,
        children: CallChildren,
        limit: usize,
        finished: bool,
    ) {
        let Some(parent_node) = self.nodes.get_mut(&parent) else {
            return;
        };
        let parent_depth = parent_node.depth;
        let previous = match std::mem::replace(&mut parent_node.children, ChildrenState::NotLoaded)
        {
            ChildrenState::Loaded { shown, .. } => shown,
            _ => Vec::new(),
        };
        parent_node.refreshing = !finished;
        parent_node.language_server_empty = children.language_server_empty;
        parent_node.index_partial = children.index_partial;

        let mut symbols = children.symbols;
        let shown_count = limit.max(previous.len()).min(symbols.len());
        let hidden = symbols.split_off(shown_count);
        let mut reusable = previous;
        let mut shown = Vec::with_capacity(symbols.len());
        for symbol in symbols {
            let reused = reusable.iter().position(|id| {
                self.nodes
                    .get(id)
                    .is_some_and(|node| node.symbol.same_row(&symbol))
            });
            let recursive = self.is_on_path(parent, &symbol);
            match reused.map(|position| reusable.remove(position)) {
                Some(id) => {
                    if let Some(node) = self.nodes.get_mut(&id) {
                        node.symbol = symbol;
                        node.recursive = recursive;
                    }
                    shown.push(id);
                }
                None => shown.push(self.insert(symbol, Some(parent), parent_depth + 1, recursive)),
            }
        }
        for id in reusable {
            self.remove_subtree(id);
        }
        if let Some(node) = self.nodes.get_mut(&parent) {
            node.children = ChildrenState::Loaded {
                shown,
                hidden,
                excluded: children.excluded,
                truncated: children.truncated,
            };
        }
    }

    pub fn show_more(&mut self, parent: NodeId, limit: usize) {
        let Some(node) = self.nodes.get_mut(&parent) else {
            return;
        };
        let ChildrenState::Loaded { hidden, .. } = &mut node.children else {
            return;
        };
        let rest = hidden.split_off(limit.min(hidden.len()));
        let next = std::mem::replace(hidden, rest);
        self.append_children(parent, next);
    }

    /// Lists the callees the settings excluded, until the node is asked again.
    pub fn show_excluded(&mut self, parent: NodeId) {
        let Some(node) = self.nodes.get_mut(&parent) else {
            return;
        };
        let ChildrenState::Loaded { excluded, .. } = &mut node.children else {
            return;
        };
        let excluded = std::mem::take(excluded);
        self.append_children(parent, excluded);
    }

    fn append_children(&mut self, parent: NodeId, symbols: Vec<CallSymbol>) {
        let Some(parent_depth) = self.nodes.get(&parent).map(|node| node.depth) else {
            return;
        };
        let new_ids = symbols
            .into_iter()
            .map(|symbol| {
                let recursive = self.is_on_path(parent, &symbol);
                self.insert(symbol, Some(parent), parent_depth + 1, recursive)
            })
            .collect::<Vec<_>>();
        if let Some(ChildrenState::Loaded { shown, .. }) =
            self.nodes.get_mut(&parent).map(|node| &mut node.children)
        {
            shown.extend(new_ids);
        }
    }

    /// Starts a fetch of the node's children and returns its generation. The
    /// children shown are kept while it runs when `keep_children` is set.
    pub fn begin_fetch(&mut self, id: NodeId, keep_children: bool) -> Option<u64> {
        let keep_children = keep_children
            && self
                .nodes
                .get(&id)
                .is_some_and(|node| matches!(node.children, ChildrenState::Loaded { .. }));
        if !keep_children {
            self.remove_children(id);
        }
        let node = self.nodes.get_mut(&id)?;
        node.expanded = true;
        node.fetch_generation += 1;
        if keep_children {
            node.refreshing = true;
        } else {
            node.refreshing = false;
            node.children = ChildrenState::Loading;
        }
        Some(node.fetch_generation)
    }

    /// Whether an answer of the fetch `generation` still belongs to the node.
    pub fn is_current_fetch(&self, id: NodeId, generation: u64) -> bool {
        self.nodes
            .get(&id)
            .is_some_and(|node| node.expanded && node.fetch_generation == generation)
    }

    pub fn set_failed(&mut self, id: NodeId, message: SharedString) {
        self.remove_children(id);
        if let Some(node) = self.nodes.get_mut(&id) {
            node.refreshing = false;
            node.children = ChildrenState::Failed(message);
        }
    }

    /// Folds the node and forgets its children; expanding again asks anew.
    pub fn collapse(&mut self, id: NodeId) {
        self.remove_children(id);
        if let Some(node) = self.nodes.get_mut(&id) {
            node.expanded = false;
            node.fetch_generation += 1;
            node.refreshing = false;
            node.children = ChildrenState::NotLoaded;
            node.language_server_empty = false;
            node.index_partial = false;
            node.empty_retries = 0;
            node.retry_owed = false;
            node.retry_not_before = None;
        }
    }

    fn remove_children(&mut self, id: NodeId) {
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        let children = match std::mem::replace(&mut node.children, ChildrenState::NotLoaded) {
            ChildrenState::Loaded { shown, .. } => shown,
            _ => Vec::new(),
        };
        for child in children {
            self.remove_subtree(child);
        }
    }

    fn remove_subtree(&mut self, id: NodeId) {
        let mut stack = vec![id];
        while let Some(child) = stack.pop() {
            if let Some(removed) = self.nodes.remove(&child)
                && let ChildrenState::Loaded { shown, .. } = removed.children
            {
                stack.extend(shown);
            }
        }
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        match self.nodes.get(&id).map(|node| &node.children) {
            Some(ChildrenState::Loaded { shown, .. }) => shown,
            _ => &[],
        }
    }

    /// The rows of the tree, depth first. With a filter, only nodes whose name
    /// contains it (ignoring case) and their ancestors are kept.
    pub fn rows(&self, filter: &str) -> Vec<Row> {
        let mut rows = Vec::new();
        let Some(root) = self.root else {
            return rows;
        };
        let filter = filter.trim().to_lowercase();
        if filter.is_empty() {
            self.push_rows(root, &mut rows);
        } else {
            self.push_filtered_rows(root, &filter, &mut rows);
        }
        rows
    }

    fn push_rows(&self, id: NodeId, rows: &mut Vec<Row>) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        rows.push(Row::Node {
            id,
            depth: node.depth,
        });
        if !node.expanded {
            return;
        }
        let depth = node.depth + 1;
        match &node.children {
            ChildrenState::NotLoaded | ChildrenState::Loading => {}
            ChildrenState::Failed(_) => rows.push(Row::Error { parent: id, depth }),
            ChildrenState::Loaded {
                shown,
                hidden,
                excluded,
                truncated,
            } => {
                if shown.is_empty() && hidden.is_empty() && excluded.is_empty() {
                    rows.push(Row::Empty { parent: id, depth });
                }
                for child in shown {
                    self.push_rows(*child, rows);
                }
                if !hidden.is_empty() {
                    rows.push(Row::ShowMore {
                        parent: id,
                        depth,
                        remaining: hidden.len(),
                    });
                }
                if !excluded.is_empty() {
                    rows.push(Row::Excluded {
                        parent: id,
                        depth,
                        count: excluded.len(),
                    });
                }
                if *truncated {
                    rows.push(Row::Truncated { parent: id, depth });
                }
            }
        }
    }

    /// Returns whether anything under `id` (or `id` itself) matched.
    fn push_filtered_rows(&self, id: NodeId, filter: &str, rows: &mut Vec<Row>) -> bool {
        let Some(node) = self.nodes.get(&id) else {
            return false;
        };
        let position = rows.len();
        let mut any_child_matched = false;
        if node.expanded {
            for child in self.children(id) {
                any_child_matched |= self.push_filtered_rows(*child, filter, rows);
            }
        }
        let matched = node.symbol.name.to_lowercase().contains(filter);
        if matched || any_child_matched {
            rows.insert(
                position,
                Row::Node {
                    id,
                    depth: node.depth,
                },
            );
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_row_depth_and_node() {
        let row = Row::ShowMore {
            parent: NodeId(3),
            depth: 2,
            remaining: 5,
        };
        assert_eq!(row.depth(), 2);
        assert_eq!(row.node(), NodeId(3));
        let row = Row::Excluded {
            parent: NodeId(4),
            depth: 1,
            count: 2,
        };
        assert_eq!(row.depth(), 1);
        assert_eq!(row.node(), NodeId(4));
    }
}
