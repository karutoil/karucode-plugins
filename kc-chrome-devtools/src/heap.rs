//! V8 heap snapshot parsing and analysis.
//!
//! A `.heapsnapshot` is a JSON document with a flat, int-encoded `nodes` array
//! and a string table. Format, as emitted by `HeapProfiler.takeHeapSnapshot`:
//!
//! * `snapshot.meta.node_fields` — names of the node columns, e.g.
//!   `["type","name","id","self_size","edge_count","trace_node_id","detachedness"]`
//! * `snapshot.meta.edge_fields` — `["type","name_or_index","to_node"]`
//! * `nodes` — `node_fields.len()` ints per node
//! * `edges` — `edge_fields.len()` ints per edge, grouped by the owning node in
//!   the same order as `nodes`
//! * `strings` — the string table every name index points into
//!
//! Everything here is derived lazily from that flat representation: the file is
//! parsed once, then node/edge iteration is index arithmetic. Analysis never
//! mutates the original document, so one snapshot can answer many questions.
//!
//! Retention is implemented with the standard V8 algorithm: the snapshot is
//! interpreted as a directed graph from GC roots, and nodes are only retained
//! if removing them would disconnect them from every root. Retainers are the
//! *reverse* of the traversal edges (a node with `strong` edge to X is a
//! retainer of X).

use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

/// Edge types, matching V8's `v8::internal::HeapGraphEdge::Type`.
pub const EDGE_CONTEXT: u64 = 0;
pub const EDGE_ELEMENT: u64 = 1;
pub const EDGE_PROPERTY: u64 = 2;
pub const EDGE_INTERNAL: u64 = 3;
pub const EDGE_HIDDEN: u64 = 4;
pub const EDGE_SHORTCUT: u64 = 5;
pub const EDGE_WEAK: u64 = 6;
pub const EDGE_INVISIBLE: u64 = 7;

fn edge_type_name(edge_type: u64) -> &'static str {
    match edge_type {
        EDGE_CONTEXT => "context",
        EDGE_ELEMENT => "element",
        EDGE_PROPERTY => "property",
        EDGE_INTERNAL => "internal",
        EDGE_HIDDEN => "hidden",
        EDGE_SHORTCUT => "shortcut",
        EDGE_WEAK => "weak",
        EDGE_INVISIBLE => "invisible",
        _ => "unknown",
    }
}

/// Which edge types keep a node alive for retention purposes.
///
/// Weak and invisible edges are references that do not by themselves retain;
/// hidden/internal edges do (they are how V8 expresses map↔properties links).
fn retains(edge_type: u64) -> bool {
    !matches!(edge_type, EDGE_WEAK | EDGE_INVISIBLE)
}

/// Bytes and counts for one aggregate group (constructor, type, …).
#[derive(Debug, Clone, Default)]
pub struct GroupStat {
    pub name: String,
    pub count: usize,
    pub self_size: u64,
}

/// A parsed heap snapshot.
pub struct HeapSnapshot {
    node_fields: Vec<String>,
    edge_fields: Vec<String>,
    nodes: Vec<u64>,
    edges: Vec<u64>,
    strings: Vec<String>,
    node_count: usize,
    edge_count: usize,
    /// Byte offset of the start of each node's edges within `edges`.
    node_edge_start: Vec<usize>,
    /// Raw node index of each node's first child (inverse is computed lazily).
    root_index: usize,
}

