//! Accessibility-tree snapshots: rendering and element uid resolution.
//!
//! Lightpanda's `Accessibility.getFullAXTree` is the same source `chrome-devtools-mcp`
//! uses through Puppeteer. We render a compact, indented tree in which every
//! interactive or named node carries a short uid (`3_4`, `e12`, …). Tools such
//! as `click` and `fill` take that uid back, which the plugin resolves to a
//! `BackendNodeId` and then to a `RemoteObjectId` for DOM calls.
//!
//! The renderer is deliberately tolerant: unknown node shapes, ignored nodes,
//! and cycles in the parent/child links all degrade to "skip" rather than an
//! error, because the AX tree of a real page is never as tidy as a fixture.

use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Roles that are never interesting in a snapshot: they add noise without
/// telling the model anything actionable.
const NOISE_ROLES: &[&str] = &[
    "none",
    "presentation",
    "generic",
    "InlineTextBox",
    "LineBreak",
    "StaticText_Child",
];

/// Roles that own their own uid even when they have no accessible name.
const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "combobox",
    "listbox",
    "checkbox",
    "radio",
    "switch",
    "slider",
    "spinbutton",
    "tab",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "treeitem",
    "gridcell",
    "columnheader",
    "rowheader",
    "scrollbar",
    "textfield",
    "ColorWell",
    "date",
    "time",
    "file",
];

/// One resolved element in a snapshot.
#[derive(Debug, Clone)]
pub struct ElementRef {
    pub uid: String,
    pub backend_node_id: i64,
    pub role: String,
    pub name: String,
}

/// A rendered snapshot plus the uid → element mapping used by later tools.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub text: String,
    pub elements: Vec<ElementRef>,
    pub node_count: usize,
}

impl Snapshot {
    /// Look up an element by the uid shown in the rendered text.
    pub fn element(&self, uid: &str) -> Option<&ElementRef> {
        self.elements.iter().find(|e| e.uid == uid)
    }
}

