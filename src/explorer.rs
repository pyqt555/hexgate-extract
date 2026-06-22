use std::collections::{HashMap, HashSet};
use crate::FolderTreeNode;

#[derive(Clone, Debug)]
pub struct ExplorerNode {
    pub id: u64,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
    pub size: u64,
    pub tag_bitmask: u64,
    pub parent_id: u64,
    pub child_ids: Vec<u64>, // Direct children IDs

    // Active UI states
    pub is_expanded: bool,
    pub is_selected: bool,
    pub is_visible: bool,
}

/// Recursively flattens the nested FolderTreeNode into a flat DFS order vector
pub fn flatten_explorer_tree(
    node: &FolderTreeNode,
    depth: usize,
    parent_id: u64,
    flat_list: &mut Vec<ExplorerNode>,
) {
    let mut child_ids = Vec::new();
    for child in &node.children {
        child_ids.push(child.id);
    }

    flat_list.push(ExplorerNode {
        id: node.id,
        name: node.name.clone(),
        is_dir: node.is_dir,
        depth,
        size: node.size,
        tag_bitmask: node.tag_bitmask,
        parent_id,
        child_ids,
        is_expanded: false,
        is_selected: false,
        is_visible: true,
    });

    for child in &node.children {
        flatten_explorer_tree(child, depth + 1, node.id, flat_list);
    }
}

/// Dynamic visibility updater (Supports standard/regex searches, tag filters, none/neutral pill, and parent collapse rules)
pub fn update_explorer_visibility(
    nodes: &mut [ExplorerNode],
    query: &str,
    is_regex: bool,
    selected_tags: &HashSet<u8>,
    select_none_tag: bool,
) {
    let mut id_to_idx = HashMap::new();
    for (idx, node) in nodes.iter().enumerate() {
        id_to_idx.insert(node.id, idx);
    }

    let mut matches_filter = vec![false; nodes.len()];

    let regex_opt = if is_regex && !query.is_empty() {
        regex::Regex::new(&format!("(?i){}", query)).ok()
    } else {
        None
    };
    let query_lower = query.to_lowercase();

    // 1. Tag & text search matching on individual files
    for i in 0..nodes.len() {
        let node = &nodes[i];
        if node.is_dir {
            continue; // Folder visibility is derived recursively from child files
        }

        // Tag matching (including "none" tag for untagged global files)
        let tag_match = if selected_tags.is_empty() && !select_none_tag {
            true // No filters active
        } else {
            let mask = node.tag_bitmask;
            if mask == 0 {
                select_none_tag
            } else {
                selected_tags.iter().any(|&tag_id| {
                    let bit = tag_id.saturating_sub(1);
                    (mask & (1 << bit)) != 0
                })
            }
        };

        if !tag_match {
            continue;
        }

        // Text query matching
        let text_match = if query.is_empty() {
            true
        } else if let Some(ref re) = regex_opt {
            re.is_match(&node.name)
        } else {
            node.name.to_lowercase().contains(&query_lower)
        };

        if text_match {
            matches_filter[i] = true;

            // Recurse up and force parent directories to stay visible
            let mut curr_parent_id = node.parent_id;
            while curr_parent_id != 0 {
                if let Some(&p_idx) = id_to_idx.get(&curr_parent_id) {
                    matches_filter[p_idx] = true;
                    curr_parent_id = nodes[p_idx].parent_id;
                } else {
                    break;
                }
            }
        }
    }

    let is_filtering = !query.is_empty() || !selected_tags.is_empty() || select_none_tag;

    // 2. Set actual row visibility based on parent collapse rules
    for i in 0..nodes.len() {
        let node = &nodes[i];

        let matches = if is_filtering {
            matches_filter[i]
        } else {
            true
        };

        if !matches {
            nodes[i].is_visible = false;
            continue;
        }

        // Verify parent expansion (collapses are ignored when search queries are active)
        let mut parent_expanded = true;
        let mut curr_parent_id = node.parent_id;
        while curr_parent_id != 0 {
            if let Some(&p_idx) = id_to_idx.get(&curr_parent_id) {
                if !nodes[p_idx].is_expanded && !is_filtering {
                    parent_expanded = false;
                    break;
                }
                curr_parent_id = nodes[p_idx].parent_id;
            } else {
                break;
            }
        }

        nodes[i].is_visible = parent_expanded;
    }
}

/// Recursively selects all visible children of a given folder row (DFS order optimization)
pub fn select_visible_folder_members(nodes: &mut [ExplorerNode], dir_id: u64, select: bool) {
    let mut targets = HashSet::new();
    targets.insert(dir_id);

    // Because the list is flattened in DFS order, children sit sequentially under their parents.
    // This allows us to complete selection recursively in a single, allocation-free pass.
    for node in nodes.iter_mut() {
        if targets.contains(&node.parent_id) && node.is_visible {
            targets.insert(node.id);
            node.is_selected = select;
        }
        if node.id == dir_id {
            node.is_selected = select;
        }
    }
}

/// Selects/Deselects all currently visible manifest rows
pub fn select_all_visible_rows(nodes: &mut [ExplorerNode], select: bool) {
    for node in nodes.iter_mut() {
        if node.is_visible {
            node.is_selected = select;
        }
    }
}