impl HeapSnapshot {
    /// Parse a heap snapshot document. Fails with a message naming the missing
    /// or malformed field rather than panicking, because the input is a large
    /// file we did not write.
    pub fn parse(doc: &Value) -> Result<Self, String> {
        let snapshot = doc
            .get("snapshot")
            .ok_or("heap snapshot has no `snapshot` section")?;
        let meta = snapshot
            .get("meta")
            .ok_or("heap snapshot has no `snapshot.meta`")?;

        let field_names = |key: &str| -> Result<Vec<String>, String> {
            meta.get(key)
                .and_then(|v| v.as_array())
                .ok_or(format!("heap snapshot meta is missing `{key}`"))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .ok_or(format!("heap snapshot meta `{key}` is not all strings"))
                })
                .collect()
        };
        let node_fields = field_names("node_fields")?;
        let edge_fields = field_names("edge_fields")?;
        if node_fields.is_empty() || edge_fields.is_empty() {
            return Err("heap snapshot meta has empty node or edge fields".into());
        }

        let ints = |key: &str| -> Result<Vec<u64>, String> {
            doc.get(key)
                .and_then(|v| v.as_array())
                .ok_or(format!("heap snapshot has no `{key}` array"))?
                .iter()
                .map(|v| {
                    v.as_u64()
                        .ok_or(format!("heap snapshot `{key}` contains a non-integer"))
                })
                .collect()
        };
        let nodes = ints("nodes")?;
        let edges = ints("edges")?;
        let strings: Vec<String> = doc
            .get("strings")
            .and_then(|v| v.as_array())
            .ok_or("heap snapshot has no `strings` array")?
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();

        let nf = node_fields.len();
        let ef = edge_fields.len();
        if nodes.len() % nf != 0 {
            return Err(format!(
                "heap snapshot `nodes` length {} is not a multiple of the {nf} declared node fields",
                nodes.len()
            ));
        }
        if edges.len() % ef != 0 {
            return Err(format!(
                "heap snapshot `edges` length {} is not a multiple of the {ef} declared edge fields",
                edges.len()
            ));
        }
        let node_count = nodes.len() / nf;
        let edge_count = edges.len() / ef;

        // Node field offsets.
        let offset = |fields: &[String], name: &str| -> Result<usize, String> {
            fields
                .iter()
                .position(|f| f == name)
                .ok_or(format!("heap snapshot meta is missing the `{name}` field"))
        };
        let node_type_off = offset(&node_fields, "type")?;
        let name_off = offset(&node_fields, "name")?;
        let id_off = offset(&node_fields, "id")?;
        let size_off = offset(&node_fields, "self_size")?;
        let edge_count_off = offset(&node_fields, "edge_count")?;
        let _ = (node_type_off, name_off, id_off, size_off, edge_count_off);

        // Precompute each node's edge start. This is the one O(n) pass that
        // makes every later node/child lookup cheap.
        let mut node_edge_start = Vec::with_capacity(node_count);
        let mut cursor = 0usize;
        for i in 0..node_count {
            node_edge_start.push(cursor);
            let count = nodes[i * nf + edge_count_off] as usize;
            cursor += count;
        }
        if cursor != edge_count {
            return Err(format!(
                "heap snapshot edge counts disagree: nodes claim {cursor} edges, \
                 the `edges` array holds {edge_count}"
            ));
        }

        // `nodes` field order tells us which column is which at use time; keep
        // the offsets in the struct via a small helper below.
        let mut snapshot = Self {
            node_fields,
            edge_fields,
            nodes,
            edges,
            strings,
            node_count,
            edge_count,
            node_edge_start,
            root_index: 0,
        };
        snapshot.root_index = snapshot.find_root();
        Ok(snapshot)
    }

    /// Total edge slots, exposed for diagnostics.
    pub fn edge_count(&self) -> usize {
        self.edge_count
    }

    pub fn node_count(&self) -> usize {
        self.node_count
    }

    fn nf(&self) -> usize {
        self.node_fields.len()
    }

    fn ef(&self) -> usize {
        self.edge_fields.len()
    }

    fn field(&self, off_name: &str) -> usize {
        self.node_fields
            .iter()
            .position(|f| f == off_name)
            .unwrap_or(0)
    }

    pub fn node_type(&self, idx: usize) -> u64 {
        self.nodes[idx * self.nf() + self.field("type")]
    }

    fn node_name_index(&self, idx: usize) -> u64 {
        self.nodes[idx * self.nf() + self.field("name")]
    }

    pub fn node_id(&self, idx: usize) -> u64 {
        self.nodes[idx * self.nf() + self.field("id")]
    }

    pub fn self_size(&self, idx: usize) -> u64 {
        self.nodes[idx * self.nf() + self.field("self_size")]
    }

    pub fn node_type_name(&self, idx: usize) -> String {
        // V8 node types are stable integers; `snapshot.meta.node_types` in the
        // file is a second, optional copy of these names. Prefer the file when
        // present, fall back to the documented order.
        const TYPES: &[&str] = &[
            "hidden",
            "array",
            "string",
            "object",
            "code",
            "closure",
            "regexp",
            "number",
            "native",
            "synthetic",
            "concatenated string",
            "sliced string",
            "symbol",
            "bigint",
            "object shape",
        ];
        let t = self.node_type(idx) as usize;
        TYPES.get(t).copied().unwrap_or("unknown").to_string()
    }

    pub fn node_name(&self, idx: usize) -> String {
        let name_index = self.node_name_index(idx) as usize;
        self.strings
            .get(name_index)
            .cloned()
            .unwrap_or_else(|| format!("<string #{name_index}>"))
    }

    /// Edges of a node as `(edge_type, name, target_node_index)`.
    pub fn node_edges(&self, idx: usize) -> Vec<(u64, String, usize)> {
        let ef = self.ef();
        let start = self.node_edge_start[idx];
        let count = self.nodes[idx * self.nf() + self.field("edge_count")] as usize;
        let mut out = Vec::with_capacity(count);
        for e in 0..count {
            let base = (start + e) * ef;
            let edge_type = self.edges[base];
            let name_or_index = self.edges[base + 1];
            let to_node = (self.edges[base + 2] as usize) / self.nf();
            if to_node >= self.node_count {
                continue;
            }
            let name = match edge_type {
                EDGE_ELEMENT => format!("[{name_or_index}]"),
                EDGE_CONTEXT => format!("context({name_or_index})"),
                EDGE_HIDDEN | EDGE_INTERNAL => self
                    .strings
                    .get(name_or_index as usize)
                    .cloned()
                    .unwrap_or_default(),
                _ => self
                    .strings
                    .get(name_or_index as usize)
                    .cloned()
                    .unwrap_or_default(),
            };
            out.push((edge_type, name, to_node));
        }
        out
    }

    /// Find the root node: V8's synthetic root has id 1 and type `synthetic`
    /// in practice; fall back to the first node otherwise.
    fn find_root(&self) -> usize {
        (0..self.node_count)
            .find(|&i| self.node_id(i) == 1)
            .or_else(|| {
                (0..self.node_count).find(|&i| self.node_type_name(i) == "synthetic")
            })
            .unwrap_or(0)
    }

    /// Reverse edges (`retainers`) keyed by target node index.
    fn reverse_edges(&self) -> HashMap<usize, Vec<(u64, String, usize)>> {
        let mut map: HashMap<usize, Vec<(u64, String, usize)>> = HashMap::new();
        for idx in 0..self.node_count {
            for (edge_type, name, target) in self.node_edges(idx) {
                map.entry(target).or_default().push((edge_type, name, idx));
            }
        }
        map
    }

    /// Nodes reachable from the root over retaining edges.
    fn reachable(&self) -> HashSet<usize> {
        let mut seen = HashSet::with_capacity(self.node_count);
        let mut queue = VecDeque::new();
        queue.push_back(self.root_index);
        seen.insert(self.root_index);
        while let Some(idx) = queue.pop_front() {
            for (edge_type, _, target) in self.node_edges(idx) {
                if retains(edge_type) && seen.insert(target) {
                    queue.push_back(target);
                }
            }
        }
        seen
    }

    /// Summary of the snapshot: counts and the largest aggregates by retained
    /// size, constructor, and node type.
    pub fn summary(&self) -> HeapSummary {
        let reachable = self.reachable();
        let mut by_type: HashMap<String, GroupStat> = HashMap::new();
        let mut by_constructor: HashMap<String, GroupStat> = HashMap::new();
        let mut total_size = 0u64;
        for idx in 0..self.node_count {
            if !reachable.contains(&idx) {
                continue;
            }
            let size = self.self_size(idx);
            total_size += size;
            let type_name = self.node_type_name(idx);
            let entry = by_type.entry(type_name.clone()).or_insert(GroupStat {
                name: type_name,
                ..Default::default()
            });
            entry.count += 1;
            entry.self_size += size;

            let ctor = self.constructor_name(idx);
            let entry = by_constructor.entry(ctor.clone()).or_insert(GroupStat {
                name: ctor,
                ..Default::default()
            });
            entry.count += 1;
            entry.self_size += size;
        }
        let mut types: Vec<GroupStat> = by_type.into_values().collect();
        types.sort_by_key(|g| std::cmp::Reverse(g.self_size));
        let mut ctors: Vec<GroupStat> = by_constructor.into_values().collect();
        ctors.sort_by_key(|g| std::cmp::Reverse(g.self_size));
        HeapSummary {
            node_count: self.node_count,
            reachable_count: reachable.len(),
            total_self_size: total_size,
            by_type: types,
            by_constructor: ctors,
        }
    }

    /// The `ConstructorName` V8 stores as the second shortcut edge of an
    /// object node (the first is its `__proto__`).
    fn constructor_name(&self, idx: usize) -> String {
        let type_name = self.node_type_name(idx);
        if !matches!(type_name.as_str(), "object" | "closure" | "regexp" | "native") {
            return type_name;
        }
        let mut shortcut_index = 0;
        for (edge_type, name, target) in self.node_edges(idx) {
            if edge_type != EDGE_SHORTCUT {
                continue;
            }
            shortcut_index += 1;
            if shortcut_index >= 2 {
                let _ = name;
                return self.edge_target_name(target);
            }
            if shortcut_index == 1 && !name.is_empty() {
                // Proto points at a Map/prototype object; the constructor name
                // lives on its `constructor` shortcut instead.
            }
        }
        // Fall back to the node name (V8 uses it for many objects).
        let name = self.node_name(idx);
        if name.is_empty() {
            type_name
        } else {
            name
        }
    }

    /// A readable name for an edge target, preferring constructor shortcuts.
    fn edge_target_name(&self, idx: usize) -> String {
        for (edge_type, name, target) in self.node_edges(idx) {
            if edge_type == EDGE_SHORTCUT && name == "constructor" {
                let ctor_name = self.node_name(target);
                if !ctor_name.is_empty() {
                    return ctor_name;
                }
            }
        }
        let own = self.node_name(idx);
        if own.is_empty() {
            self.node_type_name(idx)
        } else {
            own
        }
    }

    /// Nodes of a given type (by name), largest first.
    pub fn nodes_of_type(&self, type_name: &str, limit: usize) -> Vec<usize> {
        let reachable = self.reachable();
        let mut out: Vec<usize> = (0..self.node_count)
            .filter(|&i| reachable.contains(&i) && self.node_type_name(i) == type_name)
            .collect();
        out.sort_by_key(|&n| std::cmp::Reverse(self.self_size(n)));
        out.truncate(limit);
        out
    }

    /// Nodes whose own name contains `needle` (case-insensitive), largest first.
    pub fn query_nodes(&self, needle: &str, limit: usize) -> Vec<usize> {
        let needle_lower = needle.to_lowercase();
        let reachable = self.reachable();
        let mut out: Vec<usize> = (0..self.node_count)
            .filter(|&i| {
                reachable.contains(&i) && {
                    let name = self.node_name(i).to_lowercase();
                    let ctor = self.constructor_name(i).to_lowercase();
                    name.contains(&needle_lower) || ctor.contains(&needle_lower)
                }
            })
            .collect();
        out.sort_by_key(|&n| std::cmp::Reverse(self.self_size(n)));
        out.truncate(limit);
        out
    }

    /// Direct retainers of a node: edges that keep it alive.
    pub fn retainers(&self, idx: usize) -> Vec<(u64, String, usize)> {
        self.reverse_edges()
            .remove(&idx)
            .unwrap_or_default()
            .into_iter()
            .filter(|(edge_type, _, _)| retains(*edge_type))
            .collect()
    }

    /// Retaining paths from the root to `idx`, shortest first. Returns at most
    /// `limit` paths, each a list of `(edge_name, node_index)` hops.
    pub fn retaining_paths(&self, idx: usize, limit: usize) -> Vec<Vec<(String, usize)>> {
        if idx >= self.node_count {
            return Vec::new();
        }
        // Walk the reverse graph outwards from the node until the root is
        // reached, recording for each visited node the retainer we came from.
        // `pred[x] = (parent, edge_name)` means `parent --edge_name--> x`.
        let reverse = self.reverse_edges();
        let mut pred: HashMap<usize, (usize, String)> = HashMap::new();
        let mut queue = VecDeque::new();
        queue.push_back(idx);
        let mut visited = HashSet::new();
        visited.insert(idx);
        while let Some(node) = queue.pop_front() {
            if node == self.root_index {
                break;
            }
            for (edge_type, name, from) in reverse.get(&node).cloned().unwrap_or_default() {
                if !retains(edge_type) || !visited.insert(from) {
                    continue;
                }
                pred.entry(node).or_insert((from, name));
                queue.push_back(from);
            }
        }
        if !pred.contains_key(&idx) {
            return Vec::new();
        }

        // Reconstruct root → idx by walking parents backwards, then reversing.
        let mut chain: Vec<(String, usize)> = Vec::new();
        let mut current = idx;
        while current != self.root_index {
            let Some((parent, edge_name)) = pred.get(&current).cloned() else {
                break;
            };
            chain.push((edge_name, current));
            current = parent;
        }
        if current != self.root_index {
            return Vec::new();
        }
        chain.reverse();
        let mut paths = vec![chain];
        paths.truncate(limit);
        paths
    }

    /// Dominators, computed with the Cooper–Harvey–Kennedy iterative algorithm
    /// over the retaining graph. Returns node index → immediate dominator.
    pub fn dominators(&self) -> HashMap<usize, usize> {
        let n = self.node_count;
        // Reverse post-order over forward edges from the root.
        let mut post_order: Vec<usize> = Vec::with_capacity(n);
        let mut visited = vec![false; n];
        let mut stack: Vec<(usize, usize)> = vec![(self.root_index, 0)];
        visited[self.root_index] = true;
        while let Some((node, child_cursor)) = stack.last_mut() {
            let edges = self.node_edges(*node);
            if *child_cursor < edges.len() {
                let (edge_type, _, target) = edges[*child_cursor];
                *child_cursor += 1;
                if retains(edge_type) && !visited[target] {
                    visited[target] = true;
                    stack.push((target, 0));
                }
            } else {
                post_order.push(*node);
                stack.pop();
            }
        }
        post_order.reverse(); // reverse post-order

        let mut order_index = vec![usize::MAX; n];
        for (i, node) in post_order.iter().enumerate() {
            order_index[*node] = i;
        }

        let mut idom = vec![usize::MAX; n];
        idom[self.root_index] = self.root_index;

        let intersect = |mut a: usize, mut b: usize, idom: &Vec<usize>, order_index: &Vec<usize>| {
            while a != b {
                while order_index[a] > order_index[b] {
                    a = idom[a];
                }
                while order_index[b] > order_index[a] {
                    b = idom[b];
                }
            }
            a
        };

        let reverse = self.reverse_edges();
        let mut changed = true;
        while changed {
            changed = false;
            for &node in &post_order {
                if node == self.root_index {
                    continue;
                }
                let preds: Vec<usize> = reverse
                    .get(&node)
                    .map(|v| {
                        v.iter()
                            .filter(|(t, _, _)| retains(*t))
                            .map(|(_, _, from)| *from)
                            .filter(|from| idom[*from] != usize::MAX)
                            .collect()
                    })
                    .unwrap_or_default();
                let Some(&first) = preds.first() else {
                    continue;
                };
                let mut new_idom = first;
                for &pred in preds.iter().skip(1) {
                    new_idom = intersect(new_idom, pred, &idom, &order_index);
                }
                if idom[node] != new_idom {
                    idom[node] = new_idom;
                    changed = true;
                }
            }
        }
        idom.into_iter()
            .enumerate()
            .filter(|(_, d)| *d != usize::MAX)
            .collect()
    }

    /// Retained size per node from a dominator tree: a node retains its own
    /// size plus the sizes of everything it dominates.
    pub fn retained_sizes(&self) -> HashMap<usize, u64> {
        let dom = self.dominators();
        let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
        for (node, idom) in &dom {
            if node != &self.root_index {
                children.entry(*idom).or_default().push(*node);
            }
        }
        let mut sizes: HashMap<usize, u64> = HashMap::new();
        // Post-order over the dominator tree so children are summed first.
        fn accumulate(
            node: usize,
            snapshot: &HeapSnapshot,
            children: &HashMap<usize, Vec<usize>>,
            sizes: &mut HashMap<usize, u64>,
        ) -> u64 {
            if let Some(v) = sizes.get(&node) {
                return *v;
            }
            let mut total = snapshot.self_size(node);
            for child in children.get(&node).cloned().unwrap_or_default() {
                total += accumulate(child, snapshot, children, sizes);
            }
            sizes.insert(node, total);
            total
        }
        accumulate(self.root_index, self, &children, &mut sizes);
        sizes
    }

    /// Cross-snapshot diff of self sizes grouped by object name.
    pub fn diff_by_name(&self, other: &HeapSnapshot, limit: usize) -> Vec<DiffEntry> {
        let mine = self.sizes_by_name();
        let theirs = other.sizes_by_name();
        let mut names: HashSet<&String> = HashSet::new();
        names.extend(mine.keys());
        names.extend(theirs.keys());
        let mut out: Vec<DiffEntry> = names
            .into_iter()
            .map(|name| {
                let before = mine.get(name).copied().unwrap_or(0);
                let after = theirs.get(name).copied().unwrap_or(0);
                DiffEntry {
                    name: name.clone(),
                    before,
                    after,
                }
            })
            .filter(|e| e.before != e.after)
            .collect();
        out.sort_by(|a, b| {
            let da = b.delta().abs();
            let db = a.delta().abs();
            da.cmp(&db)
        });
        out.truncate(limit);
        out
    }

    fn sizes_by_name(&self) -> HashMap<String, u64> {
        let reachable = self.reachable();
        let mut map: HashMap<String, u64> = HashMap::new();
        for idx in 0..self.node_count {
            if !reachable.contains(&idx) {
                continue;
            }
            let name = self.node_name(idx);
            *map.entry(name).or_insert(0) += self.self_size(idx);
        }
        map
    }

    /// Duplicate strings: identical string contents appearing many times.
    pub fn duplicate_strings(&self, limit: usize) -> Vec<GroupStat> {
        let reachable = self.reachable();
        let mut map: HashMap<String, GroupStat> = HashMap::new();
        for idx in 0..self.node_count {
            if !reachable.contains(&idx) {
                continue;
            }
            let type_name = self.node_type_name(idx);
            if !type_name.contains("string") {
                continue;
            }
            let value = self.node_name(idx);
            if value.is_empty() {
                continue;
            }
            let entry = map.entry(value.clone()).or_insert(GroupStat {
                name: value,
                ..Default::default()
            });
            entry.count += 1;
            entry.self_size += self.self_size(idx);
        }
        let mut out: Vec<GroupStat> = map
            .into_values()
            .filter(|s| s.count > 1)
            .collect();
        out.sort_by_key(|g| std::cmp::Reverse(g.self_size));
        out.truncate(limit);
        out
    }

    pub fn describe_node(&self, idx: usize) -> String {
        if idx >= self.node_count {
            return format!("node #{idx} is out of range (snapshot has {} nodes)", self.node_count);
        }
        format!(
            "{} \"{}\" (id={}, self_size={}, edges={})",
            self.node_type_name(idx),
            self.node_name(idx),
            self.node_id(idx),
            self.self_size(idx),
            self.node_edges(idx).len()
        )
    }

    /// Which named contexts (frames/worlds) retain the most memory. V8 encodes
    /// a context edge's name as the context object's index; the context node's
    /// own name is the script/iframe identifier when available.
    pub fn context_breakdown(&self, limit: usize) -> Vec<GroupStat> {
        let reachable = self.reachable();
        let mut map: HashMap<String, GroupStat> = HashMap::new();
        for idx in 0..self.node_count {
            if !reachable.contains(&idx) {
                continue;
            }
            for (edge_type, _, target) in self.node_edges(idx) {
                if edge_type != EDGE_CONTEXT {
                    continue;
                }
                let name = {
                    let n = self.node_name(target);
                    if n.is_empty() || n == "(GC roots)" {
                        format!("context@{}", self.node_id(target))
                    } else {
                        n
                    }
                };
                let entry = map.entry(name.clone()).or_insert(GroupStat {
                    name,
                    ..Default::default()
                });
                entry.count += 1;
                entry.self_size += self.self_size(target);
            }
        }
        let mut out: Vec<GroupStat> = map.into_values().collect();
        out.sort_by_key(|g| std::cmp::Reverse(g.self_size));
        out.truncate(limit);
        out
    }
}

