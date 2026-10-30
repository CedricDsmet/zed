use std::{collections::BTreeMap, ops::Range};

use collections::HashSet;
use file_icons::FileIcons;
use git::{repository::RepoPath, status::FileStatus};
use gpui::{
    AnyElement, ClickEvent, Context, EventEmitter, FocusHandle, Pixels, Stateful, Subscription,
    UniformListScrollHandle, actions, px, uniform_list,
};
use multi_buffer::PathKey;
use settings::{Settings, SettingsStore};
use smallvec::SmallVec;
use theme::ActiveTheme;
use ui::{IndentGuideColors, Tooltip, WithScrollbar, prelude::*};
use util::{ResultExt as _, paths::PathStyle};
use workspace::Workspace;

use crate::{git_panel_settings::GitPanelSettings, git_status_icon};

actions!(
    git,
    [
        /// Shows or hides the file tree next to branch and commit diffs.
        ToggleDiffFileTree,
    ]
);

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleDiffFileTree, _window, cx| {
        let is_visible = GitPanelSettings::get_global(cx).diff_file_tree;
        settings::update_settings_file(workspace.app_state().fs.clone(), cx, move |settings, _| {
            settings.git_panel.get_or_insert_default().diff_file_tree = Some(!is_visible);
        });
    });
}

pub(crate) fn render_toggle_button(focus_handle: &FocusHandle, cx: &App) -> IconButton {
    let is_visible = GitPanelSettings::get_global(cx).diff_file_tree;
    let focus_handle = focus_handle.clone();
    IconButton::new("toggle-diff-file-tree", IconName::ListTree)
        .icon_size(IconSize::Small)
        .toggle_state(is_visible)
        .tooltip(move |_window, cx| {
            let title = if is_visible {
                "Hide File Tree"
            } else {
                "Show File Tree"
            };
            Tooltip::for_action_in(title, &ToggleDiffFileTree, &focus_handle, cx)
        })
        .on_click(|_, window, cx| {
            window.dispatch_action(ToggleDiffFileTree.boxed_clone(), cx);
        })
}

const TREE_INDENT: f32 = 16.0;
const INDENT_GUIDE_LEFT_OFFSET: Pixels = px(19.);
const FILE_TREE_WIDTH: Pixels = px(250.);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiffFileTreeEntry {
    pub(crate) repo_path: RepoPath,
    pub(crate) status: FileStatus,
    pub(crate) path_key: PathKey,
}

pub(crate) enum DiffFileTreeEvent {
    OpenEntry(PathKey),
}

#[derive(Debug, PartialEq, Eq)]
enum Row {
    Directory {
        path: RepoPath,
        name: SharedString,
        depth: usize,
        expanded: bool,
    },
    File {
        entry_ix: usize,
        depth: usize,
    },
}

impl Row {
    fn depth(&self) -> usize {
        match self {
            Row::Directory { depth, .. } | Row::File { depth, .. } => *depth,
        }
    }
}

/// A read-only list of the files in a diff, shown next to the diff editor so
/// large diffs can be navigated by file. Follows the Git panel's tree view
/// setting so both lists look alike.
pub(crate) struct DiffFileTree {
    entries: Vec<DiffFileTreeEntry>,
    rows: Vec<Row>,
    collapsed_dirs: HashSet<RepoPath>,
    selected: Option<RepoPath>,
    tree_view: bool,
    path_style: PathStyle,
    scroll_handle: UniformListScrollHandle,
    _settings_subscription: Subscription,
}

impl EventEmitter<DiffFileTreeEvent> for DiffFileTree {}

impl DiffFileTree {
    pub(crate) fn new(path_style: PathStyle, cx: &mut Context<Self>) -> Self {
        let settings_subscription = cx.observe_global::<SettingsStore>(|this, cx| {
            let tree_view = GitPanelSettings::get_global(cx).tree_view;
            if tree_view != this.tree_view {
                this.tree_view = tree_view;
                this.rebuild_rows();
                cx.notify();
            }
        });
        Self {
            entries: Vec::new(),
            rows: Vec::new(),
            collapsed_dirs: HashSet::default(),
            selected: None,
            tree_view: GitPanelSettings::get_global(cx).tree_view,
            path_style,
            scroll_handle: UniformListScrollHandle::new(),
            _settings_subscription: settings_subscription,
        }
    }

    pub(crate) fn set_entries(
        &mut self,
        mut entries: Vec<DiffFileTreeEntry>,
        cx: &mut Context<Self>,
    ) {
        // The diff editor orders its excerpts by path key, so sorting the same
        // way keeps the list in the order the files appear in the editor.
        entries.sort_by(|a, b| a.path_key.cmp(&b.path_key));
        if entries == self.entries {
            return;
        }
        self.entries = entries;
        self.rebuild_rows();
        cx.notify();
    }

