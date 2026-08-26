# Bug 1 Research: Opening a file with `space` in the project panel is slow

## Symptom

While navigating the project panel ("Explorer view") with the keyboard, pressing
`space` on a file takes noticeably long (~1 second) before the file opens.
Clicking the same file with the mouse opens it instantly.

## Reproduction path

1. Enable vim mode (`"vim_mode": true`) — the reporter uses vim mode (see bug 2
   in bugs.md, which is about vim's repeat cycle).
2. Focus the project panel and select a file with `j`/`k` or arrow keys.
3. Press `space`. The file opens only after a ~1 second pause.
4. Single-click the same file with the mouse — it opens immediately.

Without vim mode the delay does not occur; both paths dispatch immediately.

## Code paths involved

### Keyboard path

- `space` is bound to `project_panel::Open` in the `ProjectPanel` context
  (`assets/keymaps/default-linux.json:1008`, same in the macOS/Windows keymaps).
- The handler is `ProjectPanel::open` (`crates/project_panel/src/project_panel.rs:1743`),
  which calls `open_internal` → `open_entry` (`project_panel.rs:2155`) →
  `cx.emit(Event::OpenedEntry { .. })`.
- The workspace subscription (`project_panel.rs:892`) handles the event and calls
  `workspace.open_path_preview(...)` (`project_panel.rs:905`).

### Mouse path

- The entry's `.on_click` listener (`crates/project_panel/src/project_panel.rs:6023`)
  calls `open_entry(entry_id, ...)` directly at `project_panel.rs:6123`.
- Same downstream flow: `Event::OpenedEntry` → `open_path_preview`.

The two paths converge on the exact same `open_entry`/`open_path_preview` code, so
the slowness is **not** in the open logic itself. The difference is in *keystroke
dispatch*, which only the keyboard path goes through.

## Root cause: `space` becomes a pending multi-key prefix under vim mode

GPUI's keystroke dispatch (`crates/gpui/src/window.rs:5309-5371`) works like this:

1. `dispatch_key` (`crates/gpui/src/key_dispatch.rs:483`) calls
   `Keymap::bindings_for_input` (`crates/gpui/src/keymap.rs:165`), which returns
   both the fully matched bindings and a `pending` flag saying "a longer binding
   could still match".
2. If `pending` is true and the prefix has a binding (`pending_has_binding`,
   `window.rs:5335-5336`), the keystroke is **held for 1 second**
   (`cx.background_executor.timer(Duration::from_secs(1))`, `window.rs:5340`)
   before the single-key binding is finally flushed and dispatched.

When vim mode is enabled, `assets/keymaps/vim.json` is loaded **after** the
default keymap (`crates/zed/src/zed.rs:2427-2444`), and it contains this block
(`assets/keymaps/vim.json:981-998`):

```json
{
  "context": "!Editor && !Terminal",
  "bindings": {
    "space f": "file_finder::Toggle",
    "space /": "pane::DeploySearch",
    "space shift-s": "project_symbols::Toggle",
    "space w h": "workspace::ActivatePaneLeft",
    "space w j": "workspace::ActivatePaneDown",
    "space w k": "workspace::ActivatePaneUp",
    "space w l": "workspace::ActivatePaneRight",
    "space w q": "pane::CloseActiveItem"
  }
}
```

The project panel's context stack satisfies `!Editor && !Terminal` (its
`ContextPredicate::depth_of`, `crates/gpui/src/keymap/context.rs:260`, matches at
the deepest node), so all of these `space …` chords are *pending candidates* when
`space` is pressed in the panel.

`bindings_for_input` only suppresses a pending binding when the fully matched
binding was added to the keymap **later** than it (`crates/gpui/src/keymap.rs:229-234`:
`if binding_ix > ix { continue; }`). The full match here is the default keymap's
`"space": "project_panel::Open"`, which is loaded *before* vim.json, so its
binding index is lower than the vim chords' indices and the pending set stays
non-empty. Result: `match_result.pending` is non-empty, `pending_has_binding` is
true, and `project_panel::Open` only fires after the 1-second pending-input
timeout (or immediately if the user presses an unrelated next key).

The mouse click never goes through keystroke dispatch, hence no delay — exactly
matching the reported symptom.

## Related (same mechanism, not fixed here)

Any other single-key `space` binding in a non-editor panel suffers the same
delay under vim mode, e.g. `outline_panel::OpenSelectedEntry`
(`default-linux.json:954`) and `git::ToggleStaged` (`default-linux.json:1029`).
