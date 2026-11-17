//! Preset menu tree shared by the shell extension, the X11 popup and the tray.
//!
//! `Preset::group` is a path: names separated by ` / ` (a bare `/` works too).
//! One definition here keeps the three menus from drifting apart:
//!
//! - segments are trimmed and empty segments dropped, so `None`, `""` and
//!   `" / "` all mean "ungrouped";
//! - groups keep first-appearance order and are merged by their trimmed name
//!   (case-sensitive: `16:9` and ` 16:9 ` merge, while `16:9` and `16:9:` do not);
//! - a group holding a single preset is still a group, never inlined;
//! - presets keep their configured order inside their group;
//! - nesting is unlimited: one level per path segment.

use crate::Preset;

/// One row of a preset menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuNode {
    /// A preset; `index` points into `Config::presets`.
    Preset { index: usize, label: String },
    /// A horizontal line; not selectable and not activatable.
    Separator { index: usize },
    /// A submenu. Empty groups never exist because they would hold no preset.
    Group {
        label: String,
        children: Vec<MenuNode>,
    },
}

/// Build the menu for `presets`.
pub fn menu(presets: &[Preset]) -> Vec<MenuNode> {
    let mut nodes = Vec::new();
    for (index, preset) in presets.iter().enumerate() {
        let path = group_path(preset.group.as_deref());
        let node = if preset.is_separator() {
            MenuNode::Separator { index }
        } else {
            MenuNode::Preset {
                index,
                label: preset.label.trim().to_string(),
            }
        };
        insert(&mut nodes, &path, node);
    }
    nodes
}

/// Labels of every preset in depth-first menu order.
///
/// This is the order `ActivateMenuRow` indexes and the order the daemon expects
/// the shell to report back.
pub fn preset_labels(nodes: &[MenuNode]) -> Vec<String> {
    let mut labels = Vec::new();
    collect_labels(nodes, &mut labels);
    labels
}

fn collect_labels(nodes: &[MenuNode], labels: &mut Vec<String>) {
    for node in nodes {
        match node {
            MenuNode::Preset { label, .. } => labels.push(label.clone()),
            MenuNode::Group { children, .. } => collect_labels(children, labels),
            // A separator is not activatable, so it has no row in the label list.
            MenuNode::Separator { .. } => {}
        }
    }
}

