use std::{sync::LazyLock, time::Duration};

use crate::{IsZero, Pixels, Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px};

const VISIBLE_OWNER_DEVICE_PIXELS: f32 = 0.75;
const BOUNDARY_EPSILON: f32 = 1.0e-4;
const WHEEL_TRANSACTION_IDLE: Duration = Duration::from_millis(110);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ScrollNodeId(pub(crate) usize);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum NestedScrollPolicy {
    StickyVisibleOwner,
    #[default]
    ContinuousOutward,
}

impl NestedScrollPolicy {
    fn configured() -> Self {
        static POLICY: LazyLock<NestedScrollPolicy> = LazyLock::new(|| {
            match std::env::var("GPUI_NESTED_SCROLL_POLICY")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str()
            {
                "sticky" | "sticky-visible-owner" => NestedScrollPolicy::StickyVisibleOwner,
                _ => NestedScrollPolicy::ContinuousOutward,
            }
        });
        *POLICY
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransactionKind {
    Finger,
    Wheel,
}

#[derive(Clone, Copy, Debug, Default)]
struct CandidateMotion {
    position: Point<f32>,
    max_excursion: f32,
}

impl CandidateMotion {
    fn record(&mut self, applied: Point<Pixels>, device_scale: f32) -> bool {
        self.position.x += applied.x.as_f32();
        self.position.y += applied.y.as_f32();
        self.max_excursion = self
            .max_excursion
            .max(self.position.x.hypot(self.position.y) * device_scale.max(f32::EPSILON));
        self.max_excursion >= VISIBLE_OWNER_DEVICE_PIXELS
    }
}

#[derive(Debug)]
struct ScrollTransaction {
    kind: TransactionKind,
    chain: Vec<ScrollNodeId>,
    frozen: bool,
    current_index: usize,
    committed: bool,
    pending_handoff: bool,
    candidate: CandidateMotion,
    provisional_direction: Point<f32>,
    last_event_at: std::time::Instant,
}

#[derive(Debug)]
struct MomentumChain {
    driver: ScrollNodeId,
    chain: Vec<ScrollNodeId>,
    current_index: usize,
    pending_handoff: bool,
    policy: NestedScrollPolicy,
}

impl MomentumChain {
    fn owner(&self) -> Option<ScrollNodeId> {
        self.chain.get(self.current_index).copied()
    }
}

impl ScrollTransaction {
    fn new(kind: TransactionKind, direction: Point<Pixels>, now: std::time::Instant) -> Self {
        Self {
            kind,
            chain: Vec::new(),
            frozen: false,
            current_index: 0,
            committed: kind == TransactionKind::Wheel,
            pending_handoff: false,
            candidate: CandidateMotion::default(),
            provisional_direction: point(direction.x.as_f32(), direction.y.as_f32()),
            last_event_at: now,
        }
    }

    fn owner(&self) -> Option<ScrollNodeId> {
        self.committed
            .then(|| self.chain.get(self.current_index).copied())
            .flatten()
    }

    fn index_for_node(&mut self, node: ScrollNodeId, under_pointer: bool) -> Option<usize> {
        if let Some(index) = self.chain.iter().position(|candidate| *candidate == node) {
            return Some(index);
        }
        if self.frozen || !under_pointer {
            return None;
        }
        let index = self.chain.len();
        self.chain.push(node);
        Some(index)
    }

    fn maybe_reset_probe_for_reversal(&mut self, direction: Point<Pixels>) {
        if self.kind != TransactionKind::Finger || self.committed {
            return;
        }
        let direction = point(direction.x.as_f32(), direction.y.as_f32());
        let old_major = if self.provisional_direction.y.abs() >= self.provisional_direction.x.abs()
        {
            self.provisional_direction.y
        } else {
            self.provisional_direction.x
        };
        let new_major = if direction.y.abs() >= direction.x.abs() {
            direction.y
        } else {
            direction.x
        };
        if old_major != 0. && new_major != 0. && old_major.signum() != new_major.signum() {
            self.current_index = 0;
            self.candidate = CandidateMotion::default();
        }
        self.provisional_direction = direction;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchLifecycle {
    Movement,
    Momentum,
    Ended,
    Cancelled,
}

#[derive(Debug)]
struct ScrollDispatch {
    lifecycle: DispatchLifecycle,
    end_after_movement: bool,
    remaining: Point<Pixels>,
    applied: Point<Pixels>,
    claimed: bool,
    cancel_nodes: Vec<ScrollNodeId>,
    seen_nodes: Vec<ScrollNodeId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScrollNodeRequest {
    None,
    Cancel,
    Finish,
    Scroll(Point<Pixels>),
    Momentum(Point<Pixels>),
}

#[derive(Debug)]
pub(crate) struct NestedScrollCoordinator {
    policy: NestedScrollPolicy,
    transaction: Option<ScrollTransaction>,
    dispatch: Option<ScrollDispatch>,
    momentum: Option<MomentumChain>,
    completed_applied: Point<Pixels>,
    device_scale: f32,
}

impl Default for NestedScrollCoordinator {
    fn default() -> Self {
        Self {
            policy: NestedScrollPolicy::configured(),
            transaction: None,
            dispatch: None,
            momentum: None,
            completed_applied: Point::default(),
            device_scale: 1.,
        }
    }
}

impl NestedScrollCoordinator {
    pub(crate) fn begin_dispatch(
        &mut self,
        event: &ScrollWheelEvent,
        pixel_delta: Point<Pixels>,
        now: std::time::Instant,
        device_scale: f32,
    ) {
        self.device_scale = device_scale;

        let end_after_movement = event.touch_phase == TouchPhase::Ended && !pixel_delta.is_zero();
        let lifecycle = if event.touch_phase == TouchPhase::Cancelled {
            DispatchLifecycle::Cancelled
        } else if event.touch_phase == TouchPhase::Ended && pixel_delta.is_zero() {
            DispatchLifecycle::Ended
        } else {
            DispatchLifecycle::Movement
        };
        let kind = if event.synthesize_momentum && matches!(event.delta, ScrollDelta::Pixels(_)) {
            TransactionKind::Finger
        } else {
            TransactionKind::Wheel
        };

        let replace_transaction = match self.transaction.as_ref() {
            None => lifecycle == DispatchLifecycle::Movement,
            Some(transaction) => {
                transaction.kind != kind
                    || (kind == TransactionKind::Finger && event.touch_phase == TouchPhase::Started)
                    || (kind == TransactionKind::Wheel
                        && now.saturating_duration_since(transaction.last_event_at)
                            > WHEEL_TRANSACTION_IDLE)
            }
        };

        let mut cancel_nodes = Vec::new();
        if replace_transaction {
            if let Some(transaction) = self.transaction.take() {
                cancel_nodes.extend(transaction.chain);
            }
            if let Some(driver) = self.momentum.take().map(|momentum| momentum.driver) {
                if !cancel_nodes.contains(&driver) {
                    cancel_nodes.push(driver);
                }
            }
            self.transaction = Some(ScrollTransaction::new(kind, pixel_delta, now));
        }

        if let Some(transaction) = self.transaction.as_mut() {
            transaction.last_event_at = now;
            if lifecycle == DispatchLifecycle::Movement {
                transaction.maybe_reset_probe_for_reversal(pixel_delta);
            }
        }
        self.dispatch = Some(ScrollDispatch {
            lifecycle,
            end_after_movement,
            remaining: pixel_delta,
            applied: Point::default(),
            claimed: false,
            cancel_nodes,
            seen_nodes: Vec::new(),
        });
    }

    pub(crate) fn begin_momentum_dispatch(
        &mut self,
        owner: ScrollNodeId,
        delta: Point<Pixels>,
    ) -> bool {
        if delta.is_zero() || self.momentum.as_ref().map(|momentum| momentum.driver) != Some(owner)
        {
            return false;
        }
        self.completed_applied = Point::default();
        self.dispatch = Some(ScrollDispatch {
            lifecycle: DispatchLifecycle::Momentum,
            end_after_movement: false,
            remaining: delta,
            applied: Point::default(),
            claimed: false,
            cancel_nodes: Vec::new(),
            seen_nodes: Vec::new(),
        });
        true
    }

    pub(crate) fn dispatch_active(&self) -> bool {
        self.dispatch.is_some()
    }

    pub(crate) fn request(&mut self, node: ScrollNodeId, under_pointer: bool) -> ScrollNodeRequest {
        let Some(dispatch) = self.dispatch.as_mut() else {
            return ScrollNodeRequest::None;
        };
        if dispatch.lifecycle == DispatchLifecycle::Momentum {
            let Some(momentum) = self.momentum.as_mut() else {
                return ScrollNodeRequest::None;
            };
            let Some(index) = momentum
                .chain
                .iter()
                .position(|candidate| *candidate == node)
            else {
                return ScrollNodeRequest::None;
            };
            if !dispatch.seen_nodes.contains(&node) {
                dispatch.seen_nodes.push(node);
            }
            if momentum.pending_handoff && index > momentum.current_index {
                momentum.current_index = index;
                momentum.pending_handoff = false;
            }
            return if index == momentum.current_index && !dispatch.remaining.is_zero() {
                ScrollNodeRequest::Momentum(dispatch.remaining)
            } else {
                ScrollNodeRequest::None
            };
        }
        let Some(transaction) = self.transaction.as_mut() else {
            return if dispatch.cancel_nodes.contains(&node) {
                ScrollNodeRequest::Cancel
            } else {
                ScrollNodeRequest::None
            };
        };

        let index = transaction.index_for_node(node, under_pointer);
        if index.is_some() && !dispatch.seen_nodes.contains(&node) {
            dispatch.seen_nodes.push(node);
        }
        match dispatch.lifecycle {
            DispatchLifecycle::Cancelled => {
                return if index.is_some() || dispatch.cancel_nodes.contains(&node) {
                    ScrollNodeRequest::Cancel
                } else {
                    ScrollNodeRequest::None
                };
            }
            DispatchLifecycle::Ended => {
                return if transaction.owner() == Some(node) {
                    ScrollNodeRequest::Finish
                } else if index.is_some() || dispatch.cancel_nodes.contains(&node) {
                    ScrollNodeRequest::Cancel
                } else {
                    ScrollNodeRequest::None
                };
            }
            DispatchLifecycle::Movement => {}
            DispatchLifecycle::Momentum => unreachable!(),
        }

        let Some(index) = index else {
            return if dispatch.cancel_nodes.contains(&node) {
                ScrollNodeRequest::Cancel
            } else {
                ScrollNodeRequest::None
            };
        };
        if dispatch.remaining.is_zero() {
            return ScrollNodeRequest::None;
        }

        if transaction.pending_handoff && index > transaction.current_index {
            transaction.current_index = index;
            transaction.pending_handoff = false;
            transaction.candidate = CandidateMotion::default();
            transaction.committed = transaction.kind == TransactionKind::Wheel;
        }
        if index != transaction.current_index {
            return ScrollNodeRequest::None;
        }

        ScrollNodeRequest::Scroll(dispatch.remaining)
    }

    pub(crate) fn report_consumed_with_boundary(
        &mut self,
        node: ScrollNodeId,
        requested: Point<Pixels>,
        applied: Point<Pixels>,
        boundary_confirmed: bool,
    ) {
        let Some(dispatch) = self.dispatch.as_mut() else {
            return;
        };
        let mut remainder = point(requested.x - applied.x, requested.y - applied.y);
        if remainder.x.abs().as_f32() <= BOUNDARY_EPSILON {
            remainder.x = px(0.);
        }
        if remainder.y.abs().as_f32() <= BOUNDARY_EPSILON {
            remainder.y = px(0.);
        }
        dispatch.applied.x += applied.x;
        dispatch.applied.y += applied.y;

        if dispatch.lifecycle == DispatchLifecycle::Momentum {
            let Some(momentum) = self.momentum.as_mut() else {
                return;
            };
            if momentum.owner() != Some(node) {
                return;
            }
            dispatch.claimed = true;
            if momentum.policy == NestedScrollPolicy::StickyVisibleOwner || !boundary_confirmed {
                dispatch.remaining = Point::default();
                momentum.pending_handoff = false;
            } else {
                dispatch.remaining = remainder;
                momentum.pending_handoff = !remainder.is_zero();
            }
            return;
        }
        let Some(transaction) = self.transaction.as_mut() else {
            return;
        };
        if transaction.chain.get(transaction.current_index).copied() != Some(node) {
            return;
        }

        dispatch.claimed |= !applied.is_zero() || transaction.committed;
        if !boundary_confirmed {
            dispatch.claimed = true;
        }

        if transaction.kind == TransactionKind::Finger && !transaction.committed {
            if transaction.candidate.record(applied, self.device_scale) {
                transaction.committed = true;
                dispatch.claimed = true;
                if self.policy == NestedScrollPolicy::StickyVisibleOwner || !boundary_confirmed {
                    dispatch.remaining = Point::default();
                    transaction.pending_handoff = false;
                } else {
                    dispatch.remaining = remainder;
                    transaction.pending_handoff = !remainder.is_zero();
                }
            } else if remainder.is_zero() {
                dispatch.remaining = Point::default();
            } else if !boundary_confirmed {
                dispatch.remaining = Point::default();
                transaction.pending_handoff = false;
            } else {
                dispatch.remaining = remainder;
                transaction.current_index += 1;
                transaction.candidate = CandidateMotion::default();
            }
            return;
        }

        if transaction.kind == TransactionKind::Finger
            && self.policy == NestedScrollPolicy::StickyVisibleOwner
        {
            dispatch.claimed = true;
            dispatch.remaining = Point::default();
            return;
        }

        if !boundary_confirmed {
            dispatch.remaining = Point::default();
            transaction.pending_handoff = false;
            return;
        }

        dispatch.remaining = remainder;
        transaction.pending_handoff = !remainder.is_zero();
    }

    #[cfg(test)]
    fn report_consumed(
        &mut self,
        node: ScrollNodeId,
        requested: Point<Pixels>,
        applied: Point<Pixels>,
    ) {
        self.report_consumed_with_boundary(node, requested, applied, true);
    }

    pub(crate) fn finish_dispatch(&mut self) {
        let Some(dispatch) = self.dispatch.take() else {
            return;
        };
        self.completed_applied = dispatch.applied;
        if dispatch.lifecycle == DispatchLifecycle::Momentum {
            let owner_survived = self
                .momentum
                .as_ref()
                .and_then(MomentumChain::owner)
                .is_some_and(|owner| dispatch.seen_nodes.contains(&owner));
            if let Some(momentum) = self.momentum.as_mut() {
                momentum.pending_handoff = false;
            }
            if !owner_survived {
                self.momentum = None;
            }
            return;
        }
        if let Some(transaction) = self.transaction.as_mut() {
            transaction.frozen = true;
            transaction.pending_handoff = false;
        }
        if matches!(dispatch.lifecycle, DispatchLifecycle::Ended) || dispatch.end_after_movement {
            self.momentum = self.transaction.as_ref().and_then(|transaction| {
                let driver = transaction
                    .committed
                    .then(|| transaction.chain.get(transaction.current_index).copied())
                    .flatten()?;
                Some(MomentumChain {
                    driver,
                    chain: transaction.chain.clone(),
                    current_index: transaction.current_index,
                    pending_handoff: false,
                    policy: self.policy,
                })
            });
            self.transaction = None;
        } else if matches!(dispatch.lifecycle, DispatchLifecycle::Cancelled) {
            self.transaction = None;
            self.momentum = None;
        } else if let Some(owner) = self.transaction.as_ref().and_then(ScrollTransaction::owner)
            && !dispatch.seen_nodes.contains(&owner)
        {
            self.transaction = None;
        }
    }

    pub(crate) fn take_completed_applied(&mut self) -> Point<Pixels> {
        std::mem::take(&mut self.completed_applied)
    }

    pub(crate) fn event_claimed(&self) -> bool {
        self.dispatch
            .as_ref()
            .is_some_and(|dispatch| dispatch.claimed)
    }

    #[cfg(test)]
    pub(crate) fn set_policy(&mut self, policy: NestedScrollPolicy) {
        self.policy = policy;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Modifiers, ScrollWheelEvent};
    use std::time::Instant;

    fn finger(delta: f32, phase: TouchPhase) -> ScrollWheelEvent {
        ScrollWheelEvent {
            delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
            touch_phase: phase,
            synthesize_momentum: true,
            modifiers: Modifiers::default(),
            ..Default::default()
        }
    }

    fn wheel(delta: f32) -> ScrollWheelEvent {
        ScrollWheelEvent {
            delta: ScrollDelta::Lines(point(0., delta)),
            touch_phase: TouchPhase::Moved,
            synthesize_momentum: false,
            modifiers: Modifiers::default(),
            ..Default::default()
        }
    }

    fn begin(coordinator: &mut NestedScrollCoordinator, event: &ScrollWheelEvent, now: Instant) {
        coordinator.begin_dispatch(event, event.delta.pixel_delta(px(20.)), now, 1.);
    }

    #[test]
    fn sticky_visible_child_never_hands_off_after_commitment() {
        let now = Instant::now();
        let child = ScrollNodeId(1);
        let parent = ScrollNodeId(2);
        let mut coordinator = NestedScrollCoordinator::default();
        coordinator.set_policy(NestedScrollPolicy::StickyVisibleOwner);

        let event = finger(10., TouchPhase::Started);
        begin(&mut coordinator, &event, now);
        assert_eq!(
            coordinator.request(child, true),
            ScrollNodeRequest::Scroll(point(px(0.), px(10.)))
        );
        coordinator.report_consumed(child, point(px(0.), px(10.)), point(px(0.), px(2.)));
        assert_eq!(coordinator.request(parent, true), ScrollNodeRequest::None);
        coordinator.finish_dispatch();

        let event = finger(10., TouchPhase::Moved);
        begin(&mut coordinator, &event, now + Duration::from_millis(8));
        assert_eq!(
            coordinator.request(child, true),
            ScrollNodeRequest::Scroll(point(px(0.), px(10.)))
        );
        coordinator.report_consumed(child, point(px(0.), px(10.)), Point::default());
        assert_eq!(coordinator.request(parent, true), ScrollNodeRequest::None);
        assert!(coordinator.event_claimed());
    }

    #[test]
    fn continuous_owner_hands_only_exact_remainder_outward() {
        let now = Instant::now();
        let child = ScrollNodeId(1);
        let parent = ScrollNodeId(2);
        let mut coordinator = NestedScrollCoordinator::default();
        coordinator.set_policy(NestedScrollPolicy::ContinuousOutward);

        let event = finger(10., TouchPhase::Started);
        begin(&mut coordinator, &event, now);
        assert!(matches!(
            coordinator.request(child, true),
            ScrollNodeRequest::Scroll(_)
        ));
        coordinator.report_consumed(child, point(px(0.), px(10.)), point(px(0.), px(2.)));
        assert_eq!(
            coordinator.request(parent, true),
            ScrollNodeRequest::Scroll(point(px(0.), px(8.)))
        );
        coordinator.report_consumed(parent, point(px(0.), px(8.)), point(px(0.), px(8.)));
        coordinator.finish_dispatch();

        let event = finger(-4., TouchPhase::Moved);
        begin(&mut coordinator, &event, now + Duration::from_millis(8));
        assert_eq!(coordinator.request(child, true), ScrollNodeRequest::None);
        assert_eq!(
            coordinator.request(parent, true),
            ScrollNodeRequest::Scroll(point(px(0.), px(-4.)))
        );
    }

    #[test]
    fn subvisible_child_capacity_passes_exact_remainder_to_parent() {
        let now = Instant::now();
        let child = ScrollNodeId(1);
        let parent = ScrollNodeId(2);
        let mut coordinator = NestedScrollCoordinator::default();
        coordinator.set_policy(NestedScrollPolicy::StickyVisibleOwner);

        let event = finger(10., TouchPhase::Started);
        begin(&mut coordinator, &event, now);
        assert!(matches!(
            coordinator.request(child, true),
            ScrollNodeRequest::Scroll(_)
        ));
        coordinator.report_consumed(child, point(px(0.), px(10.)), point(px(0.), px(0.4)));
        assert_eq!(
            coordinator.request(parent, true),
            ScrollNodeRequest::Scroll(point(px(0.), px(9.6)))
        );
        coordinator.report_consumed(parent, point(px(0.), px(9.6)), point(px(0.), px(9.6)));
        assert!(coordinator.event_claimed());
    }

    #[test]
    fn unconfirmed_virtual_boundary_keeps_the_child_until_layout_confirms_it() {
        let now = Instant::now();
        let child = ScrollNodeId(1);
        let parent = ScrollNodeId(2);
        let mut coordinator = NestedScrollCoordinator::default();

        let first = finger(10., TouchPhase::Started);
        begin(&mut coordinator, &first, now);
        assert!(matches!(
            coordinator.request(child, true),
            ScrollNodeRequest::Scroll(_)
        ));
        coordinator.report_consumed_with_boundary(
            child,
            point(px(0.), px(10.)),
            point(px(0.), px(2.)),
            false,
        );
        assert_eq!(coordinator.request(parent, true), ScrollNodeRequest::None);
        coordinator.finish_dispatch();

        let after_layout = finger(10., TouchPhase::Moved);
        begin(
            &mut coordinator,
            &after_layout,
            now + Duration::from_millis(8),
        );
        assert!(matches!(
            coordinator.request(child, true),
            ScrollNodeRequest::Scroll(_)
        ));
        coordinator.report_consumed(child, point(px(0.), px(10.)), Point::default());
        assert_eq!(
            coordinator.request(parent, true),
            ScrollNodeRequest::Scroll(point(px(0.), px(10.)))
        );
        coordinator.report_consumed(parent, point(px(0.), px(10.)), point(px(0.), px(10.)));
        coordinator.finish_dispatch();
    }

    #[test]
    fn pinned_child_allows_parent_to_own_then_parent_keeps_reversal() {
        let now = Instant::now();
        let child = ScrollNodeId(1);
        let parent = ScrollNodeId(2);
        let mut coordinator = NestedScrollCoordinator::default();
        coordinator.set_policy(NestedScrollPolicy::StickyVisibleOwner);

        let event = finger(10., TouchPhase::Started);
        begin(&mut coordinator, &event, now);
        assert!(matches!(
            coordinator.request(child, true),
            ScrollNodeRequest::Scroll(_)
        ));
        coordinator.report_consumed(child, point(px(0.), px(10.)), Point::default());
        assert!(matches!(
            coordinator.request(parent, true),
            ScrollNodeRequest::Scroll(_)
        ));
        coordinator.report_consumed(parent, point(px(0.), px(10.)), point(px(0.), px(10.)));
        coordinator.finish_dispatch();

        let event = finger(-5., TouchPhase::Moved);
        begin(&mut coordinator, &event, now + Duration::from_millis(8));
        assert_eq!(coordinator.request(child, true), ScrollNodeRequest::None);
        assert_eq!(
            coordinator.request(parent, true),
            ScrollNodeRequest::Scroll(point(px(0.), px(-5.)))
        );
    }

    #[test]
    fn cumulative_subpixel_motion_commits_the_child() {
        let now = Instant::now();
        let child = ScrollNodeId(1);
        let parent = ScrollNodeId(2);
        let mut coordinator = NestedScrollCoordinator::default();
        coordinator.set_policy(NestedScrollPolicy::StickyVisibleOwner);

        let first = finger(0.3, TouchPhase::Started);
        begin(&mut coordinator, &first, now);
        assert!(matches!(
            coordinator.request(child, true),
            ScrollNodeRequest::Scroll(_)
        ));
        coordinator.report_consumed(child, point(px(0.), px(0.3)), point(px(0.), px(0.3)));
        assert_eq!(coordinator.request(parent, true), ScrollNodeRequest::None);
        coordinator.finish_dispatch();

        let second = finger(0.5, TouchPhase::Moved);
        begin(&mut coordinator, &second, now + Duration::from_millis(8));
        assert!(matches!(
            coordinator.request(child, true),
            ScrollNodeRequest::Scroll(_)
        ));
        coordinator.report_consumed(child, point(px(0.), px(0.5)), point(px(0.), px(0.5)));
        assert_eq!(coordinator.request(parent, true), ScrollNodeRequest::None);
        coordinator.finish_dispatch();

        let ended = finger(0., TouchPhase::Ended);
        begin(&mut coordinator, &ended, now + Duration::from_millis(16));
        assert_eq!(coordinator.request(child, false), ScrollNodeRequest::Finish);
        assert_eq!(
            coordinator.request(parent, false),
            ScrollNodeRequest::Cancel
        );
    }

    #[test]
    fn frozen_chain_ignores_a_new_card_that_moves_under_the_pointer() {
        let now = Instant::now();
        let outer = ScrollNodeId(1);
        let moved_under_pointer = ScrollNodeId(2);
        let mut coordinator = NestedScrollCoordinator::default();
        coordinator.set_policy(NestedScrollPolicy::StickyVisibleOwner);

        let first = finger(10., TouchPhase::Started);
        begin(&mut coordinator, &first, now);
        assert!(matches!(
            coordinator.request(outer, true),
            ScrollNodeRequest::Scroll(_)
        ));
        coordinator.report_consumed(outer, point(px(0.), px(10.)), point(px(0.), px(10.)));
        coordinator.finish_dispatch();

        let second = finger(5., TouchPhase::Moved);
        begin(&mut coordinator, &second, now + Duration::from_millis(8));
        assert_eq!(
            coordinator.request(moved_under_pointer, true),
            ScrollNodeRequest::None
        );
        assert_eq!(
            coordinator.request(outer, false),
            ScrollNodeRequest::Scroll(point(px(0.), px(5.)))
        );
    }

    #[test]
    fn wheel_burst_hands_exact_remainder_outward_and_never_returns_inward() {
        let now = Instant::now();
        let child = ScrollNodeId(1);
        let parent = ScrollNodeId(2);
        let mut coordinator = NestedScrollCoordinator::default();

        let first = wheel(1.);
        begin(&mut coordinator, &first, now);
        let ScrollNodeRequest::Scroll(requested) = coordinator.request(child, true) else {
            panic!("child must receive the first wheel detent");
        };
        assert_eq!(requested, point(px(0.), px(20.)));
        coordinator.report_consumed(child, requested, point(px(0.), px(3.)));
        assert_eq!(
            coordinator.request(parent, true),
            ScrollNodeRequest::Scroll(point(px(0.), px(17.)))
        );
        coordinator.report_consumed(parent, point(px(0.), px(17.)), point(px(0.), px(17.)));
        coordinator.finish_dispatch();

        let reverse = wheel(-0.5);
        begin(&mut coordinator, &reverse, now + Duration::from_millis(50));
        assert_eq!(coordinator.request(child, true), ScrollNodeRequest::None);
        assert_eq!(
            coordinator.request(parent, true),
            ScrollNodeRequest::Scroll(point(px(0.), px(-10.)))
        );
    }

    #[test]
    fn continuous_momentum_uses_the_frozen_chain_and_exact_remainder() {
        let now = Instant::now();
        let child = ScrollNodeId(1);
        let parent = ScrollNodeId(2);
        let mut coordinator = NestedScrollCoordinator::default();
        coordinator.set_policy(NestedScrollPolicy::ContinuousOutward);

        let started = finger(10., TouchPhase::Started);
        begin(&mut coordinator, &started, now);
        assert!(matches!(
            coordinator.request(child, true),
            ScrollNodeRequest::Scroll(_)
        ));
        coordinator.report_consumed(child, point(px(0.), px(10.)), point(px(0.), px(10.)));
        assert_eq!(coordinator.request(parent, true), ScrollNodeRequest::None);
        coordinator.finish_dispatch();

        let ended = finger(0., TouchPhase::Ended);
        begin(&mut coordinator, &ended, now + Duration::from_millis(8));
        assert_eq!(coordinator.request(child, false), ScrollNodeRequest::Finish);
        assert_eq!(
            coordinator.request(parent, false),
            ScrollNodeRequest::Cancel
        );
        coordinator.finish_dispatch();

        assert!(coordinator.begin_momentum_dispatch(child, point(px(0.), px(12.))));
        assert_eq!(
            coordinator.request(child, false),
            ScrollNodeRequest::Momentum(point(px(0.), px(12.)))
        );
        coordinator.report_consumed(child, point(px(0.), px(12.)), point(px(0.), px(2.)));
        assert_eq!(
            coordinator.request(parent, false),
            ScrollNodeRequest::Momentum(point(px(0.), px(10.)))
        );
        coordinator.report_consumed(parent, point(px(0.), px(10.)), point(px(0.), px(10.)));
        coordinator.finish_dispatch();
        assert_eq!(coordinator.take_completed_applied(), point(px(0.), px(12.)));

        assert!(
            coordinator.begin_momentum_dispatch(child, point(px(0.), px(4.))),
            "the original kinetic recorder remains the driver after ownership moves outward"
        );
        assert_eq!(coordinator.request(child, false), ScrollNodeRequest::None);
        assert_eq!(
            coordinator.request(parent, false),
            ScrollNodeRequest::Momentum(point(px(0.), px(4.)))
        );
    }

    #[test]
    fn sticky_momentum_stops_at_the_committed_child_boundary() {
        let now = Instant::now();
        let child = ScrollNodeId(1);
        let parent = ScrollNodeId(2);
        let mut coordinator = NestedScrollCoordinator::default();
        coordinator.set_policy(NestedScrollPolicy::StickyVisibleOwner);

        let started = finger(10., TouchPhase::Started);
        begin(&mut coordinator, &started, now);
        assert!(matches!(
            coordinator.request(child, true),
            ScrollNodeRequest::Scroll(_)
        ));
        coordinator.report_consumed(child, point(px(0.), px(10.)), point(px(0.), px(10.)));
        assert_eq!(coordinator.request(parent, true), ScrollNodeRequest::None);
        coordinator.finish_dispatch();

        let ended = finger(0., TouchPhase::Ended);
        begin(&mut coordinator, &ended, now + Duration::from_millis(8));
        assert_eq!(coordinator.request(child, false), ScrollNodeRequest::Finish);
        assert_eq!(
            coordinator.request(parent, false),
            ScrollNodeRequest::Cancel
        );
        coordinator.finish_dispatch();

        assert!(coordinator.begin_momentum_dispatch(child, point(px(0.), px(12.))));
        assert!(matches!(
            coordinator.request(child, false),
            ScrollNodeRequest::Momentum(_)
        ));
        coordinator.report_consumed(child, point(px(0.), px(12.)), point(px(0.), px(2.)));
        assert_eq!(coordinator.request(parent, false), ScrollNodeRequest::None);
        coordinator.finish_dispatch();
        assert_eq!(coordinator.take_completed_applied(), point(px(0.), px(2.)));
    }
}
