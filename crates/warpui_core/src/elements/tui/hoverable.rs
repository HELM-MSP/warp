//! [`TuiHoverable`]: wraps a child, tracks pointer-over state on a caller-owned
//! handle, and runs a click callback — the TUI mirror of the GUI's `Hoverable`,
//! reusing the same [`MouseStateHandle`]/[`MouseState`] so hover *and* click
//! gestures live on the state-owning element (the TUI's `TuiEventHandler` only
//! exposes raw key events).
//!
//! # Construction
//! The composing view owns a [`MouseStateHandle`] (created once and reused
//! across renders, since the element tree is rebuilt every frame), reads
//! [`MouseState::is_hovered`] at composition time to pick styles, and wraps
//! the element with [`TuiHoverable::new`], registering a click handler via
//! [`on_click`](TuiHoverable::on_click). Layout, render, height, and cursor
//! are transparent — they delegate to the wrapped child.
//!
//! # Dispatch policy
//! Hover and click hit-test against the child's laid-out footprint (the size
//! returned by the most recent `layout`, anchored at the area's origin), not
//! the whole slot the parent assigned — so trailing blank space in a flex row
//! is not part of the target. On [`MouseMoved`](TuiEvent::MouseMoved) the
//! pointer position is compared against that footprint; a hover transition is
//! recorded on the handle and queues a notification so the owning view
//! re-renders. Mouse moves are never consumed, so sibling hoverables observe
//! their own transitions from the same event. Other events are offered to the
//! child first; clicks use the GUI's press-then-release pairing: an unconsumed
//! [`LeftMouseDown`](TuiEvent::LeftMouseDown) inside the footprint arms a
//! pending click (recorded on the shared state, so [`MouseState::is_clicked`]
//! styling works) and is consumed; the following
//! [`LeftMouseUp`](TuiEvent::LeftMouseUp) disarms it, running the click
//! handler only when released inside the footprint. (Hover delays and the
//! other [`MouseState`] fields are unused.)

use std::sync::MutexGuard;

use super::{
    TuiBuffer, TuiConstraint, TuiElement, TuiEvent, TuiEventContext, TuiLayoutContext,
    TuiPresentationContext, TuiRect, TuiRectExt, TuiSize,
};
use crate::elements::{MouseState, MouseStateHandle};
use crate::AppContext;

type ClickCallback = Box<dyn FnMut(&mut TuiEventContext, &AppContext)>;

pub struct TuiHoverable {
    child: Box<dyn TuiElement>,
    state: MouseStateHandle,
    on_click: Option<ClickCallback>,
    /// Size from the most recent `layout`, bounding hover/click hit-testing
    /// to the child's content rather than the parent's whole slot.
    laid_out: Option<TuiSize>,
}

impl TuiHoverable {
    /// Wraps `child`, recording hover transitions on `state`.
    pub fn new(state: MouseStateHandle, child: Box<dyn TuiElement>) -> Self {
        Self {
            child,
            state,
            on_click: None,
            laid_out: None,
        }
    }

    /// Registers `callback` to run on a left click — a `LeftMouseDown` that
    /// reaches this element unhandled by the child, followed by a
    /// `LeftMouseUp`, both within this element's area.
    pub fn on_click(
        mut self,
        callback: impl FnMut(&mut TuiEventContext, &AppContext) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(callback));
        self
    }

    /// Locks and returns the shared mouse state.
    fn state(&self) -> MutexGuard<'_, MouseState> {
        self.state.lock().unwrap()
    }

    /// The child's laid-out footprint within `area`, used for hit-testing.
    /// Falls back to the whole area before the first `layout`.
    ///
    /// The footprint keeps `area`'s top-left corner, which is correct because
    /// all current TUI parents paint a child at the top-left of its slot. If a
    /// parent ever paints a child at an offset within its slot (e.g. a flex
    /// with center/end cross-axis alignment), that parent must pass the
    /// offset rect down as `area` — only it knows where the child landed.
    fn hit_area(&self, area: TuiRect) -> TuiRect {
        match self.laid_out {
            Some(size) => TuiRect::new(
                area.x,
                area.y,
                area.width.min(size.width),
                area.height.min(size.height),
            ),
            None => area,
        }
    }
}

impl TuiElement for TuiHoverable {
    fn layout(
        &mut self,
        constraint: TuiConstraint,
        ctx: &mut TuiLayoutContext,
        app: &AppContext,
    ) -> TuiSize {
        let size = self.child.layout(constraint, ctx, app);
        self.laid_out = Some(size);
        size
    }

    fn render(&self, area: TuiRect, buffer: &mut TuiBuffer, ctx: &mut TuiLayoutContext) {
        self.child.render(area, buffer, ctx);
    }

    fn cursor_position(&self, area: TuiRect, ctx: &mut TuiLayoutContext) -> Option<(u16, u16)> {
        self.child.cursor_position(area, ctx)
    }

    fn present(&mut self, ctx: &mut TuiPresentationContext<'_>) {
        self.child.present(ctx);
    }

    fn dispatch_event(
        &mut self,
        event: &TuiEvent,
        area: TuiRect,
        event_ctx: &mut TuiEventContext,
        ctx: &mut TuiLayoutContext,
        app: &AppContext,
    ) -> bool {
        let child_handled = self.child.dispatch_event(event, area, event_ctx, ctx, app);
        let hit_area = self.hit_area(area);

        if let TuiEvent::MouseMoved { position, .. } = event {
            let is_hovered = hit_area.contains_point(*position);
            let mut state = self.state();
            if is_hovered != state.is_hovered() {
                state.is_hovered = is_hovered;
                drop(state);
                event_ctx.notify();
            }
            // Mouse moves are never consumed so sibling hoverables can track
            // their own transitions from the same event.
            return false;
        }

        if child_handled {
            return true;
        }

        match event {
            // Press inside the footprint: arm the pending click.
            TuiEvent::LeftMouseDown {
                position,
                click_count,
                ..
            } if self.on_click.is_some() && hit_area.contains_point(*position) => {
                self.state().set_click_count(Some(*click_count));
                event_ctx.notify();
                true
            }
            // Release while armed: disarm, and fire only when released inside
            // the footprint (a release elsewhere cancels the click, as in the GUI).
            TuiEvent::LeftMouseUp { position, .. } if self.state().is_clicked() => {
                self.state().set_click_count(None);
                event_ctx.notify();
                if hit_area.contains_point(*position) {
                    if let Some(on_click) = self.on_click.as_mut() {
                        on_click(event_ctx, app);
                    }
                    return true;
                }
                false
            }
            _ => false,
        }
    }
}

#[cfg(test)]
#[path = "hoverable_tests.rs"]
mod tests;
