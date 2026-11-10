//! Aggregated sample tree, shared between the reader thread and the UI.
//!
//! Every node keeps three sample counters, mirroring btdu:
//! - `represented`: samples attributed to this subtree (each sample is
//!   attributed to exactly one path, so these add up to the total);
//! - `exclusive`: samples whose references all lie inside this subtree,
//!   i.e. roughly what deleting it would free;
//! - `shared`: samples with at least one reference inside this subtree,
//!   i.e. what `du` would report;
//! - `distributed`: each sample split evenly between all its references
//!   (1/n each), so snapshots sharing data each carry a fair share.

use std::collections::HashMap;
use std::time::Instant;

use crate::protocol::{Header, Message};

pub type NodeId = u32;
pub const ROOT: NodeId = 0;
const MAX_EXAMPLE_REFS: usize = 64;

pub struct Node {
    pub name: Box<str>,
    pub parent: NodeId,
    pub children: HashMap<Box<str>, NodeId>,
    pub represented: u64,
    pub exclusive: u64,
    pub shared: u64,
    pub distributed: f64,
    /// For data reached through several paths: the other paths of the
    /// first sample attributed here (snapshots, reflinks, hard links…).
    pub also_seen_at: Option<Box<[NodeId]>>,
}

impl Node {
    fn new(name: &str, parent: NodeId) -> Self {
        Self {
            name: name.into(),
            parent,
            children: HashMap::new(),
            represented: 0,
            exclusive: 0,
            shared: 0,
            distributed: 0.0,
            also_seen_at: None,
        }
    }

    pub fn is_special(&self) -> bool {
        self.name.starts_with('<') && self.name.ends_with('>')
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Starting,
    Running,
    Finished(String),
    Failed(String),
}

pub struct Model {
    pub nodes: Vec<Node>,
    /// Protocol path id → node.
    paths: Vec<NodeId>,
    pub header: Option<Header>,
    pub samples: u64,
    pub status: Status,
    pub paused: bool,
    pub log: Vec<String>,
    /// Bumped on every change so the UI knows when to repaint.
    pub version: u64,
    /// For the samples/s readout: (when, samples) at the previous UI tick.
    pub rate_mark: (Instant, u64),
    pub rate: f64,
    scratch: Vec<u32>,
    scratch_gen: u32,
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

impl Model {
    pub fn new() -> Self {
        Self {
            nodes: vec![Node::new("", ROOT)],
            paths: Vec::new(),
            header: None,
            samples: 0,
            status: Status::Starting,
            paused: false,
            log: Vec::new(),
            version: 0,
            rate_mark: (Instant::now(), 0),
            rate: 0.0,
            scratch: vec![0],
            scratch_gen: 0,
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.header.as_ref().map_or(0, |h| h.total_bytes)
    }

    /// Estimated size in bytes of `count` samples.
    pub fn bytes(&self, count: u64) -> f64 {
        if self.samples == 0 {
            return 0.0;
        }
        count as f64 / self.samples as f64 * self.total_bytes() as f64
    }

    /// Estimated size in bytes of a fractional sample count.
    pub fn bytes_f(&self, count: f64) -> f64 {
        if self.samples == 0 {
            return 0.0;
        }
        count / self.samples as f64 * self.total_bytes() as f64
    }

    /// One standard error of the estimate for `count` samples, in bytes.
    pub fn margin(&self, count: u64) -> f64 {
        if self.samples == 0 {
            return 0.0;
        }
        let n = self.samples as f64;
        let p = count as f64 / n;
        (p * (1.0 - p) / n).sqrt() * self.total_bytes() as f64
    }

    /// Precision of the sizes shown inside `dir`: the worst-case standard
    /// error of any of its entries, as a fraction of `dir`'s own size,
    /// plus the total sample count needed to bring it down to `target`.
    pub fn precision(&self, dir: NodeId, target: f64) -> Option<(f64, f64)> {
        let n = self.samples as f64;
        let d = self.nodes[dir as usize].represented as f64;
        if n == 0.0 || d == 0.0 {
            return None;
        }
        let f = d / n;
        // A binomial proportion q has standard error sqrt(n q (1 - q)) in
        // samples, largest at q = 1/2; entries of `dir` have q <= f.
        let (err, needed) = if f >= 0.5 {
            (
                1.0 / (2.0 * f * n.sqrt()),
                1.0 / (4.0 * f * f * target * target),
            )
        } else {
            (
                ((1.0 - f) / (n * f)).sqrt(),
                (1.0 - f) / (f * target * target),
            )
        };
        Some((err, needed))
    }

    pub fn apply(&mut self, msg: Message) {
        match msg {
            Message::Header(h) => {
                self.header = Some(h);
                self.status = Status::Running;
            }
            Message::Path { id, path } => {
                let node = self.intern(&path);
                let id = id as usize;
                if self.paths.len() <= id {
                    self.paths.resize(id + 1, ROOT);
                }
                self.paths[id] = node;
            }
            Message::Sample { paths } => {
                let nodes: Vec<NodeId> = paths
                    .iter()
                    .filter_map(|&id| self.paths.get(id as usize).copied())
                    .collect();
                self.add_sample(&nodes);
            }
            Message::Fatal(text) => self.status = Status::Failed(text),
        }
        self.version += 1;
    }

    fn intern(&mut self, path: &str) -> NodeId {
        let mut cur = ROOT;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur = match self.nodes[cur as usize].children.get(comp) {
                Some(&id) => id,
                None => {
                    let id = self.nodes.len() as NodeId;
                    self.nodes.push(Node::new(comp, cur));
                    self.nodes[cur as usize].children.insert(comp.into(), id);
                    id
                }
            };
        }
        cur
    }