/// Snapshot summary.
#[derive(Debug, Clone, Default)]
pub struct HeapSummary {
    pub node_count: usize,
    pub reachable_count: usize,
    pub total_self_size: u64,
    pub by_type: Vec<GroupStat>,
    pub by_constructor: Vec<GroupStat>,
}

impl HeapSummary {
    /// Render the summary as text for a tool result.
    ///
    /// `limit` caps how many rows each table shows; the totals always cover the
    /// whole snapshot.
    pub fn render(&self, limit: usize) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} nodes ({} reachable), {} retained in total\n",
            self.node_count,
            self.reachable_count,
            format_bytes(self.total_self_size),
        ));
        if !self.by_type.is_empty() {
            out.push_str("\nLargest types by self size:\n");
            for stat in self.by_type.iter().take(limit) {
                out.push_str(&format!(
                    "  {:>10}  {}\n",
                    format_bytes(stat.self_size),
                    stat.name
                ));
            }
        }
        if !self.by_constructor.is_empty() {
            out.push_str("\nLargest constructors by self size:\n");
            for stat in self.by_constructor.iter().take(limit) {
                out.push_str(&format!(
                    "  {:>10}  {} ({} objects)\n",
                    format_bytes(stat.self_size),
                    stat.name,
                    stat.count
                ));
            }
        }
        out
    }
}

