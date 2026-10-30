use std::{mem, ops::Range};

use collections::HashSet;
use editor::{SelectionEffects, SplittableEditor, scroll::Autoscroll};
use file_icons::FileIcons;
use git::{repository::RepoPath, status::FileStatus};
use gpui::{
    AnyElement, ClickEvent, Context, DragMoveEvent, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, KeyContext, Pixels, ScrollStrategy, Stateful, StyleRefinement, Subscription,
    UniformListScrollHandle, actions, deferred, px, uniform_list,
};
use multi_buffer::PathKey;
use settings::{Settings, SettingsStore};
use smallvec::SmallVec;
use theme::ActiveTheme;
use ui::{IndentGuideColors, Tooltip, WithScrollbar, prelude::*};
use util::paths::PathStyle;
use workspace::Workspace;

use crate::{git_panel::TreeNode, git_panel_settings::GitPanelSettings, git_status_icon};

actions!(
    git,
    [
        /// Shows or hides the file tree next to branch and commit diffs.
        ToggleDiffFileTree,
    ]
);

actions!(
    diff_file_tree,
    [
        /// Moves focus between the diff's file tree and its editor, showing the
        /// file tree if it is hidden.
        ToggleFocus,
        /// Expands the selected directory in the diff's file tree, or shows the
        /// selected file in the diff while keeping focus in the file tree.
        ExpandSelectedEntry,
        /// Collapses the selected directory in the diff's file tree, or selects
        /// its parent directory.
        CollapseSelectedEntry,
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
            window.dispatch_action(Box::new(ToggleDiffFileTree), cx);
        })
}

pub(crate) fn toggle_focus(
    file_tree: &Entity<DiffFileTree>,
    editor_focus_handle: &FocusHandle,
    window: &mut Window,
    cx: &mut App,
) {
    if file_tree.focus_handle(cx).contains_focused(window, cx) {
        editor_focus_handle.focus(window, cx);
        return;
    }
    file_tree.update(cx, |file_tree, cx| file_tree.select_first_if_none(cx));
    if !GitPanelSettings::get_global(cx).diff_file_tree {
        // Showing the tree writes the settings file asynchronously. Focusing the
        // tree before it renders would send keystrokes nowhere, so the editor
        // keeps focus until `sync_focus` sees the tree shown.
        file_tree.update(cx, |file_tree, _| file_tree.focus_when_shown = true);
        window.dispatch_action(Box::new(ToggleDiffFileTree), cx);
        return;
    }
    file_tree.focus_handle(cx).focus(window, cx);
}

pub(crate) fn subscribe<T: 'static>(
    file_tree: &Entity<DiffFileTree>,
    editor: &Entity<SplittableEditor>,
    window: &mut Window,
    cx: &mut Context<T>,
    open_path: impl Fn(&mut T, PathKey, &mut Window, &mut Context<T>) + 'static,
) -> Subscription {
    let editor = editor.downgrade();
    cx.subscribe_in(
        file_tree,
        window,
        move |this, _file_tree, event, window, cx| match event {
            DiffFileTreeEvent::OpenEntry(path_key) => open_path(this, path_key.clone(), window, cx),
            DiffFileTreeEvent::FocusEditor => {
                if let Some(editor) = editor.upgrade() {
                    editor.focus_handle(cx).focus(window, cx);
                }
            }
        },
    )
}

/// Returns false when the file's excerpt hasn't been added to the editor yet,
/// so the caller can retry once it has.
pub(crate) fn scroll_editor_to_path(
    editor: &Entity<SplittableEditor>,
    path_key: &PathKey,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let rhs_editor = editor.read(cx).rhs_editor().clone();
    let Some(position) = rhs_editor
        .read(cx)
        .buffer()
        .read(cx)
        .location_for_path(path_key, cx)
    else {
        return false;
    };
    rhs_editor.update(cx, |editor, cx| {
        editor.change_selections(
            SelectionEffects::scroll(Autoscroll::focused()),
            window,
            cx,
            |selections| selections.select_ranges([position..position]),
        );
    });
    true
}

/// Called when the tree's visibility may have changed. Returns focus to the
/// editor when the tree is hidden while focused, so keyboard input doesn't end
/// up in an element that is no longer rendered, and focuses the tree once it
/// is shown by `toggle_focus`.
pub(crate) fn sync_focus(
    file_tree: &Entity<DiffFileTree>,
    is_visible: bool,
    editor_focus_handle: &FocusHandle,
    window: &mut Window,
    cx: &mut App,
) {
    if is_visible {
        let focus_when_shown = file_tree.update(cx, |file_tree, _| {
            mem::take(&mut file_tree.focus_when_shown)
        });
        if focus_when_shown {
            file_tree.focus_handle(cx).focus(window, cx);
        }
    } else if file_tree.focus_handle(cx).contains_focused(window, cx) {
        editor_focus_handle.focus(window, cx);
    }
}

pub(crate) fn add_key_context(key_context: &mut KeyContext, is_visible: bool) {
    key_context.add("diff_file_tree");
    if is_visible {
        key_context.add("diff_file_tree_visible");
    }
}