fn ax_string(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value.get("value") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn is_ignored(node: &Value) -> bool {
    node.get("ignored")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// True when a node has properties that make it interactive or descriptive.
fn node_flags(node: &Value) -> (bool, bool, bool, bool, Option<String>) {
    let mut checked = false;
    let mut expanded = false;
    let mut selected = false;
    let mut disabled = false;
    let mut value: Option<String> = None;

    if let Some(props) = node.get("properties").and_then(|p| p.as_array()) {
        for prop in props {
            let name = prop.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let val = prop.get("value").and_then(|v| v.get("value"));
            match name {
                "checked" => {
                    checked = matches!(val, Some(Value::String(s)) if s == "true" || s == "mixed");
                }
                "expanded" => expanded = matches!(val, Some(Value::Bool(true))),
                "selected" => selected = matches!(val, Some(Value::Bool(true))),
                "disabled" => disabled = matches!(val, Some(Value::Bool(true))),
                "value" => {
                    if let Some(Value::String(s)) = val {
                        if !s.is_empty() {
                            value = Some(s.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    (checked, expanded, selected, disabled, value)
}

/// Render a full AX tree into the model-facing snapshot text.
///
/// `max_nodes` bounds the output; the remainder is summarised in a trailing
/// line so the model knows the snapshot was clipped rather than complete.
pub fn render_snapshot(nodes: &[Value], max_nodes: usize) -> Snapshot {
    let by_id: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|n| n.get("nodeId").and_then(|v| v.as_str()).map(|id| (id, n)))
        .collect();

    // Find the root: the node that no other node claims as a child. Chrome
    // always includes the root document, but a partial tree may not, so fall
    // back to the first node when the scan finds nothing.
    let mut child_ids: HashSet<&str> = HashSet::new();
    for node in nodes {
        if let Some(children) = node.get("childIds").and_then(|c| c.as_array()) {
            for child in children {
                if let Some(child) = child.as_str() {
                    child_ids.insert(child);
                }
            }
        }
    }
    let root_id = nodes
        .iter()
        .filter_map(|n| n.get("nodeId").and_then(|v| v.as_str()))
        .find(|id| !child_ids.contains(*id))
        .or_else(|| nodes.first().and_then(|n| n.get("nodeId")?.as_str()));

    let mut snapshot = Snapshot::default();
    let mut out = String::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut emitted = 0usize;
    let mut clipped = 0usize;

    if let Some(root_id) = root_id {
        let mut stack: Vec<(&str, usize)> = vec![(root_id, 0)];
        while let Some((node_id, depth)) = stack.pop() {
            if !seen.insert(node_id) {
                continue; // defensive: a malformed tree could cycle
            }
            let Some(node) = by_id.get(node_id) else {
                continue;
            };
            snapshot.node_count += 1;

            // Depth-first order with children pushed in reverse so the text
            // reads top-to-bottom as the DOM does.
            if let Some(children) = node.get("childIds").and_then(|c| c.as_array()) {
                for child in children.iter().rev() {
                    if let Some(child) = child.as_str() {
                        stack.push((child, depth + 1));
                    }
                }
            }

            if is_ignored(node) {
                continue;
            }
            let role = node
                .get("role")
                .map(|r| ax_string(Some(r)))
                .unwrap_or_default();
            let name = node
                .get("name")
                .map(|n| ax_string(Some(n)))
                .unwrap_or_default();
            if role.is_empty() && name.is_empty() {
                continue;
            }
            if NOISE_ROLES.contains(&role.as_str()) {
                continue;
            }
            // Only leaf-ish generic nodes are noise; containers still matter.
            let backend_node_id = node
                .get("backendDOMNodeId")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let interactive = INTERACTIVE_ROLES.contains(&role.as_str())
                || node
                    .get("properties")
                    .and_then(|p| p.as_array())
                    .map(|props| {
                        props
                            .iter()
                            .any(|p| p.get("name").and_then(|n| n.as_str()) == Some("focusable"))
                    })
                    .unwrap_or(false);

            if !interactive && name.is_empty() {
                continue;
            }
            if emitted >= max_nodes {
                clipped += 1;
                continue;
            }

            let (checked, expanded, selected, disabled, value) = node_flags(node);
            let uid = if backend_node_id > 0 && (interactive || !name.is_empty()) {
                format!("{}_{}", depth, emitted)
            } else {
                String::new()
            };
            if !uid.is_empty() {
                snapshot.elements.push(ElementRef {
                    uid: uid.clone(),
                    backend_node_id,
                    role: role.clone(),
                    name: name.clone(),
                });
            }

            let indent = "  ".repeat(depth);
            let mut line = format!("{indent}{role}");
            if !name.is_empty() {
                line.push_str(&format!(" \"{}\"", truncate(&name, 120)));
            }
            if !uid.is_empty() {
                line.push_str(&format!(" [uid={uid}]"));
            }
            if checked {
                line.push_str(" (checked)");
            }
            if selected {
                line.push_str(" (selected)");
            }
            if expanded {
                line.push_str(" (expanded)");
            }
            if disabled {
                line.push_str(" (disabled)");
            }
            if let Some(value) = value {
                line.push_str(&format!(" value=\"{}\"", truncate(&value, 80)));
            }
            out.push_str(&line);
            out.push('\n');
            emitted += 1;
        }
    }

    if clipped > 0 {
        out.push_str(&format!(
            "… {clipped} more nodes not shown (snapshot capped at {max_nodes}); \
             use a narrower query or read the DOM via evaluate_script\n"
        ));
    }
    if out.trim().is_empty() {
        out.push_str("(empty accessibility tree: the page has no accessible content)\n");
    }
    snapshot.text = out;
    snapshot
}

/// Collect ancestor/descendant DOM relationships for `get_css_styles`.
pub fn truncate(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i >= max {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, role: &str, name: &str, backend: i64, children: &[&str]) -> Value {
        let mut value = json!({
            "nodeId": id,
            "ignored": false,
            "role": {"type": "role", "value": role},
            "name": {"type": "computedString", "value": name},
            "childIds": children,
        });
        if backend > 0 {
            value["backendDOMNodeId"] = json!(backend);
        }
        value
    }

    #[test]
    fn renders_tree_with_uids_for_interactive_nodes() {
        let nodes = vec![
            node("1", "RootWebArea", "Example", 1, &["2", "3"]),
            node("2", "button", "Sign in", 42, &[]),
            node("3", "StaticText", "hello", 43, &[]),
        ];
        let snapshot = render_snapshot(&nodes, 100);
        assert!(snapshot.text.contains("RootWebArea \"Example\""));
        assert!(snapshot.text.contains("button \"Sign in\" [uid="));
        assert_eq!(snapshot.elements.len(), 3);
        let button = snapshot
            .elements
            .iter()
            .find(|e| e.role == "button")
            .expect("button captured");
        assert_eq!(button.backend_node_id, 42);
        assert!(snapshot.element(&button.uid).is_some());
    }

    #[test]
    fn skips_ignored_and_noise_nodes() {
        let mut ignored = node("2", "button", "Hidden", 42, &[]);
        ignored["ignored"] = json!(true);
        let nodes = vec![
            node("1", "RootWebArea", "Page", 1, &["2", "3"]),
            ignored,
            node("3", "none", "", 43, &[]),
        ];
        let snapshot = render_snapshot(&nodes, 100);
        assert!(!snapshot.text.contains("Hidden"));
        assert!(snapshot.elements.iter().all(|e| e.name != "Hidden"));
    }

    #[test]
    fn caps_output_and_reports_clipping() {
        let children: Vec<String> = (2..12).map(|i| i.to_string()).collect();
        let refs: Vec<&str> = children.iter().map(|s| s.as_str()).collect();
        let mut nodes = vec![node("1", "RootWebArea", "Page", 1, &refs)];
        for i in 2..12 {
            nodes.push(node(
                &i.to_string(),
                "button",
                &format!("b{i}"),
                i as i64,
                &[],
            ));
        }
        let snapshot = render_snapshot(&nodes, 3);
        assert!(snapshot.text.contains("more nodes not shown"));
        assert_eq!(snapshot.elements.len(), 3);
    }

    #[test]
    fn survives_cycles_and_unknown_children() {
        let mut a = node("1", "RootWebArea", "A", 1, &["2", "missing"]);
        a["childIds"] = json!(["2"]);
        let nodes = vec![a, node("2", "button", "B", 2, &["1"])];
        let snapshot = render_snapshot(&nodes, 100);
        assert!(snapshot.text.contains("button \"B\""));
        // Visited nodes are emitted once even with a cycle.
        assert_eq!(snapshot.text.matches("button \"B\"").count(), 1);
    }

    #[test]
    fn empty_tree_is_explicit() {
        let snapshot = render_snapshot(&[], 100);
        assert!(snapshot.text.contains("empty accessibility tree"));
        assert!(snapshot.elements.is_empty());
    }
}