    fn depth(&self, mut node: NodeId) -> usize {
        let mut d = 0;
        while node != ROOT {
            node = self.nodes[node as usize].parent;
            d += 1;
        }
        d
    }

    fn lca(&self, a: NodeId, b: NodeId) -> NodeId {
        let (mut a, mut b) = (a, b);
        let (mut da, mut db) = (self.depth(a), self.depth(b));
        while da > db {
            a = self.nodes[a as usize].parent;
            da -= 1;
        }
        while db > da {
            b = self.nodes[b as usize].parent;
            db -= 1;
        }
        while a != b {
            a = self.nodes[a as usize].parent;
            b = self.nodes[b as usize].parent;
        }
        a
    }

    fn walk_up(&mut self, mut node: NodeId, mut f: impl FnMut(&mut Node)) {
        loop {
            let n = &mut self.nodes[node as usize];
            f(n);
            if node == ROOT {
                return;
            }
            node = n.parent;
        }
    }

    fn add_sample(&mut self, nodes: &[NodeId]) {
        let Some(&first) = nodes.first() else {
            return;
        };
        self.samples += 1;
        self.walk_up(first, |n| n.represented += 1);

        if nodes.len() == 1 {
            self.walk_up(first, |n| {
                n.exclusive += 1;
                n.shared += 1;
                n.distributed += 1.0;
            });
            return;
        }

        let share = 1.0 / nodes.len() as f64;
        for &node in nodes {
            self.walk_up(node, |n| n.distributed += share);
        }

        let lca = nodes[1..].iter().fold(first, |acc, &n| self.lca(acc, n));
        self.walk_up(lca, |n| n.exclusive += 1);

        // Count `shared` once per node even if several paths go through it.
        self.scratch_gen = self.scratch_gen.wrapping_add(1);
        if self.scratch_gen == 0 {
            self.scratch.fill(0);
            self.scratch_gen = 1;
        }
        self.scratch.resize(self.nodes.len(), 0);
        let generation = self.scratch_gen;
        for &start in nodes {
            let mut node = start;
            loop {
                if self.scratch[node as usize] == generation {
                    break;
                }
                self.scratch[node as usize] = generation;
                self.nodes[node as usize].shared += 1;
                if node == ROOT {
                    break;
                }
                node = self.nodes[node as usize].parent;
            }
        }

        let rep = &mut self.nodes[first as usize];
        if rep.also_seen_at.is_none() {
            rep.also_seen_at = Some(nodes[1..].iter().take(MAX_EXAMPLE_REFS).copied().collect());
        }
    }

