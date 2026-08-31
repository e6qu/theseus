// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Timeline tree — the multiverse as a data structure.
//!
//! Each node is one timeline: a run of the system from a branch point with a
//! particular seed. Nodes that are branch points carry a captured
//! [`BranchPoint`] payload from which children are spawned. Exploration order
//! is deterministic DFS (depth-first, children in creation order), so the
//! search itself is reproducible.
//!
//! This module is generic over the payload and contains no KVM/Vmm
//! references: fully testable without hardware.

/// Identifier of a node in the tree.
pub type NodeId = u64;

/// One timeline in the tree.
#[derive(Debug)]
pub struct TimelineNode<P> {
    /// This timeline's seed.
    pub seed: u64,
    /// Parent timeline, if any. A child diverges from its parent only by
    /// events after the parent's branch point.
    pub parent: Option<NodeId>,
    /// Depth in the tree (root = 0).
    pub depth: u32,
    /// Children in creation order.
    children: Vec<NodeId>,
    /// The payload (e.g. a captured branch point), if this node is one.
    pub payload: Option<P>,
}

/// A tree of timelines.
#[derive(Debug)]
pub struct TimelineTree<P> {
    nodes: Vec<TimelineNode<P>>,
}

impl<P> TimelineTree<P> {
    /// A new tree rooted at a timeline with the given seed.
    pub fn new(root_seed: u64, root_payload: P) -> Self {
        TimelineTree {
            nodes: vec![TimelineNode {
                seed: root_seed,
                parent: None,
                depth: 0,
                children: Vec::new(),
                payload: Some(root_payload),
            }],
        }
    }

    /// Add a child timeline branching from `parent`. Returns the new node's id.
    pub fn add_child(&mut self, parent: NodeId, seed: u64, payload: P) -> NodeId {
        let id = self.nodes.len() as NodeId;
        let depth = self.nodes[parent as usize].depth + 1;
        self.nodes.push(TimelineNode {
            seed,
            parent: Some(parent),
            depth,
            children: Vec::new(),
            payload: Some(payload),
        });
        self.nodes[parent as usize].children.push(id);
        id
    }

    /// Number of timelines in the tree.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the tree contains only the root.
    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    /// Get a node.
    pub fn node(&self, id: NodeId) -> &TimelineNode<P> {
        &self.nodes[id as usize]
    }

    /// Mutable access to a node's payload.
    pub fn payload_mut(&mut self, id: NodeId) -> &mut P {
        self.nodes[id as usize]
            .payload
            .as_mut()
            .expect("node has no payload")
    }

    /// Deterministic exploration order: depth-first, children in creation
    /// order. Pure function of tree shape.
    pub fn exploration_order(&self) -> Vec<NodeId> {
        let mut order = Vec::with_capacity(self.nodes.len());
        self.dfs(0, &mut order);
        order
    }

    fn dfs(&self, id: NodeId, order: &mut Vec<NodeId>) {
        order.push(id);
        for &child in &self.nodes[id as usize].children {
            self.dfs(child, order);
        }
    }

    /// The chain of seeds from the root to `id` (inclusive), in order.
    /// Replaying this sequence against the system reproduces the timeline.
    pub fn seed_path(&self, id: NodeId) -> Vec<u64> {
        let mut path = Vec::new();
        let mut cur = Some(id);
        while let Some(nid) = cur {
            let node = &self.nodes[nid as usize];
            path.push(node.seed);
            cur = node.parent;
        }
        path.reverse();
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tree_shape_and_order() {
        // root(0)
        // ├── a(1)
        // │   ├── c(3)
        // │   └── d(4)
        // └── b(2)
        let mut tree = TimelineTree::new(100, "root");
        let a = tree.add_child(0, 200, "a");
        let b = tree.add_child(0, 300, "b");
        let c = tree.add_child(a, 400, "c");
        let d = tree.add_child(a, 500, "d");

        assert_eq!(tree.len(), 5);
        assert!(!tree.is_empty());
        assert_eq!(tree.node(c).depth, 2);
        assert_eq!(tree.node(d).parent, Some(a));
        assert_eq!(tree.node(b).depth, 1);

        // DFS, children in creation order.
        assert_eq!(tree.exploration_order(), vec![0, a, c, d, b]);

        // Seed paths.
        assert_eq!(tree.seed_path(d), vec![100, 200, 500]);
        assert_eq!(tree.seed_path(0), vec![100]);
    }

    #[test]
    fn test_exploration_order_is_reproducible() {
        let build = || {
            let mut tree = TimelineTree::new(1, ());
            let mut parents = vec![0];
            for i in 1..50u64 {
                // Deterministic pseudo-random-ish parent choice.
                let parent = parents[(i as usize * 7) % parents.len()];
                parents.push(tree.add_child(parent, i * 1000, ()));
            }
            tree.exploration_order()
        };
        assert_eq!(build(), build());
    }
}
