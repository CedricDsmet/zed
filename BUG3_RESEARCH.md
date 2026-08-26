# Bug 3 — Continuous flicker when opening an agent thread whose worktree was removed

## Symptom (from bugs.md)

> continuous flickering of the cursor and refreshing of other ui parts when trying to open an
> agent thread that was created inside a worktree that has already been removed. it is still
> being shown in the agent thread list while the worktree has been deleted through the worktree
> menu next to projects.

Two observable problems:

1. The thread is still listed in the sidebar ("agent thread list") after its worktree was
   deleted.
2. Clicking / opening that thread causes continuous flickering (caret + other UI parts keep
   refreshing).

## Code path: from clicking a history thread to opening it

The sidebar (`crates/sidebar/src/sidebar.rs`) renders one row per thread. Rows are built in
`Sidebar::rebuild_contents` (sidebar.rs:1342) from `ThreadMetadataStore` rows. Each row's
`ThreadEntryWorkspace` is resolved from the thread's `folder_paths`:

- `Open(workspace)` — a workspace with exactly those paths is currently held by the
  `MultiWorkspace`.
- `Closed { folder_paths, project_group_key }` — no such workspace exists.

Clicking (sidebar.rs:6311-6329) or keyboard-confirming (`Sidebar::confirm`, sidebar.rs:3518-3555)
dispatches:

- `Open` → `Sidebar::activate_thread` (sidebar.rs:3928) →
  `activate_thread_locally` (sidebar.rs:3843): sets `active_entry` and
  `pending_thread_activation`, activates the workspace via `MultiWorkspace::activate`
  (multi_workspace.rs:1316), then `load_agent_thread_in_workspace` (sidebar.rs:3597) →
  `AgentPanel::load_agent_thread` (agent_panel.rs:4371) with
  `work_dirs = metadata.folder_paths()` — the (now deleted) worktree path.
- `Closed` → `Sidebar::open_workspace_and_activate_thread` (sidebar.rs:3953) →
  `MultiWorkspace::find_or_create_workspace` (multi_workspace.rs:1098) →
  `find_or_create_local_workspace` (multi_workspace.rs:1208). This function stats every folder
  path; when **all** of them are missing it falls back to the project group's main path list
  (multi_workspace.rs:1229-1250) — but only when a `project_group` key was passed. It then
  looks for an existing workspace for the effective paths and otherwise calls
  `Workspace::new_local` (workspace.rs:1900) on them.

`AgentPanel::load_agent_thread` builds a `ConversationView`
(`crates/agent_ui/src/conversation_view.rs:800`), whose `initial_state`
(conversation_view.rs:1023) resolves `session_work_dirs` from the passed `work_dirs` — the
deleted worktree path — and calls `connection.load_session(session_id, project,
session_work_dirs, …)` (conversation_view.rs:1110-1134). For external ACP agents (Claude Code
etc.) the work dirs become the agent process/session cwd; a missing directory makes the
load/resume fail.

## What the worktree menu deletion actually does

The "worktree menu next to projects" is the git worktree picker
(`crates/git_ui_core/src/worktree_picker.rs`). Its delete action
(`WorktreePickerDelegate::delete_worktree`, worktree_picker.rs:474) runs
`repo.remove_worktree(path, force)` — i.e. `git worktree remove`, which deletes the worktree
directory from disk — and only updates the picker's own match list (worktree_picker.rs:626-639).

It does **not**:

- close or remove any `MultiWorkspace` workspace still open on that path,
- archive or delete the `ThreadMetadataStore` rows whose `folder_paths` reference the path,
- record an archived-worktree row (`thread_worktree_archive::persist_worktree_state`) that the
  sidebar's archive-restore flow (`Sidebar::open_thread_from_archive`, sidebar.rs:4054) could
  later use to restore the directory.

This is unlike the thread-archive flow (`Sidebar::start_archive_worktree_task` /
`thread_worktree_archive::remove_root`), which persists restore state and archives threads.
That asymmetry is why the thread stays in the list pointing at a directory that no longer
exists (symptom 1).

## Why opening it misbehaves (symptom 2)

After the deletion there are two possible states:

### a) The worktree's workspace is still open (`ThreadEntryWorkspace::Open`)

`git worktree remove` does not close the open workspace, so the sidebar row still resolves to
`Open(workspace)`. Activating the thread displays a workspace whose project's only worktree
root no longer exists on disk:

- `Worktree::local` keeps a snapshot with no root entry; the fs watcher registers the missing
  root as a *pending* path and `poll_path_until_created` (fs/src/fs_watcher.rs:482) polls it
  every 2 s (`POLL_INTERVAL`, fs_watcher.rs:1090).
- The workspace's `AgentPanel` re-runs `update_thread_work_dirs` (agent_panel.rs:4113) and
  `cx.notify()` on every `WorktreeAdded/Removed/WorktreePathsChanged` project event
  (agent_panel.rs:1528-1540), and `ConversationView` notifies on every
  `WorktreeUpdatedEntries` (conversation_view.rs:829-842). As the dead worktree's state
  churns (entries dropped, git repository unloaded, watcher rescans), these subscriptions keep
  re-rendering the panel and the editor, which restarts the caret blink — visible as
  continuous cursor flicker and UI refreshing.
- For external agents, the `ConversationView` load uses the dead path as the session cwd; the
  load fails and `ConversationView::handle_agent_servers_updated`
  (conversation_view.rs:1495-1521) retries the whole load (`reset`) every time the agent
  server store emits `AgentServersUpdated`, recreating the message editor each time.

### b) The workspace was closed (`ThreadEntryWorkspace::Closed`)

`open_workspace_and_activate_thread` → `find_or_create_local_workspace` falls back to the
group's main paths only because a `project_group` key is passed. When the thread's stored
`worktree_paths` were recorded inside the worktree workspace itself (a workspace whose *main*
worktree was the linked worktree), `ProjectGroupKey::from_worktree_paths` yields the deleted
path again, so the fallback re-targets the same missing directory and
`Workspace::new_local(deleted_path)` opens a workspace with a dead root — landing in state
(a).

In both cases nothing validates `metadata.folder_paths()` against the filesystem before the
thread is opened, and nothing repairs the metadata after the worktree is gone.

## Empirical check

A reproduction test (`test_bug3_open_thread_for_deleted_worktree`,
crates/sidebar/src/sidebar_tests.rs) sets up a main project plus a thread whose
`folder_paths` point at a non-existent worktree directory and confirms the row. Results are
described in BUG3_SOLUTIONS.md alongside the chosen fix.

## Root cause summary

Deleting a git worktree from the worktree picker removes the directory but leaves the agent
thread metadata (and any open workspace) pointing at it. Opening such a thread then feeds a
non-existent directory through the activation pipeline: the workspace/panel subscriptions to
worktree and agent-server events keep firing against the dead path (load failure → retry on
`AgentServersUpdated`, worktree/watcher churn → `cx.notify()` cascades), so the conversation
view and message editor are repeatedly torn down and rebuilt — continuous caret flicker and UI
refreshing — instead of failing once with a clear error or being routed to a directory that
still exists.
