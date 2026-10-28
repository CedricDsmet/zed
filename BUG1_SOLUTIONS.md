# Bug 1 Solutions: Slow `space` open in the project panel

The root cause (see BUG1_RESEARCH.md): with vim mode enabled, vim.json's
`!Editor && !Terminal` block defines `space f`, `space /`, `space shift-s`, and
`space w …` chords. These match in the project panel and turn a plain `space`
press into a *pending* keystroke, so `project_panel::Open` only fires after
GPUI's 1-second multi-key timeout.

## Option A (implemented): Bind `space` to `project_panel::Open` in vim.json's project panel block — most straightforward

Add `"space": "project_panel::Open"` to the existing
`"ProjectPanel && not_editing"` (netrw compatibility) block in
`assets/keymaps/vim.json`. That block appears *after* the `!Editor && !Terminal`
block in the file, so the new binding has a higher keymap index than the `space …`
chords. `Keymap::bindings_for_input` (crates/gpui/src/keymap.rs:229-234) then
suppresses those chords as pending candidates, and `space` dispatches
immediately.

- Pros: one-line change; scoped to vim users in the project panel (non-vim users
  are unaffected and already have no delay); consistent with the netrw block,
  which already overrides many panel keys (`p`, `t`, `enter`, …).
- Cons: `space f` / `space /` / `space shift-s` / `space w …` chords no longer
  work while the project panel is focused (they now open the selected file
  instead of waiting for a second key). These remain available from every other
  non-editor surface, and the panel offers alternatives for its own actions
  (`/` for search, `p`/`enter`/`t` to open). This trade-off is inherent: either
  `space` dispatches instantly or it waits to disambiguate the chords.

## Option B: Exclude the project panel from the vim leader-ish block

Change the block's context to `!Editor && !Terminal && !ProjectPanel`.

- Pros: also removes the delay; keeps the chord definitions in one place.
- Cons: same functional loss as Option A but spread over every chord in the
  block; a stack-level `!ProjectPanel` predicate is subtler than an explicit
  per-panel override and diverges from how the netrw block already handles
  conflicts.

## Option C: Add the override to `assets/keymaps/specific-overrides.json`

That file loads after all base keymaps and exists for tight-context overrides.

- Pros: highest non-user precedence.
- Cons: the conflict is vim-specific, and specific-overrides.json asks that
  entries be added "only if absolutely needed"; the vim netrw block is the more
  natural home. Behavior for non-vim users would be unchanged either way.

## Option D: Change GPUI's pending-binding precedence rules

Teach `bindings_for_input` to drop pending candidates when a full match exists in
a more specific context (e.g. compare context specificity, not just binding
index/depth).

- Pros: fixes the whole class of "chord in broad context delays single key in
  specific context" problems (outline panel, git panel `space`, …).
- Cons: `ProjectPanel` and `!Editor && !Terminal` both match at the same stack
  depth, so there is no existing precedence signal to compare — this needs a new
  notion of predicate specificity, changes keystroke semantics globally, and has
  a large regression surface. Not straightforward.

## Option E: Shorten the 1-second pending timeout

- Cons: a global behavior change that only papers over the problem; every vim
  multi-key sequence would be affected. Rejected.

**Chosen: Option A.**
