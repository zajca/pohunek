//! A session-family view over one `session.list` answer.
//!
//! The tree maps `spawned_by` attributions onto direct children, so every
//! caller (the list handler, the retention selector, the remove-tree cascade,
//! the CLI `--tree` renderer) shares one definition of children, orphans and
//! descendants. The view is read-only and borrows the listed sessions: it
//! allocates only the index, never the sessions.
//!
//! Nothing here crosses compatibility versions: the daemon accepts the shape
//! on every supported negotiation and drops only the wire value an older peer
//! never defined.

// Rust guideline compliant 2026-11-13

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::session::{SessionId, SessionInfo};

/// One node of a [`SessionTree`] walk, in walk order.
///
/// `parent` is the direct parent the walk came from and `depth` counts edges
/// below the walked root; the walked root itself is never emitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "TreeNode.ts"))]
pub struct TreeNode {
    /// Walked session.
    pub session_id: SessionId,
    /// Direct parent of `session_id`.
    pub parent: Option<SessionId>,
    /// Edges below the walked root, so direct children carry 1.
    pub depth: u32,
}

/// Children, orphans and descendants over one `session.list` answer.
///
/// The index maps every listed session id to its position; children group by
/// the `spawned_by` session id and are sorted by id, so every walk is stable.
/// An orphan is a session whose `spawned_by` names an id absent from this
/// list: its parent is gone, lives on another host, or is only filtered out.
/// A top-level session either carries no `spawned_by` or is an orphan.
#[derive(Debug)]
pub struct SessionTree<'a> {
    sessions: &'a [SessionInfo],
    index: BTreeMap<&'a str, usize>,
    children: BTreeMap<&'a str, Vec<usize>>,
}

impl<'a> SessionTree<'a> {
    /// Indexes `sessions` by id and groups direct children by their parent id.
    #[must_use]
    pub fn new(sessions: &'a [SessionInfo]) -> Self {
        let mut index = BTreeMap::new();
        for (position, session) in sessions.iter().enumerate() {
            index.insert(session.id.0.as_str(), position);
        }
        let mut children: BTreeMap<&'a str, Vec<usize>> = BTreeMap::new();
        for (position, session) in sessions.iter().enumerate() {
            if let Some(spawned_by) = session.spawned_by.as_ref() {
                children
                    .entry(spawned_by.session_id.0.as_str())
                    .or_default()
                    .push(position);
            }
        }
        for positions in children.values_mut() {
            positions.sort_by(|left, right| sessions[*left].id.0.cmp(&sessions[*right].id.0));
        }
        Self {
            sessions,
            index,
            children,
        }
    }

    /// Direct-child count of `id`, zero when `id` spawned nothing listed.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, reason = "list sizes fit u32")]
    pub fn child_count(&self, id: &SessionId) -> u32 {
        self.children
            .get(id.0.as_str())
            .map_or(0, |positions| positions.len() as u32)
    }

    /// Whether `session` names a parent that is not in this list.
    #[must_use]
    pub fn is_orphan(&self, session: &SessionInfo) -> bool {
        session
            .spawned_by
            .as_ref()
            .is_some_and(|spawned_by| !self.index.contains_key(spawned_by.session_id.0.as_str()))
    }

    /// Whether `session` is a tree root: no `spawned_by`, or an orphan.
    #[must_use]
    pub fn is_top_level(&self, session: &SessionInfo) -> bool {
        session.spawned_by.is_none() || self.is_orphan(session)
    }

    /// Direct children of `id`, sorted by id.
    #[must_use]
    pub fn children(&self, id: &SessionId) -> Vec<&'a SessionInfo> {
        self.children
            .get(id.0.as_str())
            .map_or(Vec::new(), |positions| {
                positions
                    .iter()
                    .map(|position| &self.sessions[*position])
                    .collect()
            })
    }

    /// Descendants of `root` deepest first, siblings by id, root excluded.
    ///
    /// The visited set keeps a `spawned_by` cycle from looping: the first
    /// visit wins and later ones are skipped.
    #[must_use]
    pub fn descendants_post_order(&self, root: &SessionId) -> Vec<TreeNode> {
        let mut order = Vec::new();
        let mut visited = std::collections::BTreeSet::new();
        visited.insert(root.0.clone());
        self.append_post_order(root.0.as_str(), 1, &mut visited, &mut order);
        order
    }

    fn append_post_order(
        &self,
        id: &str,
        depth: u32,
        visited: &mut std::collections::BTreeSet<String>,
        order: &mut Vec<TreeNode>,
    ) {
        let Some(positions) = self.children.get(id) else {
            return;
        };
        for position in positions.clone() {
            let session = &self.sessions[position];
            if !visited.insert(session.id.0.clone()) {
                continue;
            }
            let child_id = session.id.0.clone();
            self.append_post_order(&child_id, depth.saturating_add(1), visited, order);
            order.push(TreeNode {
                session_id: session.id.clone(),
                parent: session
                    .spawned_by
                    .as_ref()
                    .map(|spawned| spawned.session_id.clone()),
                depth,
            });
        }
    }

    /// Every session paired with its display depth, roots first.
    ///
    /// Roots are the top-level sessions sorted by id; each root is followed
    /// by its subtree pre-order. Sessions no root reaches (only a
    /// `spawned_by` cycle can strand one) are appended at depth 0, so no
    /// session is ever dropped from the display.
    #[must_use]
    pub fn display_order(&self) -> Vec<(&'a SessionInfo, u32)> {
        let mut order = Vec::with_capacity(self.sessions.len());
        let mut visited = std::collections::BTreeSet::new();
        let mut roots: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| self.is_top_level(session))
            .map(|(position, _)| position)
            .collect();
        roots.sort_by(|left, right| self.sessions[*left].id.0.cmp(&self.sessions[*right].id.0));
        for root in roots {
            self.append_display(root, 0, &mut visited, &mut order);
        }
        let mut stranded: Vec<usize> = (0..self.sessions.len())
            .filter(|position| !visited.contains(position))
            .collect();
        stranded.sort_by(|left, right| self.sessions[*left].id.0.cmp(&self.sessions[*right].id.0));
        for position in stranded {
            visited.insert(position);
            order.push((&self.sessions[position], 0));
        }
        order
    }

    fn append_display(
        &self,
        position: usize,
        depth: u32,
        visited: &mut std::collections::BTreeSet<usize>,
        order: &mut Vec<(&'a SessionInfo, u32)>,
    ) {
        if !visited.insert(position) {
            return;
        }
        order.push((&self.sessions[position], depth));
        let id = self.sessions[position].id.0.clone();
        if let Some(children) = self.children.get(id.as_str()) {
            for child in children.clone() {
                let child_depth = depth.saturating_add(1);
                self.append_display(child, child_depth, visited, order);
            }
        }
    }
}