    /// Path of `node` relative to the top-level subvolume, `/`-separated.
    pub fn path_of(&self, mut node: NodeId) -> String {
        let mut parts = Vec::new();
        while node != ROOT {
            let n = &self.nodes[node as usize];
            parts.push(&*n.name);
            node = n.parent;
        }
        parts.reverse();
        parts.join("/")
    }

    /// Chain of ancestors from the root down to `node`, inclusive.
    pub fn ancestry(&self, mut node: NodeId) -> Vec<NodeId> {
        let mut chain = vec![node];
        while node != ROOT {
            node = self.nodes[node as usize].parent;
            chain.push(node);
        }
        chain.reverse();
        chain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(m: &mut Model, samples: &[&[&str]]) {
        let mut ids: HashMap<String, u32> = HashMap::new();
        m.apply(Message::Header(Header {
            total_bytes: 1000,
            nodesize: 16384,
            devices: 1,
            threads: 1,
        }));
        for s in samples {
            let mut sample = Vec::new();
            for p in *s {
                let next = ids.len() as u32;
                let id = *ids.entry((*p).to_owned()).or_insert_with(|| {
                    m.apply(Message::Path {
                        id: next,
                        path: (*p).to_owned(),
                    });
                    next
                });
                sample.push(id);
            }
            m.apply(Message::Sample { paths: sample });
        }
    }

    fn get<'a>(m: &'a Model, path: &str) -> &'a Node {
        let mut cur = ROOT;
        for c in path.split('/') {
            cur = m.nodes[cur as usize].children[c];
        }
        &m.nodes[cur as usize]
    }

    #[test]
    fn counters() {
        let mut m = Model::new();
        feed(
            &mut m,
            &[
                &["@home/a/x"],
                &["@home/a/y", "snap/a/y"],
                &["@home/a/z", "@home/b/z"],
                &["<UNALLOCATED>"],
            ],
        );
        assert_eq!(m.samples, 4);
        let root = &m.nodes[ROOT as usize];
        assert_eq!((root.represented, root.exclusive, root.shared), (4, 4, 4));

        let home = get(&m, "@home");
        assert_eq!((home.represented, home.exclusive, home.shared), (3, 2, 3));
        let a = get(&m, "@home/a");
        assert_eq!((a.represented, a.exclusive, a.shared), (3, 1, 3));
        let snap = get(&m, "snap");
        assert_eq!((snap.represented, snap.exclusive, snap.shared), (0, 0, 1));
        assert_eq!(snap.distributed, 0.5);
        assert_eq!(home.distributed, 2.5);
        assert_eq!(m.nodes[ROOT as usize].distributed, 4.0);
        let b = get(&m, "@home/b");
        assert_eq!((b.represented, b.exclusive, b.shared), (0, 0, 1));
        assert_eq!(
            get(&m, "@home/a/y")
                .also_seen_at
                .as_deref()
                .map(|s| s.len()),
            Some(1)
        );
        assert_eq!(m.bytes(1), 250.0);
        // Root: worst case q = 1/2 over 4 samples → 1/(2·2) = 25 %.
        let (err, needed) = m.precision(ROOT, 0.01).unwrap();
        assert!((err - 0.25).abs() < 1e-9);
        assert!((needed - 2500.0).abs() < 1e-6);
        assert_eq!(m.path_of(m.nodes[ROOT as usize].children["@home"]), "@home");
    }
}