    pub(crate) fn file_count(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn entry_paths(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.repo_path.as_unix_str().to_string())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn open_path(&mut self, path: &str, cx: &mut Context<Self>) {
        let entry_ix = self
            .entries
            .iter()
            .position(|entry| entry.repo_path.as_unix_str() == path)
            .expect("path is in the file tree");
        self.open_entry(entry_ix, cx);
    }

    fn rebuild_rows(&mut self) {
        self.rows = build_rows(&self.entries, self.tree_view, &self.collapsed_dirs);
    }

    fn toggle_directory(&mut self, path: &RepoPath, cx: &mut Context<Self>) {
        if !self.collapsed_dirs.remove(path) {
            self.collapsed_dirs.insert(path.clone());
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn open_entry(&mut self, entry_ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(entry_ix) else {
            return;
        };
        self.selected = Some(entry.repo_path.clone());
        cx.emit(DiffFileTreeEvent::OpenEntry(entry.path_key.clone()));
        cx.notify();
    }

    fn visible_depths(&self, range: Range<usize>) -> SmallVec<[usize; 64]> {
        range
            .map(|ix| self.rows.get(ix).map_or(0, Row::depth))
            .collect()
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let element = match self.rows.get(ix)? {
            Row::Directory {
                path,
                name,
                depth,
                expanded,
            } => {
                let path = path.clone();
                let (chevron, folder) = if *expanded {
                    (IconName::ChevronDown, IconName::FolderOpen)
                } else {
                    (IconName::ChevronRight, IconName::Folder)
                };
                self.render_row_container(ix, false, cx)
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1()
                            .pl(px(*depth as f32 * TREE_INDENT))
                            .child(
                                h_flex()
                                    .flex_none()
                                    .gap_0p5()
                                    .child(
                                        Icon::new(chevron)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Icon::new(folder)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                Label::new(name.clone())
                                    .single_line()
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.toggle_directory(&path, cx);
                    }))
            }
            Row::File { entry_ix, depth } => {
                let entry_ix = *entry_ix;
                let entry = self.entries.get(entry_ix)?;
                let is_selected = self.selected.as_ref() == Some(&entry.repo_path);
                let is_deleted = entry.status.is_deleted();
                let full_path: SharedString =
                    entry.repo_path.display(self.path_style).to_string().into();
                let file_name = entry
                    .repo_path
                    .file_name()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| full_path.to_string());
                let parent_dir = entry
                    .repo_path
                    .parent()
                    .filter(|parent| !parent.is_empty())
                    .map(|parent| parent.display(self.path_style).to_string());
                let file_icon = FileIcons::get_icon(entry.repo_path.as_std_path(), cx)
                    .map(Icon::from_path)
                    .unwrap_or_else(|| Icon::new(IconName::File))
                    .size(IconSize::Small)
                    .color(Color::Muted);

                self.render_row_container(ix, is_selected, cx)
                    .child(
                        h_flex()
                            .min_w_0()
                            .flex_1()
                            .gap_1()
                            .pl(px(*depth as f32 * TREE_INDENT))
                            .when(self.tree_view, |this| {
                                // Leaves room for the chevron so file names line up
                                // with sibling directory names.
                                this.child(
                                    div().flex_none().size(IconSize::Small.rems()).invisible(),
                                )
                            })
                            .child(file_icon)
                            .child(git_status_icon(entry.status))
                            .child(
                                div().flex_none().child(
                                    Label::new(file_name)
                                        .single_line()
                                        .when(is_deleted, Label::strikethrough),
                                ),
                            )
                            .when(!self.tree_view, |this| {
                                this.when_some(parent_dir, |this, parent_dir| {
                                    this.child(
                                        Label::new(parent_dir)
                                            .single_line()
                                            .color(Color::Muted)
                                            .truncate_start(),
                                    )
                                })
                            }),
                    )
                    .tooltip(Tooltip::text(full_path))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.open_entry(entry_ix, cx);
                    }))
            }
        };
        Some(element.into_any_element())
    }

    fn render_row_container(&self, ix: usize, is_selected: bool, cx: &App) -> Stateful<Div> {
        let colors = cx.theme().colors();
        h_flex()
            .id(("diff-file-tree-row", ix))
            .h(rems(1.75))
            .w_full()
            .min_w_0()
            .pl_2p5()
            .pr_1()
            .cursor_pointer()
            .when(is_selected, |this| this.bg(colors.ghost_element_selected))
            .hover(|style| style.bg(colors.ghost_element_hover))
            .active(|style| style.bg(colors.ghost_element_active))
    }
}

impl Render for DiffFileTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let file_count = self.entries.len();
        let header = if file_count == 1 {
            "1 file changed".to_string()
        } else {
            format!("{file_count} files changed")
        };

        v_flex()
            .flex_none()
            .h_full()
            .w(FILE_TREE_WIDTH)
            .border_r_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .flex_none()
                    .h(rems(1.75))
                    .px_2p5()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Label::new(header)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "diff-file-tree",
                            self.rows.len(),
                            cx.processor(|this, range: Range<usize>, _window, cx| {
                                range
                                    .filter_map(|ix| this.render_row(ix, cx))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .when(self.tree_view, |list| {
                            list.with_decoration(
                                ui::indent_guides(px(TREE_INDENT), IndentGuideColors::panel(cx))
                                    .with_left_offset(INDENT_GUIDE_LEFT_OFFSET)
                                    .with_compute_indents_fn(
                                        cx.entity(),
                                        |this, range, _window, _cx| this.visible_depths(range),
                                    ),
                            )
                        })
                        .size_full()
                        .track_scroll(&self.scroll_handle),
                    )
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx),
            )
    }
}

