//! The page's accessibility tree as agents read it (CDP
//! `Accessibility.getFullAXTree`): indented lines where interactive nodes
//! carry `[ref=eN]`, mapped to DOM backend node ids until the next snapshot.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

pub(crate) const MAX_TREE_CHARS: usize = 64 * 1024;

/// Roles an agent can act on; they get `[ref=eN]`.
const INTERACTIVE: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "option",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "switch",
    "slider",
    "spinbutton",
    "treeitem",
];
/// Structural roles printed only through their children.
const TRANSPARENT: &[&str] = &[
    "none",
    "generic",
    "InlineTextBox",
    "LineBreak",
    "RootWebArea",
    "presentation",
];
const STATES: &[&str] = &[
    "focused", "checked", "disabled", "expanded", "selected", "pressed", "required",
];

pub(crate) struct Formatted {
    pub tree: String,
    pub refs: HashMap<String, i64>,
    pub truncated: bool,
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

fn quote(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = collapsed.chars().take(200).collect();
    serde_json::to_string(&short).unwrap_or_default()
}

pub(crate) fn format(nodes: &[Value]) -> Formatted {
    let by_id: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|node| node["nodeId"].as_str().map(|id| (id, node)))
        .collect();
    let root = nodes.iter().find(|node| {
        node["parentId"]
            .as_str()
            .is_none_or(|parent| !by_id.contains_key(parent))
    });
    let mut out = Formatted {
        tree: String::new(),
        refs: HashMap::new(),
        truncated: false,
    };
    let mut visited = HashSet::new();
    if let Some(root) = root {
        walk(root, 0, ("", ""), &by_id, &mut visited, &mut out);
    }
    if out.tree.ends_with('\n') {
        out.tree.pop();
    }
    out
}

fn walk<'a>(
    node: &'a Value,
    depth: usize,
    parent: (&str, &str),
    by_id: &HashMap<&str, &'a Value>,
    visited: &mut HashSet<&'a str>,
    out: &mut Formatted,
) {
    let Some(id) = node["nodeId"].as_str() else {
        return;
    };
    if out.truncated || !visited.insert(id) {
        return;
    }
    let role = text(&node["role"]["value"]);
    let name = text(&node["name"]["value"]).trim().to_owned();
    let value = text(&node["value"]["value"]);
    // Static text repeating its parent's name or value adds nothing.
    let printed = node["ignored"].as_bool() != Some(true)
        && !role.is_empty()
        && !(TRANSPARENT.contains(&role.as_str()) && name.is_empty())
        && !(role == "StaticText" && (name.is_empty() || name == parent.0 || name == parent.1));
    if printed {
        let mut line = format!(
            "{}- {}",
            "  ".repeat(depth),
            if role == "StaticText" { "text" } else { &role }
        );
        if !name.is_empty() {
            line.push(' ');
            line.push_str(&quote(&name));
        }
        if !value.is_empty() && value != name {
            line.push_str(" value=");
            line.push_str(&quote(&value));
        }
        for property in node["properties"].as_array().into_iter().flatten() {
            let Some(property_name) = property["name"].as_str() else {
                continue;
            };
            let property_value = &property["value"]["value"];
            if !STATES.contains(&property_name) || is_falsy(property_value) {
                continue;
            }
            if property_value == &Value::Bool(true) {
                line.push_str(&format!(" [{property_name}]"));
            } else {
                line.push_str(&format!(" [{property_name}={}]", text(property_value)));
            }
        }
        if INTERACTIVE.contains(&role.as_str()) {
            if let Some(backend) = node["backendDOMNodeId"].as_i64() {
                let reference = format!("e{}", out.refs.len() + 1);
                line.push_str(&format!(" [ref={reference}]"));
                out.refs.insert(reference, backend);
            }
        }
        if out.tree.len() + line.len() + 1 > MAX_TREE_CHARS {
            out.truncated = true;
            return;
        }
        out.tree.push_str(&line);
        out.tree.push('\n');
    }
    if role == "StaticText" {
        return;
    }
    let next_parent = if printed {
        (name.as_str(), value.as_str())
    } else {
        parent
    };
    for child in node["childIds"].as_array().into_iter().flatten() {
        if let Some(child) = child.as_str().and_then(|child| by_id.get(child)) {
            walk(
                child,
                if printed { depth + 1 } else { depth },
                next_parent,
                by_id,
                visited,
                out,
            );
        }
    }
}

fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::String(text) => text.is_empty(),
        Value::Number(number) => number.as_f64() == Some(0.0),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn node(id: &str, role: &str, name: &str, children: &[&str], extra: Value) -> Value {
        let mut node = json!({
            "nodeId": id,
            "role": { "value": role },
            "name": { "value": name },
            "childIds": children,
        });
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                node[key] = value;
            }
        }
        node
    }

    #[test]
    fn interactive_nodes_get_refs_and_structure_collapses() {
        let nodes = vec![
            node("1", "RootWebArea", "Dev", &["2"], json!({})),
            node(
                "2",
                "generic",
                "",
                &["3", "5", "6"],
                json!({ "parentId": "1" }),
            ),
            node(
                "3",
                "button",
                "Sign in",
                &["4"],
                json!({ "parentId": "2", "backendDOMNodeId": 31 }),
            ),
            node(
                "4",
                "StaticText",
                "Sign in",
                &[],
                json!({ "parentId": "3" }),
            ),
            node(
                "5",
                "textbox",
                "Email",
                &[],
                json!({
                    "parentId": "2", "backendDOMNodeId": 32, "value": { "value": "a@b.c" },
                    "properties": [{ "name": "focused", "value": { "value": true } }, { "name": "required", "value": { "value": false } }]
                }),
            ),
            node("6", "StaticText", "Hello", &[], json!({ "parentId": "2" })),
        ];
        let formatted = format(&nodes);
        assert_eq!(
            formatted.tree,
            "- RootWebArea \"Dev\"\n  - button \"Sign in\" [ref=e1]\n  - textbox \"Email\" value=\"a@b.c\" [focused] [ref=e2]\n  - text \"Hello\""
        );
        assert_eq!(formatted.refs["e1"], 31);
        assert_eq!(formatted.refs["e2"], 32);
        assert!(!formatted.truncated);
    }

    #[test]
    fn large_trees_truncate_and_cycles_terminate() {
        let mut nodes = vec![node(
            "root",
            "RootWebArea",
            "",
            &(0..3000)
                .map(|i| format!("n{i}"))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            json!({}),
        )];
        for i in 0..3000 {
            nodes.push(node(
                &format!("n{i}"),
                "link",
                &"x".repeat(40),
                &["root"],
                json!({ "parentId": "root", "backendDOMNodeId": i }),
            ));
        }
        let formatted = format(&nodes);
        assert!(formatted.truncated);
        assert!(formatted.tree.len() <= MAX_TREE_CHARS);
    }
}