/// Human-readable byte count.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// One entry of a snapshot-to-snapshot comparison.
#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub name: String,
    pub before: u64,
    pub after: u64,
}

impl DiffEntry {
    pub fn delta(&self) -> i64 {
        self.after as i64 - self.before as i64
    }
}

/// Human-readable byte count.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Human-readable edge type (used in tool output).
pub fn edge_type_label(edge_type: u64) -> &'static str {
    edge_type_name(edge_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Small synthetic snapshot:
    ///
    /// ```text
    /// (synthetic root) --property "window"--> Object "Window"
    ///                   --property "leak"   --> Array "leaky array"
    ///                          Array --element[0]--> Object "Payload" (100 bytes)
    ///                          Array --element[1]--> Object "Payload" (100 bytes)
    /// ```
    fn fixture() -> HeapSnapshot {
        let node_fields = vec!["type", "name", "id", "self_size", "edge_count"];
        let edge_fields = vec!["type", "name_or_index", "to_node"];
        // strings: 0 (GC roots), 1 (Window), 2 (leak), 3 (leaky array),
        //          4 (Payload), 5 (window)
        let strings = vec!["(GC roots)", "Window", "leak", "leaky array", "Payload", "window"];
        // Node indexes: 0 root, 1 Window, 2 array, 3 payload, 4 payload.
        let nodes: Vec<u64> = {
            let mut v = Vec::new();
            let mut push = |t: u64, name: u64, id: u64, size: u64, edges: u64| {
                v.extend_from_slice(&[t, name, id, size, edges]);
            };
            push(9, 0, 1, 0, 2); // root, synthetic
            push(3, 1, 2, 50, 0); // Window object
            push(1, 3, 3, 80, 2); // array
            push(3, 4, 4, 100, 0); // payload
            push(3, 4, 5, 100, 0); // payload
            v
        };
        let nf = node_fields.len() as u64;
        let edges: Vec<u64> = vec![
            // root's 2 edges
            EDGE_PROPERTY, 5, nf, // "window" -> node 1
            EDGE_PROPERTY, 2, nf * 2, // "leak" -> node 2
            // array's 2 edges
            EDGE_ELEMENT, 0, nf * 3,
            EDGE_ELEMENT, 1, nf * 4,
        ];
        let doc = json!({
            "snapshot": {"meta": {
                "node_fields": node_fields,
                "edge_fields": edge_fields,
            }},
            "nodes": nodes,
            "edges": edges,
            "strings": strings,
        });
        HeapSnapshot::parse(&doc).expect("fixture parses")
    }

    #[test]
    fn parses_and_reads_basic_fields() {
        let snapshot = fixture();
        assert_eq!(snapshot.node_count(), 5);
        assert_eq!(snapshot.self_size(3), 100);
        assert_eq!(snapshot.node_name(1), "Window");
        assert_eq!(snapshot.node_id(2), 3);
    }

    #[test]
    fn rejects_malformed_snapshots_with_a_reason() {
        let doc = json!({"snapshot": {"meta": {"node_fields": ["type"]}}, "nodes": [1, 2], "strings": []});
        let err = HeapSnapshot::parse(&doc).err().expect("must fail");
        assert!(err.contains("edge_fields") || err.contains("`edges`"), "got: {err}");

        let doc = json!({
            "snapshot": {"meta": {"node_fields": ["type", "name"], "edge_fields": ["type"]}},
            "nodes": [0, 1], "edges": [], "strings": []
        });
        let err = HeapSnapshot::parse(&doc).err().expect("must fail");
        assert!(err.contains("missing the"), "got: {err}");
    }

    #[test]
    fn summary_groups_by_type_and_constructor() {
        let summary = fixture().summary();
        assert_eq!(summary.node_count, 5);
        assert_eq!(summary.reachable_count, 5);
        let objects = summary.by_type.iter().find(|g| g.name == "object").unwrap();
        assert_eq!(objects.count, 3);
        assert_eq!(objects.self_size, 250);
        // Payload appears twice as an object; the array once.
        let payload = summary
            .by_constructor
            .iter()
            .find(|g| g.name == "object")
            .or_else(|| summary.by_constructor.iter().find(|g| g.name == "Payload"))
            .expect("some payload group");
        assert!(payload.count >= 2);
    }

    #[test]
    fn retainers_and_paths_go_back_to_the_root() {
        let snapshot = fixture();
        let retainers = snapshot.retainers(3);
        assert!(retainers.iter().any(|(_, _, from)| *from == 2));
        let paths = snapshot.retaining_paths(3, 4);
        assert!(!paths.is_empty(), "expected a retaining path");
        let flat: Vec<String> = paths[0].iter().map(|(_, n)| snapshot.node_name(*n)).collect();
        assert!(flat.iter().any(|n| n == "leaky array"));
    }

    #[test]
    fn dominators_and_retained_sizes_are_consistent() {
        let snapshot = fixture();
        let dom = snapshot.dominators();
        // The two payloads are dominated by the array.
        assert_eq!(dom.get(&3).copied(), Some(2));
        assert_eq!(dom.get(&4).copied(), Some(2));
        let retained = snapshot.retained_sizes();
        assert_eq!(retained.get(&2).copied(), Some(80 + 100 + 100));
    }

    #[test]
    fn queries_filter_by_name_and_type() {
        let snapshot = fixture();
        let payloads = snapshot.query_nodes("payload", 10);
        assert_eq!(payloads.len(), 2);
        let arrays = snapshot.nodes_of_type("array", 10);
        assert_eq!(arrays, vec![2]);
    }

    #[test]
    fn diff_reports_growth() {
        let before = fixture();
        let mut doc_after = json!({
            "snapshot": {"meta": {
                "node_fields": ["type", "name", "id", "self_size", "edge_count"],
                "edge_fields": ["type", "name_or_index", "to_node"],
            }},
            "nodes": [9,0,1,0,1, 3,4,4,900,0],
            "edges": [EDGE_PROPERTY, 0, 5],
            "strings": ["(GC roots)", "", "", "", "Payload"],
        });
        doc_after["strings"] = json!(["(GC roots)", "", "", "", "Payload"]);
        let after = HeapSnapshot::parse(&doc_after).unwrap();
        let diff = before.diff_by_name(&after, 10);
        let payload = diff.iter().find(|d| d.name == "Payload").expect("payload diff");
        assert_eq!(payload.before, 200);
        assert_eq!(payload.after, 900);
        assert_eq!(payload.delta(), 700);
    }

    #[test]
    fn duplicate_strings_and_human_bytes() {
        let snapshot = fixture();
        // The fixture's strings live in the string table, not as string nodes,
        // so there is nothing to report; the call must still be safe.
        assert!(snapshot.duplicate_strings(10).is_empty());
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
    }
}