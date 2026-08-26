# Bug 3 — Possible solutions

See BUG3_RESEARCH.md for the analysis. The core problem: after a git worktree is deleted from
the worktree picker, agent threads keep `folder_paths` pointing at the removed directory, and
opening one feeds that dead path through the activation pipeline (workspace activation /
creation, agent session cwd), which fails and re-renders repeatedly instead of degrading
gracefully.

## Option 1 — Validate paths at open time and fall back to the main worktree paths (chosen)

When a thread is opened from the sidebar list, stat its `folder_paths` first. Replace every
missing folder path with the thread's recorded *main* worktree path (when that still exists),
persist the repaired metadata, and open the thread in the workspace for the surviving paths.
If nothing survives, show a toast and don't open.

- Pros: fixes the user-visible breakage at its entry point; self-heals the metadata so the
  stale worktree chip disappears and subsequent opens are clean; mirrors the existing
  `all_paths_missing` fallback in `MultiWorkspace::find_or_create_local_workspace` and the
  path-replacement flow used after archive restore (`update_restored_worktree_paths`); no
  changes to worktree/git/project plumbing; errors surface to the user (toast) when nothing
  can be done.
- Cons: one extra async fs stat per open of a non-active local thread (negligible); does not
  by itself clean up rows for worktrees deleted outside Zed until the user opens them (the
  list keeps showing the chip until then).
- **Most straightforward: yes.** Small, local to `crates/sidebar`, and reuses existing store
  and routing APIs.

## Option 2 — Make the worktree picker deletion clean up thread metadata

Extend `WorktreePickerDelegate::delete_worktree` to (a) close any open workspace for the
removed path, and (b) re-point or archive `ThreadMetadataStore` rows referencing it (like the
thread-archive flow does via `thread_worktree_archive`).

- Pros: fixes the list immediately at deletion time; no stale state ever exists.
- Cons: `git_ui_core` currently has no dependency on the agent thread metadata store — the
  coupling would have to be inverted (event/observer) or the cleanup moved into the sidebar
  observing repository updates; also doesn't help for worktrees deleted outside Zed (CLI
  `git worktree remove`), so open-time validation is needed anyway.

## Option 3 — Filter or mark stale threads in the list

During `Sidebar::rebuild_contents`, stat each thread's folder paths and grey out / hide rows
whose paths are gone.

- Pros: user sees the true state.
- Cons: `rebuild_contents` is synchronous and hot (runs on every store/workspace change);
  adding fs IO there needs caching and invalidation machinery; hiding threads silently loses
  history the user may still want to open (the conversation itself is still in the database
  and perfectly loadable).

## Option 4 — Break the retry/notify loops at the ConversationView level

Stop `handle_agent_servers_updated` from re-running the load when the previous failure was
path-related, and debounce the panel/conversation `cx.notify()` cascades on worktree events.

- Pros: defensively prevents any future "missing directory" scenario from flickering.
- Cons: treats the symptom, not the stale-path cause — the thread would still open against a
  missing cwd and fail; the retry-on-`AgentServersUpdated` exists intentionally to recover
  from authentication races and would regress that.

## Implemented fix (Option 1)

`crates/sidebar/src/sidebar.rs`:

- New `Sidebar::open_thread_entry` — the single entry point used by the sidebar row click
  handler, keyboard `confirm`, and the next/previous-thread cycling. It stats the thread's
  folder paths off the UI thread (`fs::Fs::metadata`); when every path still exists (or the
  thread is remote / already active / has no paths) it routes exactly as before via
  `open_thread_entry_unchecked`.
- New `Sidebar::open_thread_entry_with_healed_paths` — for threads with missing folder paths:
  drops each missing folder path from the thread's `WorktreePaths`, substitutes the recorded
  main worktree path when that directory still exists, persists the repaired metadata
  (`ThreadMetadataStore::save`), and then activates the thread in the workspace for the
  surviving paths (current window → other window → open a workspace, the same routing as the
  archive-restore flow). When no path survives, a status toast explains that the thread's
  folder no longer exists and nothing is opened — no dead workspace, no failing session
  load, no flicker.

Tests (`crates/sidebar/src/sidebar_tests.rs`):

- `test_open_thread_for_deleted_worktree_falls_back_to_main_paths` — a thread whose linked
  worktree directory is gone opens in the existing main-project workspace, its metadata is
  repaired to the main path, and no workspace is opened at the deleted path.
- `test_open_thread_with_no_surviving_paths_shows_toast_and_does_not_open` — when neither the
  folder path nor the main path exists, nothing is opened.

Not addressed here (follow-ups): making the worktree picker deletion archive/repair thread
metadata eagerly (Option 2), and gracefully closing a still-open workspace whose root
directory was deleted.