const TREE_INDENT: f32 = 16.0;
const INDENT_GUIDE_LEFT_OFFSET: Pixels = px(19.);
const DEFAULT_FILE_TREE_WIDTH: Pixels = px(250.);
const MIN_FILE_TREE_WIDTH: Pixels = px(150.);
const MAX_FILE_TREE_WIDTH: Pixels = px(600.);
const RESIZE_HANDLE_WIDTH: Pixels = px(6.);

/// Carries the id of the tree being resized, since several trees can be
/// visible at once (e.g. split commit views) and each listens for drag moves.
#[derive(Clone)]
struct DraggedDiffFileTreeEdge(EntityId);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiffFileTreeEntry {
    pub(crate) repo_path: RepoPath,
    pub(crate) status: FileStatus,
    pub(crate) path_key: PathKey,
}

pub(crate) enum DiffFileTreeEvent {
    OpenEntry(PathKey),
    FocusEditor,
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
    width: Pixels,
    focus_handle: FocusHandle,
    focus_when_shown: bool,
    _settings_subscription: Subscription,
}

impl EventEmitter<DiffFileTreeEvent> for DiffFileTree {}

impl Focusable for DiffFileTree {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

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
            width: DEFAULT_FILE_TREE_WIDTH,
            focus_handle: cx.focus_handle(),
            focus_when_shown: false,
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

    /// Embeds the tree as a cached view. Uncached, it is re-rendered on every
    /// frame the diff editor beside it scrolls, since both share a parent view.
    pub(crate) fn cached_view(file_tree: Entity<Self>, cx: &App) -> impl IntoElement {
        // A cached view is sized by this style rather than its contents, so the
        // dragged width has to be applied here too.
        let width = file_tree.read(cx).width;
        file_tree.cached(StyleRefinement::default().flex_none().h_full().w(width))
    }

    pub(crate) fn file_count(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn entries(&self) -> &[DiffFileTreeEntry] {
        &self.entries
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

    #[cfg(test)]
    pub(crate) fn selected_path(&self) -> Option<String> {
        self.selected
            .as_ref()
            .map(|path| path.as_unix_str().to_string())
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

    fn resize(&mut self, event: &DragMoveEvent<DraggedDiffFileTreeEdge>, cx: &mut Context<Self>) {
        let is_this_tree = event
            .dragged_item()
            .downcast_ref::<DraggedDiffFileTreeEdge>()
            .is_some_and(|dragged| dragged.0 == cx.entity_id());
        if !is_this_tree {
            return;
        }
        let width = (event.event.position.x - event.bounds.left())
            .clamp(MIN_FILE_TREE_WIDTH, MAX_FILE_TREE_WIDTH);
        if width != self.width {
            self.width = width;
            cx.notify();
        }
    }

    fn render_resize_handle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity_id = cx.entity_id();
        deferred(
            div()
                .id("diff-file-tree-resize-handle")
                .absolute()
                .top_0()
                .right(-RESIZE_HANDLE_WIDTH / 2.)
                .h_full()
                .w(RESIZE_HANDLE_WIDTH)
                .cursor_col_resize()
                .on_drag(DraggedDiffFileTreeEdge(entity_id), |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| gpui::Empty)
                })
                .on_click(cx.listener(|this, event: &ClickEvent, _window, cx| {
                    if event.click_count() >= 2 {
                        this.width = DEFAULT_FILE_TREE_WIDTH;
                        cx.notify();
                    }
                    cx.stop_propagation();
                }))
                .occlude(),
        )
    }

    fn row_path<'a>(&'a self, row: &'a Row) -> Option<&'a RepoPath> {
        match row {
            Row::Directory { path, .. } => Some(path),
            Row::File { entry_ix, .. } => self.entries.get(*entry_ix).map(|entry| &entry.repo_path),
        }
    }

    fn selected_row_index(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.rows
            .iter()
            .position(|row| self.row_path(row) == Some(selected))
    }

