# TuiHoverable content-sized hit area — Tech Spec

Branch: `harry/fix-hoverable-hit-box`.

## Context

TUI event dispatch is area-passing: a parent hands each child a slot rect via
`TuiElement::dispatch_event(event, area, ...)`, and the child hit-tests against that
rect. `TuiFlex` gives every child its full cross-axis extent, so a collapsible
thinking header ("Thought for 4s ▾") in a flex column received a full-width row —
hover and click activated over the blank space right of the chevron, not just the
text.

The GUI framework doesn't have this bug because its `Hoverable` hit-tests against
its child's own laid-out size ([`crates/warpui_core/src/elements/gui/hoverable.rs (402-424) @ dc727c39`](https://github.com/warpdotdev/warp/blob/dc727c3950ee13f0351b162e69421ce0d300e338/crates/warpui_core/src/elements/gui/hoverable.rs#L402-L424)).
Adopting the GUI's cached-geometry dispatch wholesale was evaluated and rejected:
it depends on the retained `Scene` for clip/occlusion, requires `render(&mut self)`
to record origins, has silent stale-geometry failure modes, and doesn't fit
`TuiClipped`'s scratch-buffer rendering. Area-passing stays; the fix ports only the
"hit target = content footprint" idea into `TuiHoverable`.

## Changes

All in [`crates/warpui_core/src/elements/tui/hoverable.rs @ dc727c39`](https://github.com/warpdotdev/warp/blob/dc727c3950ee13f0351b162e69421ce0d300e338/crates/warpui_core/src/elements/tui/hoverable.rs):

- `laid_out: Option<TuiSize>` — the child's size recorded during `layout`.
- `hit_area(area)` — clips the parent-passed slot to the laid-out footprint,
  anchored at the slot's origin; falls back to the whole area before first layout.
- `dispatch_event` hover transitions and click containment both use `hit_area`;
  the child still receives the full `area`.

This is parent-agnostic: any element wrapped in a `TuiHoverable` gets a
content-sized hit target with no call-site changes (`tui_collapsible` needed none).
Elements that fill their slot (input box, `Stretch`ed banners) have
`laid_out == slot`, so the clip is a no-op and full-slot targets are preserved.

### Contract: parents own placement offsets

`hit_area` keeps `area`'s top-left corner, which is correct because all current TUI
parents paint children at the start of their slot. A parent that places a child at
an offset within its slot (e.g. `CrossAxisAlignment::Center`/`End` from
`harry/tui-flex-alignment`) must pass the placed rect down as `area` — the offset is
not recoverable in the leaf. That branch's `child_rect_for` already satisfies this
by feeding the same rect to `render`, `cursor_position`, and `dispatch_event`,
which also makes the clip a harmless no-op under those alignments. The two changes
compose without modification.

## Testing and validation

- [`hoverable_tests.rs @ dc727c39`](https://github.com/warpdotdev/warp/blob/dc727c3950ee13f0351b162e69421ce0d300e338/crates/warpui_core/src/elements/tui/hoverable_tests.rs):
  `hit_testing_is_bounded_to_the_child_laid_out_size` — hover and click register
  inside the text but not in the slot's trailing blank space.
- [`collapsible_tests.rs @ dc727c39`](https://github.com/warpdotdev/warp/blob/dc727c3950ee13f0351b162e69421ce0d300e338/crates/warpui_core/src/elements/tui/collapsible_tests.rs):
  `only_a_header_click_invokes_on_toggle` extended — clicking right of the
  label + chevron does not toggle.
- Run with `cargo nextest run -p warpui_core --features tui -E 'test(tui::hoverable) or test(tui::collapsible)'`
  (the TUI module is gated behind the `tui` feature).

## Follow-ups

- Dedupe the slot-splitting loops in `TuiFlex`/`TuiViewportedList` (repeated across
  `render`/`cursor_position`/`dispatch_event`) into one placement helper each, so
  paint and hit-test geometry cannot drift. `harry/tui-flex-alignment` already does
  this for flex via `child_rect_for`.