/// Split a group path into its trimmed, non-empty segments.
pub fn group_path(group: Option<&str>) -> Vec<String> {
    group
        .unwrap_or_default()
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

/// Canonical spelling of a group path, or `None` when it has no segments.
///
/// Writing through this keeps `" / "`, `"A//B"` and `"A / B"` from creating
/// configurations that look different but render as the same menu.
pub fn normalize_group(group: Option<&str>) -> Option<String> {
    let path = group_path(group);
    if path.is_empty() {
        None
    } else {
        Some(path.join(" / "))
    }
}

fn insert(nodes: &mut Vec<MenuNode>, path: &[String], node: MenuNode) {
    let Some(name) = path.first() else {
        nodes.push(node);
        return;
    };
    let position = match nodes
        .iter()
        .position(|node| matches!(node, MenuNode::Group { label, .. } if label == name))
    {
        Some(position) => position,
        None => {
            nodes.push(MenuNode::Group {
                label: name.clone(),
                children: Vec::new(),
            });
            nodes.len() - 1
        }
    };
    if let MenuNode::Group { children, .. } = &mut nodes[position] {
        insert(children, &path[1..], node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Preset};

    fn preset(label: &str, group: Option<&str>) -> Preset {
        Preset {
            label: label.to_string(),
            size: "800x600".to_string(),
            group: group.map(str::to_string),
            anchor: None,
            hotkey: None,
            monitor: None,
            x: None,
            y: None,
            allow_outside_bounds: None,
            kind: crate::PresetKind::Resize,
        }
    }

    fn separator(group: Option<&str>) -> Preset {
        Preset::separator(group.map(str::to_string))
    }

    #[test]
    fn separators_keep_their_configured_position() {
        let nodes = menu(
            &config(vec![
                preset("a", None),
                separator(None),
                preset("b", Some("G")),
                separator(Some("G")),
            ])
            .presets,
        );
        assert_eq!(
            nodes,
            vec![
                MenuNode::Preset {
                    index: 0,
                    label: "a".to_string()
                },
                MenuNode::Separator { index: 1 },
                MenuNode::Group {
                    label: "G".to_string(),
                    children: vec![
                        MenuNode::Preset {
                            index: 2,
                            label: "b".to_string()
                        },
                        MenuNode::Separator { index: 3 },
                    ],
                },
            ]
        );
        // Separators never appear in the labels used by ActivateMenuRow.
        assert_eq!(preset_labels(&nodes), vec!["a", "b"]);
    }

    fn config(presets: Vec<Preset>) -> Config {
        Config {
            presets,
            ..Config::default()
        }
    }

    #[test]
    fn ungrouped_presets_stay_on_the_top_level() {
        let nodes = menu(&config(vec![preset("a", None), preset("b", Some("  "))]).presets);
        assert_eq!(
            nodes,
            vec![
                MenuNode::Preset {
                    index: 0,
                    label: "a".to_string()
                },
                MenuNode::Preset {
                    index: 1,
                    label: "b".to_string()
                },
            ]
        );
    }

    #[test]
    fn group_paths_are_trimmed_and_empty_segments_dropped() {
        assert_eq!(group_path(Some("  A /  B  ")), vec!["A", "B"]);
        assert_eq!(group_path(Some("A//B")), vec!["A", "B"]);
        assert_eq!(group_path(Some(" / ")), Vec::<String>::new());
        assert_eq!(group_path(None), Vec::<String>::new());
    }

    #[test]
    fn groups_merge_and_keep_first_appearance_order() {
        let nodes = menu(
            &config(vec![
                preset("wide", Some("16:9")),
                preset("full", None),
                preset("hd", Some("16:9")),
                preset("square", Some("4:3")),
            ])
            .presets,
        );
        assert_eq!(
            nodes,
            vec![
                MenuNode::Group {
                    label: "16:9".to_string(),
                    children: vec![
                        MenuNode::Preset {
                            index: 0,
                            label: "wide".to_string()
                        },
                        MenuNode::Preset {
                            index: 2,
                            label: "hd".to_string()
                        },
                    ],
                },
                MenuNode::Preset {
                    index: 1,
                    label: "full".to_string()
                },
                MenuNode::Group {
                    label: "4:3".to_string(),
                    children: vec![MenuNode::Preset {
                        index: 3,
                        label: "square".to_string()
                    }],
                },
            ]
        );
    }

    #[test]
    fn a_single_preset_group_is_still_a_group() {
        let nodes = menu(&config(vec![preset("only", Some("Solo"))]).presets);
        assert_eq!(
            nodes,
            vec![MenuNode::Group {
                label: "Solo".to_string(),
                children: vec![MenuNode::Preset {
                    index: 0,
                    label: "only".to_string()
                }],
            }]
        );
    }

    #[test]
    fn nested_paths_become_nested_submenus() {
        let nodes = menu(
            &config(vec![
                preset("a", Some("16:9 / UHD")),
                preset("b", Some("16:9 / HD")),
                preset("c", Some("16:9 / UHD / ultrawide")),
            ])
            .presets,
        );
        assert_eq!(
            nodes,
            vec![MenuNode::Group {
                label: "16:9".to_string(),
                children: vec![
                    MenuNode::Group {
                        label: "UHD".to_string(),
                        children: vec![
                            MenuNode::Preset {
                                index: 0,
                                label: "a".to_string()
                            },
                            // Three path segments: one more submenu level.
                            MenuNode::Group {
                                label: "ultrawide".to_string(),
                                children: vec![MenuNode::Preset {
                                    index: 2,
                                    label: "c".to_string()
                                }],
                            },
                        ],
                    },
                    MenuNode::Group {
                        label: "HD".to_string(),
                        children: vec![MenuNode::Preset {
                            index: 1,
                            label: "b".to_string()
                        }],
                    },
                ],
            }]
        );
    }

    #[test]
    fn preset_labels_follow_depth_first_menu_order() {
        let nodes = menu(
            &config(vec![
                preset("a", Some("16:9")),
                preset("full", None),
                preset("b", Some("16:9")),
            ])
            .presets,
        );
        assert_eq!(
            preset_labels(&nodes),
            vec!["a".to_string(), "b".to_string(), "full".to_string()]
        );
    }
}
