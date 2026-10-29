//! Keep click movement from reaching GPUI's drag recognizer until a short hold.

use std::{cell::Cell, rc::Rc, time::Duration};

use gpui::{
    AnyElement, App, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, Hitbox,
    HitboxBehavior, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Window,
};

pub(crate) const HOLD_DELAY: Duration = Duration::from_millis(200);

#[derive(Default)]
pub(crate) struct Hold {
    generation: Cell<u64>,
    pressed: Cell<bool>,
    ready: Cell<bool>,
}

impl Hold {
    fn invalidate(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.ready.set(false);
    }

    fn cancel(&self) {
        self.invalidate();
        self.pressed.set(false);
    }
}

/// Wrap a clickable drag source without changing its layout or drop payload.
/// Each press must earn its own hold; release anywhere cancels the timer.
pub(crate) fn hold_to_drag(id: impl Into<ElementId>, child: impl IntoElement) -> HoldToDrag {
    HoldToDrag {
        id: id.into(),
        child: child.into_any_element(),
    }
}

pub(crate) struct HoldToDrag {
    id: ElementId,
    child: AnyElement,
}

impl Element for HoldToDrag {
    type RequestLayoutState = ();
    type PrepaintState = (Rc<Hold>, Hitbox);

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let hold = window.with_element_state(id.unwrap(), |state: Option<Rc<Hold>>, _| {
            let state = state.unwrap_or_default();
            (state.clone(), state)
        });
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        self.child.prepaint(window, cx);
        (hold, hitbox)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (hold, hitbox) = state.clone();
        // Register before the child: capture gates its built-in movement
        // recognizer, while bubble lets child buttons consume their presses.
        window.on_mouse_event({
            let hold = hold.clone();
            let hitbox = hitbox.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture {
                    hold.cancel();
                    return;
                }
                if !hitbox.is_hovered(window) {
                    return;
                }
                if event.button != MouseButton::Left || cx.has_active_drag() {
                    return;
                }
                hold.pressed.set(true);
                let generation = hold.generation.get();
                let hold = Rc::downgrade(&hold);
                window
                    .spawn(cx, async move |cx| {
                        cx.background_executor().timer(HOLD_DELAY).await;
                        let _ = cx.update(|window, _| {
                            if let Some(hold) = hold.upgrade()
                                && hold.generation.get() == generation
                                && hold.pressed.get()
                                && window.is_window_active()
                            {
                                hold.ready.set(true);
                            }
                        });
                    })
                    .detach();
            }
        });
        window.on_mouse_event({
            let hold = hold.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Capture || !hold.pressed.get() || cx.has_active_drag() {
                    return;
                }
                if event.pressed_button != Some(MouseButton::Left) || !window.is_window_active() {
                    hold.invalidate();
                    cx.stop_propagation();
                } else if !hold.ready.get() {
                    // Leaving before the hold completes abandons this press.
                    if !hitbox.bounds.contains(&event.position) || !hitbox.is_hovered(window) {
                        hold.invalidate();
                    }
                    cx.stop_propagation();
                }
            }
        });
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, _| {
            if phase == DispatchPhase::Capture {
                hold.cancel();
            }
        });
        self.child.paint(window, cx);
    }
}

impl IntoElement for HoldToDrag {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        AppContext, Context, Empty, Modifiers, Render, TestAppContext, div, point, prelude::*, px,
    };

    struct Fixture {
        clicks: usize,
    }

    impl Render for Fixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(hold_to_drag(
                "hold",
                div()
                    .id("row")
                    .debug_selector(|| "hold-test-row".into())
                    .w(px(200.))
                    .h(px(40.))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clicks += 1;
                        cx.notify();
                    }))
                    .on_drag((), |_, _, _, cx| cx.new(|_| Empty)),
            ))
        }
    }

    #[gpui::test]
    fn quick_click_with_jitter_still_clicks_and_each_press_needs_a_hold(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| Fixture { clicks: 0 });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let start = point(px(50.), px(20.));
        let moved = start + point(px(8.), px(0.));
        let none = Modifiers::default();
        cx.simulate_mouse_down(start, MouseButton::Left, none);
        cx.simulate_mouse_move(moved, Some(MouseButton::Left), none);
        cx.update(|_, cx| assert!(!cx.has_active_drag()));
        cx.simulate_mouse_up(moved, MouseButton::Left, none);
        view.read_with(cx, |view, _| assert_eq!(view.clicks, 1));

        // A cancelled timer cannot arm the next press.
        cx.executor().advance_clock(HOLD_DELAY / 2);
        cx.simulate_mouse_down(start, MouseButton::Left, none);
        cx.executor().advance_clock(HOLD_DELAY / 2);
        cx.run_until_parked();
        cx.simulate_mouse_move(moved, Some(MouseButton::Left), none);
        cx.update(|_, cx| assert!(!cx.has_active_drag()));
        cx.executor().advance_clock(HOLD_DELAY / 2);
        cx.run_until_parked();
        cx.simulate_mouse_move(moved, Some(MouseButton::Left), none);
        cx.update(|_, cx| assert!(cx.has_active_drag()));
        cx.simulate_mouse_up(moved, MouseButton::Left, none);
        view.read_with(cx, |view, _| {
            assert_eq!(view.clicks, 1, "drag release must not activate the row")
        });
    }

    #[gpui::test]
    fn leaving_before_the_hold_or_releasing_outside_cancels_it(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| Fixture { clicks: 0 });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let start = point(px(50.), px(20.));
        let outside = point(px(250.), px(100.));
        let none = Modifiers::default();
        assert!(!cx.debug_bounds("hold-test-row").unwrap().contains(&outside));
        cx.simulate_mouse_down(start, MouseButton::Left, none);
        cx.simulate_mouse_move(outside, Some(MouseButton::Left), none);
        cx.update(|_, cx| assert!(!cx.has_active_drag()));
        cx.executor().advance_clock(HOLD_DELAY);
        cx.run_until_parked();
        cx.simulate_mouse_move(outside, Some(MouseButton::Left), none);
        cx.update(|_, cx| assert!(!cx.has_active_drag()));
        cx.simulate_mouse_up(outside, MouseButton::Left, none);
        cx.executor().advance_clock(HOLD_DELAY);
        cx.run_until_parked();
        cx.simulate_mouse_move(start, None, none);
        cx.update(|_, cx| assert!(!cx.has_active_drag()));
        view.read_with(cx, |view, _| assert_eq!(view.clicks, 0));
    }
}
