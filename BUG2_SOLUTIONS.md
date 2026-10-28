# Bug 2 Solutions: Vim repeat state leaking when the replay chain dies

## Candidate solutions

### 1. Run the replay cleanup on the Insertion early-return paths (MOST STRAIGHTFORWARD — implemented)

In `Replayer::next` (`crates/vim/src/normal/repeat.rs`), when a
`ReplayableAction::Insertion` cannot be applied because the window has no
workspace or the active item is not an editor, run the same cleanup that the
"queue exhausted" path runs: take `globals.replayer` and reset
`globals.dot_replaying`, instead of silently `return`ing.

- Pros: minimal, targeted at the exact leak; reuses the existing, tested
  fail-safe semantics; aborting a replay that has lost its target editor matches
  Vim's behavior (an interrupted `.`/macro simply stops).
- Cons: the remaining queued actions are dropped (but continuing to replay into
  whatever grabs focus next would be worse).

### 2. Skip the unplayable insertion and keep the chain alive

Instead of aborting, schedule `next()` again so later actions (including the
trailing `EndRepeat`) still run.

- Pros: replay "recovers" if the editor becomes active again a frame later.
- Cons: replaying the rest of a change with a missing middle insertion produces
  corrupted edits; dispatching leftover normal-mode actions at a non-editor item
  is unpredictable. Worse than aborting.

### 3. Make `EndRepeat` also clear the wedged replayer

Have the `EndRepeat` handler call `stop_replaying` (take the replayer, stop it).

- Pros: makes the manual escape hatch fully effective.
- Cons: `EndRepeat` is dispatched *mid-chain* during every normal replay, and
  during macro replays that embed a `.`; taking the replayer there would drop the
  remaining macro actions and break nested replay. Only safe with extra
  "is the chain still running" bookkeeping. Not straightforward.

### 4. Watchdog timer that resets `dot_replaying` after N seconds

- Pros: catches any unknown leak path.
- Cons: papers over bugs, adds timing-dependent behavior, wrong resets during
  legitimately long replays. Rejected.

## Chosen fix

Solution 1. Both `let ... else { return; }` guards in the Insertion branch of
`Replayer::next` now perform the fail-safe cleanup (`globals.replayer.take()` and
`globals.dot_replaying = false`), shared with the queue-exhausted path via a small
`finish_replay` helper.