    fn select_row(&mut self, row_ix: usize, cx: &mut Context<Self>) {
        let Some(path) = self
            .rows
            .get(row_ix)
            .and_then(|row| self.row_path(row))
            .cloned()
        else {
            return;
        };
        self.selected = Some(path);
        self.scroll_handle
            .scroll_to_item(row_ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    pub(crate) fn select_first_if_none(&mut self, cx: &mut Context<Self>) {
        if self.selected_row_index().is_none() {
            self.select_row(0, cx);
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let next_ix = match self.selected_row_index() {
            Some(row_ix) => (row_ix + 1).min(self.rows.len().saturating_sub(1)),
            None => 0,
        };
        self.select_row(next_ix, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_ix = self
            .selected_row_index()
            .map_or(0, |row_ix| row_ix.saturating_sub(1));
        self.select_row(previous_ix, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select_row(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select_row(self.rows.len().saturating_sub(1), cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row_ix) = self.selected_row_index() else {
            return;
        };
        match self.rows.get(row_ix) {
            Some(Row::Directory { path, .. }) => {
                let path = path.clone();
                self.toggle_directory(&path, cx);
            }
            Some(Row::File { entry_ix, .. }) => {
                self.open_entry(*entry_ix, cx);
                cx.emit(DiffFileTreeEvent::FocusEditor);
            }
            None => {}
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DiffFileTreeEvent::FocusEditor);
    }

    fn expand_selected_entry(
        &mut self,
        _: &ExpandSelectedEntry,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row_ix) = self.selected_row_index() else {
            self.select_row(0, cx);
            return;
        };
        match self.rows.get(row_ix) {
            Some(Row::Directory {
                path,
                expanded: false,
                ..
            }) => {
                let path = path.clone();
                self.toggle_directory(&path, cx);
            }
            Some(Row::Directory { expanded: true, .. }) => self.select_row(row_ix + 1, cx),
            Some(Row::File { entry_ix, .. }) => self.open_entry(*entry_ix, cx),
            None => {}
        }
    }

    fn collapse_selected_entry(
        &mut self,
        _: &CollapseSelectedEntry,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row_ix) = self.selected_row_index() else {
            self.select_row(0, cx);
            return;
        };
        let Some(row) = self.rows.get(row_ix) else {
            return;
        };
        if let Row::Directory {
            path,
            expanded: true,
            ..
        } = row
        {
            let path = path.clone();
            self.toggle_directory(&path, cx);
            return;
        }

        let depth = row.depth();
        let parent_ix = self.rows.get(..row_ix).and_then(|preceding_rows| {
            preceding_rows.iter().rposition(|candidate| {
                matches!(candidate, Row::Directory { .. }) && candidate.depth() < depth
            })
        });
        if let Some(parent_ix) = parent_ix {
            self.select_row(parent_ix, cx);
        }
    }

    fn visible_depths(&self, range: Range<usize>) -> SmallVec<[usize; 64]> {
        range
            .map(|ix| self.rows.get(ix).map_or(0, Row::depth))
            .collect()
    }

    fn render_row(
        &self,
        ix: usize,
        is_focused: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let element = match self.rows.get(ix)? {
            Row::Directory {
                path,
                name,
                depth,
                expanded,
            } => {
                let is_selected = self.selected.as_ref() == Some(path);
                let path = path.clone();
                let (chevron, folder) = if *expanded {
                    (IconName::ChevronDown, IconName::FolderOpen)
                } else {
                    (IconName::ChevronRight, IconName::Folder)
                };
                self.render_row_container(ix, is_selected, is_focused, cx)
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
                                        Icon::new(folder).size(IconSize::Small).color(Color::Muted),
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
                        this.selected = Some(path.clone());
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

                self.render_row_container(ix, is_selected, is_focused, cx)
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
                        cx.emit(DiffFileTreeEvent::FocusEditor);
                    }))
            }
        };
        Some(element.into_any_element())
    }

    fn render_row_container(
        &self,
        ix: usize,
        is_selected: bool,
        is_focused: bool,
        cx: &App,
    ) -> Stateful<Div> {
        let colors = cx.theme().colors();
        h_flex()
            .id(("diff-file-tree-row", ix))
            .h(rems(1.75))
            .w_full()
            .min_w_0()
            .pl_2p5()
            .pr_1()
            .border_1()
            .cursor_pointer()
            .when(is_selected, |this| this.bg(colors.ghost_element_selected))
            .when(is_selected && is_focused, |this| {
                this.border_color(colors.panel_focused_border)
            })
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

        let is_focused = self.focus_handle.contains_focused(window, cx);

        v_flex()
            .relative()
            .track_focus(&self.focus_handle)
            .key_context("DiffFileTree")
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::expand_selected_entry))
            .on_action(cx.listener(Self::collapse_selected_entry))
            .flex_none()
            .h_full()
            .w(self.width)
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
                            cx.processor(move |this, range: Range<usize>, _window, cx| {
                                range
                                    .filter_map(|ix| this.render_row(ix, is_focused, cx))
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
            .on_drag_move(cx.listener(|this, event, _window, cx| this.resize(event, cx)))
            .child(self.render_resize_handle(cx))
    }
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

    let root = TreeNode::build(entries.iter().enumerate(), |(_, entry)| &entry.repo_path);
    let mut rows = Vec::new();
    push_rows(&root, 0, collapsed_dirs, &mut rows);
    rows
}

fn push_rows(
    node: &TreeNode<(usize, &DiffFileTreeEntry)>,
    depth: usize,
    collapsed_dirs: &HashSet<RepoPath>,
    rows: &mut Vec<Row>,
) {
    for child in node.children.values() {
        let (terminal, name, path) = child.compact_directory_chain();
        let Some(path) = path else {
            continue;
        };
        let expanded = !collapsed_dirs.contains(&path);
        rows.push(Row::Directory {
            path,
            name,
            depth,
            expanded,
        });
        if expanded {
            push_rows(terminal, depth + 1, collapsed_dirs, rows);
        }
    }

    for &(entry_ix, _) in &node.files {
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
