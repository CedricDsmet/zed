# Bug 2 Research: Stuck in Vim repeat cycle, requiring manual "vim: end repeat"

## Symptom

Sometimes after using `.` (dot repeat) or replaying a macro, Vim behaves as if it is
still replaying: counts behave strangely, `.` stops working entirely, and the only
way out is running the `vim: end repeat` (`vim::EndRepeat`) action from the command
palette.

## How the record/replay state machine works

All state lives in `VimGlobals` (`crates/vim/src/state.rs:253-265`):

- `dot_recording` — set by `Vim::start_recording` (`crates/vim/src/vim.rs:1662`)
  when a buffer-modifying operation begins; cleared by
  `observe_action`/`observe_insertion` (`crates/vim/src/state.rs:968-1007`) once
  `stop_recording_after_next_action` is set, or by `stop_recording_immediately`
  (`crates/vim/src/vim.rs:1731`).
- `dot_replaying` — set to `true` in `Vim::repeat`
  (`crates/vim/src/normal/repeat.rs:407`) just before the recorded actions are
  handed to the `Replayer`.
- `replayer: Option<Replayer>` — a queue of `ReplayableAction`s drained one per
  frame via a `window.defer` chain (`Replayer::next`,
  `crates/vim/src/normal/repeat.rs:137-184`).

`Vim::repeat` appends a synthetic `EndRepeat` action to the queue
(`crates/vim/src/normal/repeat.rs:399`). When the editor handles it
(`crates/vim/src/normal/repeat.rs:60-63`), `dot_replaying` is reset and the mode
switched back to Normal. There is also a fail-safe: when the replayer runs out of
actions, `Replayer::next` takes the replayer out of globals and forces
`dot_replaying = false` (`crates/vim/src/normal/repeat.rs:147-155`). That fail-safe
was added for the case where the editor loses focus so the dispatched `EndRepeat`
action never reaches Vim's editor-registered handler (see
`test_repeat_after_blur_resets_dot_replaying`, `crates/vim/src/normal/repeat.rs:997`).

## Why stuck `dot_replaying` looks like "stuck in the repeat cycle"

While `dot_replaying == true`:

- `Vim::take_count` (`crates/vim/src/vim.rs:1369-1373`) ignores what the user
  types and always returns the recorded count.
- `Vim::start_recording` (`crates/vim/src/vim.rs:1664`) refuses to record new
  actions, so `.` can never pick up a new change.
- `NormalBefore` handling in `crates/vim/src/insert.rs:50` takes the replay path.

So the user is effectively trapped until `vim::EndRepeat` is dispatched manually.

## Root cause: the replay chain can die without running its cleanup

`Replayer::next` (`crates/vim/src/normal/repeat.rs:137-184`) drains one action and
then re-schedules itself via `window.defer(...)` (line 183). The cleanup fail-safe
only runs on the "queue exhausted" path (lines 147-155). But the
`ReplayableAction::Insertion` branch has two early returns that kill the chain
*without* any cleanup and *without* scheduling the next step:

- `crates/vim/src/normal/repeat.rs:168-170` — `Workspace::for_window` returns
  `None` (window no longer hosts a workspace).
- `crates/vim/src/normal/repeat.rs:171-177` — the workspace's *active item* is not
  an editor. This happens whenever focus/active-item changes mid-replay: the user
  clicks a dock panel (terminal, agent panel, project panel search, etc.), the
  replayed actions themselves open a non-editor item, or the tab is closed while
  the deferred chain is still draining.

When either early return fires, the state left behind is:

1. `dot_replaying` stays `true` → all the symptoms above.
2. `globals.replayer` remains `Some` with `ReplayerState.running == true` →
   every future `Replayer::replay` call returns early
   (`crates/vim/src/normal/repeat.rs:108-110`), so even after manually running
   `vim: end repeat`, subsequent `.` / macro replays splice actions into a queue
   nothing is draining until the exhausted-path cleanup happens to run again.

This precisely matches the reported behavior: repeat gets wedged intermittently
(it depends on what the active item is at the exact frame an insertion is
replayed) and requires manually invoking `vim: end repeat`.

## Secondary observation (not fixed here)

`EndRepeat` only resets `dot_replaying`; it does not clear a wedged replayer.
That is why the manual escape hatch sometimes has to be combined with more
fiddling. Fixing the leak at its source makes the escape hatch unnecessary.
