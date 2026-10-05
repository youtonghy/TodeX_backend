//! The accessibility tree as agents read it: indented lines like the
//! browser snapshot, `- button "Save" [ref=e3]`, where only actionable
//! elements get a ref. Kept free of platform types so it is testable.

use std::collections::HashMap;

/// Size limit of the formatted tree.
pub(crate) const MAX_CHARS: usize = 64 * 1024;

/// One element as read from the platform accessibility API.
#[derive(Clone, Debug, Default)]
pub(crate) struct Node {
    /// xa11y role in snake case (`button`, `text_area`, ...).
    pub role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub enabled: bool,
    pub focused: bool,
    pub selected: bool,
    /// A password field: its value is never printed.
    pub secure: bool,
    /// The platform reports actions (press, set_value, ...) for it.
    pub actionable: bool,
    /// Index into the caller's element table, used for refs.
    pub handle: usize,
    pub children: Vec<Node>,
}

pub(crate) struct Formatted {
    pub text: String,
    /// `eN` → element handle.
    pub refs: HashMap<String, usize>,
    pub truncated: bool,
}

/// Roles an agent can act on even when the platform lists no action.
const ACTIONABLE: &[&str] = &[
    "button",
    "check_box",
    "radio_button",
    "text_field",
    "text_area",
    "combo_box",
    "menu_item",
    "tab",
    "list_item",
    "table_row",
    "table_cell",
    "tree_item",
    "slider",
    "spin_button",
    "switch",
    "link",
];

/// Structural roles printed only through their children when unnamed.
const TRANSPARENT: &[&str] = &[
    "group",
    "unknown",
    "split_group",
    "scroll_bar",
    "scroll_thumb",
];

fn clean(value: Option<&str>) -> Option<String> {
    let collapsed = value?.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(if collapsed.chars().count() > 200 {
        collapsed.chars().take(200).collect::<String>() + "…"
    } else {
        collapsed
    })
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(crate) fn format(root: &Node) -> Formatted {
    let mut out = Formatted {
        text: String::new(),
        refs: HashMap::new(),
        truncated: false,
    };
    walk(root, 0, &mut out);
    if out.text.ends_with('\n') {
        out.text.pop();
    }
    out
}

fn walk(node: &Node, depth: usize, out: &mut Formatted) {
    if out.truncated {
        return;
    }
    let name = clean(node.name.as_deref());
    let value = if node.secure {
        None
    } else {
        clean(node.value.as_deref())
    };
    let printed = !(TRANSPARENT.contains(&node.role.as_str()) && name.is_none())
        && !(node.role == "static_text" && value.is_none() && name.is_none());
    if printed {
        let mut line = format!("{}- {}", "  ".repeat(depth), node.role);
        if node.secure {
            line.push_str(" (password)");
        }
        if node.role == "static_text" {
            if let Some(text) = value.as_ref().or(name.as_ref()) {
                line.push(' ');
                line.push_str(&quote(text));
            }
        } else {
            if let Some(name) = &name {
                line.push(' ');
                line.push_str(&quote(name));
            }
            if let Some(value) = value.as_ref().filter(|value| Some(*value) != name.as_ref()) {
                line.push_str(" value=");
                line.push_str(&quote(value));
            }
        }
        if !node.enabled {
            line.push_str(" [disabled]");
        }
        if node.focused {
            line.push_str(" [focused]");
        }
        if node.selected {
            line.push_str(" [selected]");
        }
        if node.secure || node.actionable || ACTIONABLE.contains(&node.role.as_str()) {
            let reference = format!("e{}", out.refs.len() + 1);
            line.push_str(&format!(" [ref={reference}]"));
            out.refs.insert(reference, node.handle);
        }
        if out.text.len() + line.len() + 1 > MAX_CHARS {
            out.truncated = true;
            return;
        }
        out.text.push_str(&line);
        out.text.push('\n');
    }
    let depth = if printed { depth + 1 } else { depth };
    for child in &node.children {
        walk(child, depth, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, name: Option<&str>, handle: usize, children: Vec<Node>) -> Node {
        Node {
            role: role.to_owned(),
            name: name.map(str::to_owned),
            enabled: true,
            handle,
            children,
            ..Node::default()
        }
    }

    #[test]
    fn tree_lists_refs_hides_secrets_and_skips_unnamed_structure() {
        let mut secret = node("text_field", Some("Password"), 4, vec![]);
        secret.secure = true;
        secret.value = Some("hunter2".into());
        let mut label = node("static_text", None, 5, vec![]);
        label.value = Some("  Hello\n world ".into());
        let mut disabled = node("button", Some("Save"), 3, vec![]);
        disabled.enabled = false;
        let root = node(
            "window",
            Some("Notes"),
            0,
            vec![node(
                "group",
                None,
                1,
                vec![
                    disabled,
                    secret,
                    label,
                    node("static_text", None, 6, vec![]),
                    node("image", Some("logo"), 7, vec![]),
                ],
            )],
        );
        let formatted = format(&root);
        assert_eq!(
            formatted.text,
            "- window \"Notes\"\n  - button \"Save\" [disabled] [ref=e1]\n  - text_field (password) \"Password\" [ref=e2]\n  - static_text \"Hello world\"\n  - image \"logo\""
        );
        assert_eq!(formatted.refs["e1"], 3);
        assert_eq!(formatted.refs["e2"], 4);
        assert!(!formatted.text.contains("hunter2"));
        assert!(!formatted.truncated);
    }

    #[test]
    fn large_trees_are_truncated() {
        let children = (0..5000)
            .map(|index| node("button", Some(&"x".repeat(40)), index, vec![]))
            .collect();
        let formatted = format(&node("window", None, 0, children));
        assert!(formatted.truncated);
        assert!(formatted.text.len() <= MAX_CHARS);
    }
}