#[derive(Default)]
struct TreeNode<'a> {
    children: BTreeMap<&'a str, TreeNode<'a>>,
    files: Vec<usize>,
}

fn build_rows(
    entries: &[DiffFileTreeEntry],
    tree_view: bool,
    collapsed_dirs: &HashSet<RepoPath>,
) -> Vec<Row> {
    if !tree_view {
        return (0..entries.len())
            .map(|entry_ix| Row::File { entry_ix, depth: 0 })
            .collect();
    }

    let mut root = TreeNode::default();
    for (entry_ix, entry) in entries.iter().enumerate() {
        let mut node = &mut root;
        if let Some(parent) = entry.repo_path.parent() {
            for component in parent.components() {
                node = node.children.entry(component).or_default();
            }
        }
        node.files.push(entry_ix);
    }

    let mut rows = Vec::new();
    push_rows(&root, "", 0, collapsed_dirs, &mut rows);
    rows
}

fn push_rows(
    node: &TreeNode<'_>,
    parent_path: &str,
    depth: usize,
    collapsed_dirs: &HashSet<RepoPath>,
    rows: &mut Vec<Row>,
) {
    for (name, mut child) in &node.children {
        // Directories holding nothing but a single subdirectory are shown as
        // one row ("a/b/c"), like the Git panel does.
        let mut name = name.to_string();
        while child.files.is_empty() && child.children.len() == 1 {
            let Some((grandchild_name, grandchild)) = child.children.iter().next() else {
                break;
            };
            name.push('/');
            name.push_str(grandchild_name);
            child = grandchild;
        }

        let path = if parent_path.is_empty() {
            name.clone()
        } else {
            format!("{parent_path}/{name}")
        };
        let Some(repo_path) = RepoPath::new(&path).log_err() else {
            continue;
        };
        let expanded = !collapsed_dirs.contains(&repo_path);
        rows.push(Row::Directory {
            path: repo_path,
            name: name.into(),
            depth,
            expanded,
        });
        if expanded {
            push_rows(child, &path, depth + 1, collapsed_dirs, rows);
        }
    }

    for &entry_ix in &node.files {
        rows.push(Row::File { entry_ix, depth });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::status::{StatusCode, TrackedStatus};

    fn entry(path: &str) -> DiffFileTreeEntry {
        let repo_path = RepoPath::new(path).unwrap();
        DiffFileTreeEntry {
            path_key: PathKey::with_sort_prefix(0, repo_path.into_arc()),
            repo_path,
            status: FileStatus::Tracked(TrackedStatus {
                index_status: StatusCode::Modified,
                worktree_status: StatusCode::Unmodified,
            }),
        }
    }

    fn describe(entries: &[DiffFileTreeEntry], rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Directory {
                    name,
                    depth,
                    expanded,
                    ..
                } => format!(
                    "{}{} {name}/",
                    "  ".repeat(*depth),
                    if *expanded { "v" } else { ">" }
                ),
                Row::File { entry_ix, depth } => format!(
                    "{}{}",
                    "  ".repeat(*depth),
                    entries[*entry_ix].repo_path.file_name().unwrap()
                ),
            })
            .collect()
    }

    #[test]
    fn test_build_rows_tree() {
        let entries = vec![
            entry("README.md"),
            entry("crates/editor/src/editor.rs"),
            entry("crates/editor/src/element.rs"),
            entry("crates/git/src/git.rs"),
            entry("crates/git/Cargo.toml"),
        ];

        let rows = build_rows(&entries, true, &HashSet::default());
        assert_eq!(
            describe(&entries, &rows),
            vec![
                "v crates/",
                "  v editor/src/",
                "    editor.rs",
                "    element.rs",
                "  v git/",
                "    v src/",
                "      git.rs",
                "    Cargo.toml",
                "README.md",
            ]
        );
    }

    #[test]
    fn test_build_rows_collapsed_directory_hides_children() {
        let entries = vec![
            entry("crates/editor/src/editor.rs"),
            entry("crates/git/src/git.rs"),
            entry("README.md"),
        ];
        let mut collapsed = HashSet::default();
        collapsed.insert(RepoPath::new("crates/editor/src").unwrap());

        let rows = build_rows(&entries, true, &collapsed);
        assert_eq!(
            describe(&entries, &rows),
            vec![
                "v crates/",
                "  > editor/src/",
                "  v git/src/",
                "    git.rs",
                "README.md",
            ]
        );
    }

    #[test]
    fn test_build_rows_flat() {
        let entries = vec![entry("a/b.rs"), entry("c.rs")];
        let rows = build_rows(&entries, false, &HashSet::default());
        assert_eq!(describe(&entries, &rows), vec!["b.rs", "c.rs"]);
    }
}
