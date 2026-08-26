use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, Notify};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CoordinationLimits {
    pub(crate) key_capacity: usize,
    pub(crate) waiter_capacity: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CoordinationSnapshot {
    pub pending_keys: usize,
    pub in_flight_keys: usize,
    pub waiters: usize,
    pub retained_identifier_bytes: usize,
    pub key_capacity: usize,
    pub waiter_capacity: usize,
    pub key_high_watermark: usize,
    pub waiter_high_watermark: usize,
    pub retained_identifier_bytes_high_watermark: usize,
    pub coalesced_waiters_total: u64,
    pub completions_total: u64,
    pub cancellations_total: u64,
    pub failed_finalizations_total: u64,
    pub completed_result_owners: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoordinationPhase {
    Pending,
    InFlight,
}

struct CoordinationEntry<V, E> {
    phase: CoordinationPhase,
    waiters: HashMap<u64, oneshot::Sender<Result<V, E>>>,
}

struct CoordinationState<V, E> {
    pending: VecDeque<Arc<str>>,
    entries: HashMap<Arc<str>, CoordinationEntry<V, E>>,
    next_waiter_id: u64,
    current_waiters: usize,
    retained_identifier_bytes: usize,
    last_flush: Instant,
    key_high_watermark: usize,
    waiter_high_watermark: usize,
    retained_identifier_bytes_high_watermark: usize,
    coalesced_waiters_total: u64,
    completions_total: u64,
    cancellations_total: u64,
    failed_finalizations_total: u64,
}

impl<V, E> CoordinationState<V, E> {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            entries: HashMap::new(),
            next_waiter_id: 0,
            current_waiters: 0,
            retained_identifier_bytes: 0,
            last_flush: Instant::now(),
            key_high_watermark: 0,
            waiter_high_watermark: 0,
            retained_identifier_bytes_high_watermark: 0,
            coalesced_waiters_total: 0,
            completions_total: 0,
            cancellations_total: 0,
            failed_finalizations_total: 0,
        }
    }

    fn record_high_watermarks(&mut self) {
        self.key_high_watermark = self.key_high_watermark.max(self.entries.len());
        self.waiter_high_watermark = self.waiter_high_watermark.max(self.current_waiters);
        self.retained_identifier_bytes_high_watermark = self
            .retained_identifier_bytes_high_watermark
            .max(self.retained_identifier_bytes);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdmissionTooLarge {
    requested: usize,
    key_capacity: usize,
    waiter_capacity: usize,
}

impl fmt::Display for AdmissionTooLarge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "coordination chunk of {} exceeds key capacity {} or waiter capacity {}",
            self.requested, self.key_capacity, self.waiter_capacity
        )
    }
}

pub(crate) struct BoundedCoordinator<V, E> {
    limits: CoordinationLimits,
    batch_size: usize,
    wait: Duration,
    abandoned_outcome: E,
    state: Mutex<CoordinationState<V, E>>,
    capacity_released: Notify,
}

impl<V, E> BoundedCoordinator<V, E>
where
    V: Clone + Send + 'static,
    E: Clone + Send + 'static,
{
    pub(crate) fn new(
        limits: CoordinationLimits,
        batch_size: usize,
        wait: Duration,
        abandoned_outcome: E,
    ) -> Result<Arc<Self>, AdmissionTooLarge> {
        if batch_size == 0
            || limits.key_capacity < batch_size
            || limits.waiter_capacity < batch_size
        {
            return Err(AdmissionTooLarge {
                requested: batch_size,
                key_capacity: limits.key_capacity,
                waiter_capacity: limits.waiter_capacity,
            });
        }
        Ok(Arc::new(Self {
            limits,
            batch_size,
            wait,
            abandoned_outcome,
            state: Mutex::new(CoordinationState::new()),
            capacity_released: Notify::new(),
        }))
    }

    fn lock_state(&self) -> MutexGuard<'_, CoordinationState<V, E>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) async fn register(
        self: &Arc<Self>,
        identifiers: &[String],
    ) -> Result<Vec<WaitRegistration<V, E>>, AdmissionTooLarge> {
        if identifiers.len() > self.batch_size || identifiers.len() > self.limits.waiter_capacity {
            return Err(AdmissionTooLarge {
                requested: identifiers.len(),
                key_capacity: self.limits.key_capacity,
                waiter_capacity: self.limits.waiter_capacity,
            });
        }

        loop {
            let notified = self.capacity_released.notified();
            let admitted = {
                let mut state = self.lock_state();
                let new_identifiers = identifiers
                    .iter()
                    .filter(|identifier| !state.entries.contains_key(identifier.as_str()))
                    .collect::<HashSet<_>>();
                let has_key_capacity =
                    state.entries.len() + new_identifiers.len() <= self.limits.key_capacity;
                let has_waiter_capacity =
                    state.current_waiters + identifiers.len() <= self.limits.waiter_capacity;

                if has_key_capacity && has_waiter_capacity {
                    let mut registrations = Vec::with_capacity(identifiers.len());
                    for identifier in identifiers {
                        let key: Arc<str> = if let Some((key, _)) =
                            state.entries.get_key_value(identifier.as_str())
                        {
                            Arc::clone(key)
                        } else {
                            let key: Arc<str> = Arc::from(identifier.as_str());
                            state.retained_identifier_bytes += key.len();
                            state.pending.push_back(Arc::clone(&key));
                            state.entries.insert(
                                Arc::clone(&key),
                                CoordinationEntry {
                                    phase: CoordinationPhase::Pending,
                                    waiters: HashMap::new(),
                                },
                            );
                            key
                        };

                        let is_coalesced = state
                            .entries
                            .get(key.as_ref())
                            .is_some_and(|entry| !entry.waiters.is_empty());
                        if is_coalesced {
                            state.coalesced_waiters_total =
                                state.coalesced_waiters_total.saturating_add(1);
                        }

                        let waiter_id = state.next_waiter_id;
                        state.next_waiter_id = state.next_waiter_id.wrapping_add(1);
                        let (sender, receiver) = oneshot::channel();
                        state
                            .entries
                            .get_mut(key.as_ref())
                            .expect("coordination entry inserted before waiter")
                            .waiters
                            .insert(waiter_id, sender);
                        state.current_waiters += 1;
                        registrations.push(WaitRegistration {
                            coordinator: Arc::clone(self),
                            key,
                            waiter_id,
                            receiver: Some(receiver),
                            fallback: self.abandoned_outcome.clone(),
                            active: true,
                        });
                    }
                    state.record_high_watermarks();
                    Some(registrations)
                } else {
                    None
                }
            };
            if let Some(registrations) = admitted {
                return Ok(registrations);
            }
            notified.await;
        }
    }

    pub(crate) fn claim(self: &Arc<Self>) -> Option<ClaimGuard<V, E>> {
        let mut state = self.lock_state();
        // Demand-aware partial flush: with no key in flight, every currently
        // registered waiter is waiting on a pending key, so flushing the
        // partial claim satisfies all current demand immediately. While a
        // claim is in flight, waiters outside the pending set may bring more
        // demand, so the fixed window is retained to accumulate toward the
        // upstream batch size and protect fill efficiency.
        let has_in_flight_keys = state
            .entries
            .values()
            .any(|entry| entry.phase == CoordinationPhase::InFlight);
        let claim_size = if state.pending.len() >= self.batch_size {
            self.batch_size
        } else if !state.pending.is_empty()
            && (state.last_flush.elapsed() >= self.wait || !has_in_flight_keys)
        {
            state.pending.len()
        } else {
            return None;
        };
        state.last_flush = Instant::now();
        let identifiers = state.pending.drain(..claim_size).collect::<Vec<_>>();
        for identifier in &identifiers {
            if let Some(entry) = state.entries.get_mut(identifier.as_ref()) {
                entry.phase = CoordinationPhase::InFlight;
            }
        }
        Some(ClaimGuard {
            coordinator: Arc::clone(self),
            identifiers,
            active: true,
        })
    }

    pub(crate) fn snapshot(&self) -> CoordinationSnapshot {
        let state = self.lock_state();
        let pending_keys = state
            .entries
            .values()
            .filter(|entry| entry.phase == CoordinationPhase::Pending)
            .count();
        let in_flight_keys = state.entries.len() - pending_keys;
        CoordinationSnapshot {
            pending_keys,
            in_flight_keys,
            waiters: state.current_waiters,
            retained_identifier_bytes: state.retained_identifier_bytes,
            key_capacity: self.limits.key_capacity,
            waiter_capacity: self.limits.waiter_capacity,
            key_high_watermark: state.key_high_watermark,
            waiter_high_watermark: state.waiter_high_watermark,
            retained_identifier_bytes_high_watermark: state
                .retained_identifier_bytes_high_watermark,
            coalesced_waiters_total: state.coalesced_waiters_total,
            completions_total: state.completions_total,
            cancellations_total: state.cancellations_total,
            failed_finalizations_total: state.failed_finalizations_total,
            completed_result_owners: 0,
        }
    }

    fn cancel_waiter(&self, key: &str, waiter_id: u64) {
        let mut state = self.lock_state();
        let mut remove_pending_entry = false;
        let mut removed_waiter = false;
        if let Some(entry) = state.entries.get_mut(key) {
            removed_waiter = entry.waiters.remove(&waiter_id).is_some();
            remove_pending_entry =
                entry.waiters.is_empty() && entry.phase == CoordinationPhase::Pending;
        }
        if removed_waiter {
            state.current_waiters = state.current_waiters.saturating_sub(1);
            state.cancellations_total = state.cancellations_total.saturating_add(1);
        }
        if remove_pending_entry {
            if let Some((identifier, _)) = state.entries.remove_entry(key) {
                state.retained_identifier_bytes = state
                    .retained_identifier_bytes
                    .saturating_sub(identifier.len());
                state.pending.retain(|pending| pending.as_ref() != key);
            }
        }
        drop(state);
        self.capacity_released.notify_waiters();
    }

    fn finalize(&self, identifiers: &[Arc<str>], outcomes: Vec<Result<V, E>>) {
        let mut state = self.lock_state();
        if identifiers.len() != outcomes.len() {
            state.failed_finalizations_total = state.failed_finalizations_total.saturating_add(1);
        }

        for (index, identifier) in identifiers.iter().enumerate() {
            let outcome = outcomes
                .get(index)
                .cloned()
                .unwrap_or_else(|| Err(self.abandoned_outcome.clone()));
            let Some(entry) = state.entries.remove(identifier.as_ref()) else {
                state.failed_finalizations_total =
                    state.failed_finalizations_total.saturating_add(1);
                continue;
            };
            state.retained_identifier_bytes = state
                .retained_identifier_bytes
                .saturating_sub(identifier.len());
            state.current_waiters = state.current_waiters.saturating_sub(entry.waiters.len());
            state.completions_total = state.completions_total.saturating_add(1);
            for sender in entry.waiters.into_values() {
                let _ = sender.send(outcome.clone());
            }
        }
        drop(state);
        self.capacity_released.notify_waiters();
    }

    fn abandon_claim(&self, identifiers: &[Arc<str>]) {
        let mut state = self.lock_state();
        for identifier in identifiers {
            let Some(entry) = state.entries.remove(identifier.as_ref()) else {
                state.failed_finalizations_total =
                    state.failed_finalizations_total.saturating_add(1);
                continue;
            };
            state.retained_identifier_bytes = state
                .retained_identifier_bytes
                .saturating_sub(identifier.len());
            state.current_waiters = state.current_waiters.saturating_sub(entry.waiters.len());
            state.cancellations_total = state.cancellations_total.saturating_add(1);
            for sender in entry.waiters.into_values() {
                let _ = sender.send(Err(self.abandoned_outcome.clone()));
            }
        }
        drop(state);
        self.capacity_released.notify_waiters();
    }
}

pub(crate) struct WaitRegistration<V, E>
where
    V: Clone + Send + 'static,
    E: Clone + Send + 'static,
{
    coordinator: Arc<BoundedCoordinator<V, E>>,
    key: Arc<str>,
    waiter_id: u64,
    receiver: Option<oneshot::Receiver<Result<V, E>>>,
    fallback: E,
    active: bool,
}

impl<V, E> WaitRegistration<V, E>
where
    V: Clone + Send + 'static,
    E: Clone + Send + 'static,
{
    pub(crate) async fn receive(mut self) -> Result<V, E> {
        let receiver = self
            .receiver
            .take()
            .expect("coordination receiver consumed once");
        let outcome = receiver
            .await
            .unwrap_or_else(|_| Err(self.fallback.clone()));
        self.active = false;
        outcome
    }
}

impl<V, E> Drop for WaitRegistration<V, E>
where
    V: Clone + Send + 'static,
    E: Clone + Send + 'static,
{
    fn drop(&mut self) {
        if self.active {
            self.coordinator
                .cancel_waiter(self.key.as_ref(), self.waiter_id);
        }
    }
}

pub(crate) struct ClaimGuard<V, E>
where
    V: Clone + Send + 'static,
    E: Clone + Send + 'static,
{
    coordinator: Arc<BoundedCoordinator<V, E>>,
    identifiers: Vec<Arc<str>>,
    active: bool,
}

impl<V, E> ClaimGuard<V, E>
where
    V: Clone + Send + 'static,
    E: Clone + Send + 'static,
{
    pub(crate) fn identifiers(&self) -> &[Arc<str>] {
        &self.identifiers
    }

    pub(crate) fn finalize(mut self, outcomes: Vec<Result<V, E>>) {
        self.coordinator.finalize(&self.identifiers, outcomes);
        self.active = false;
    }
}

impl<V, E> Drop for ClaimGuard<V, E>
where
    V: Clone + Send + 'static,
    E: Clone + Send + 'static,
{
    fn drop(&mut self) {
        if self.active {
            self.coordinator.abandon_claim(&self.identifiers);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coordinator(
        key_capacity: usize,
        waiter_capacity: usize,
    ) -> Arc<BoundedCoordinator<usize, &'static str>> {
        BoundedCoordinator::new(
            CoordinationLimits {
                key_capacity,
                waiter_capacity,
            },
            2,
            Duration::ZERO,
            "claim abandoned",
        )
        .unwrap()
    }

    fn windowed_coordinator(batch_size: usize) -> Arc<BoundedCoordinator<usize, &'static str>> {
        BoundedCoordinator::new(
            CoordinationLimits {
                key_capacity: batch_size * 4,
                waiter_capacity: batch_size * 4,
            },
            batch_size,
            // A window far longer than any test run: only the demand-aware
            // immediate flush (or a full batch) can claim inside it.
            Duration::from_secs(600),
            "claim abandoned",
        )
        .unwrap()
    }

    fn assert_settled(coordinator: &BoundedCoordinator<usize, &'static str>) {
        let snapshot = coordinator.snapshot();
        assert_eq!(snapshot.pending_keys, 0);
        assert_eq!(snapshot.in_flight_keys, 0);
        assert_eq!(snapshot.waiters, 0);
        assert_eq!(snapshot.retained_identifier_bytes, 0);
    }

    #[tokio::test]
    async fn quiescent_tail_set_flushes_without_waiting_the_window() {
        let coordinator = windowed_coordinator(4);
        let registrations = coordinator.register(&["tail".to_string()]).await.unwrap();

        // No claim is in flight and the pending key covers every currently
        // registered waiter, so the partial claim must flush immediately
        // instead of parking the tail set behind the 600s window.
        let claim = coordinator
            .claim()
            .expect("quiescent partial claim flushes immediately");
        assert_eq!(claim.identifiers().len(), 1);
        claim.finalize(vec![Ok(1)]);

        assert_eq!(
            registrations.into_iter().next().unwrap().receive().await,
            Ok(1)
        );
        assert_settled(&coordinator);
    }

    #[tokio::test]
    async fn overlapping_registrations_coalesce_toward_the_batch_size() {
        let coordinator = windowed_coordinator(4);

        let first = coordinator
            .register(&[
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
            ])
            .await
            .unwrap();
        let claim_one = coordinator.claim().expect("full batch is claimable");
        assert_eq!(claim_one.identifiers().len(), 4);

        // While the first claim is in flight, an overlapping registration
        // coalesces its duplicate key onto the in-flight entry and its new
        // keys must accumulate toward the next full batch instead of issuing
        // their own request.
        let second = coordinator
            .register(&["b".to_string(), "e".to_string(), "f".to_string()])
            .await
            .unwrap();
        assert!(
            coordinator.claim().is_none(),
            "partial pending set must wait while a claim is in flight"
        );

        let third = coordinator
            .register(&["g".to_string(), "h".to_string()])
            .await
            .unwrap();
        let claim_two = coordinator
            .claim()
            .expect("accumulated full batch flushes despite the in-flight claim");
        assert_eq!(claim_two.identifiers().len(), 4);

        claim_one.finalize(vec![Ok(1), Ok(2), Ok(3), Ok(4)]);
        claim_two.finalize(vec![Ok(5), Ok(6), Ok(7), Ok(8)]);

        let mut outcomes = Vec::new();
        for registration in first.into_iter().chain(second).chain(third) {
            outcomes.push(registration.receive().await.unwrap());
        }

        // 9 waiters were served by 2 claims: the coalesced "b" waiter
        // received the in-flight claim's outcome. Fill efficiency is at the
        // maximum possible for this demand (2 requests for 8 unique keys
        // against a batch size of 4), not one request per registration.
        assert_eq!(outcomes, [1, 2, 3, 4, 2, 5, 6, 7, 8]);
        assert_eq!(
            coordinator.snapshot().coalesced_waiters_total,
            1,
            "the duplicate key registration must coalesce"
        );
        assert_settled(&coordinator);
    }

    #[tokio::test]
    async fn simultaneous_drop_of_waiters_and_claim_settles_all_ownership() {
        let coordinator = windowed_coordinator(2);
        let first = coordinator
            .register(&["one".to_string(), "two".to_string()])
            .await
            .unwrap();
        let claim = coordinator.claim().expect("full claim");
        let second = coordinator.register(&["three".to_string()]).await.unwrap();

        // Whole-batch cancellation: dropping the combined future takes the
        // in-flight claim and every waiter registration together.
        drop(first);
        drop(claim);
        drop(second);

        assert_settled(&coordinator);
    }

    #[tokio::test]
    async fn duplicate_registration_coalesces_one_key_and_delivers_to_each_waiter() {
        let coordinator = coordinator(2, 4);
        let registrations = coordinator
            .register(&["same".to_string(), "same".to_string()])
            .await
            .unwrap();
        let claim = coordinator
            .claim()
            .expect("registered key should be claimable");
        claim.finalize(vec![Ok(7)]);
        let mut outcomes = Vec::new();
        for registration in registrations {
            outcomes.push(registration.receive().await.unwrap());
        }

        assert_eq!(outcomes, [7, 7]);
        assert_eq!(coordinator.snapshot().coalesced_waiters_total, 1);
    }

    #[tokio::test]
    async fn key_capacity_backpressures_until_a_terminal_completion() {
        let coordinator = coordinator(2, 4);
        let first = coordinator
            .register(&["one".to_string(), "two".to_string()])
            .await
            .unwrap();
        let waiting_coordinator = Arc::clone(&coordinator);
        let blocked = tokio::spawn(async move {
            waiting_coordinator
                .register(&["three".to_string()])
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());

        coordinator
            .claim()
            .expect("first keys should be claimable")
            .finalize(vec![Ok(1), Ok(2)]);
        for registration in first {
            registration.receive().await.unwrap();
        }

        let third = blocked.await.unwrap();
        coordinator
            .claim()
            .expect("new key should be admitted after completion")
            .finalize(vec![Ok(3)]);
        assert_eq!(third.into_iter().next().unwrap().receive().await, Ok(3));
    }

    #[tokio::test]
    async fn waiter_capacity_backpressures_duplicate_fanout_without_an_overflow_queue() {
        let coordinator = coordinator(2, 2);
        let first = coordinator
            .register(&["same".to_string(), "same".to_string()])
            .await
            .unwrap();
        let waiting_coordinator = Arc::clone(&coordinator);
        let blocked = tokio::spawn(async move {
            waiting_coordinator
                .register(&["same".to_string()])
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());

        coordinator
            .claim()
            .expect("shared key should be claimable")
            .finalize(vec![Ok(9)]);
        for registration in first {
            registration.receive().await.unwrap();
        }

        let later = blocked.await.unwrap();
        coordinator
            .claim()
            .expect("duplicate arriving after completion starts new bounded work")
            .finalize(vec![Ok(10)]);
        assert_eq!(later.into_iter().next().unwrap().receive().await, Ok(10));
    }

    #[tokio::test]
    async fn terminal_failure_is_delivered_and_releases_all_capacity() {
        let coordinator = coordinator(2, 2);
        let registration = coordinator
            .register(&["one".to_string()])
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        coordinator
            .claim()
            .expect("key should be claimable")
            .finalize(vec![Err("upstream failure")]);

        assert_eq!(registration.receive().await, Err("upstream failure"));
        assert_eq!(coordinator.snapshot().waiters, 0);
        assert_eq!(coordinator.snapshot().retained_identifier_bytes, 0);
    }

    #[tokio::test]
    async fn waiter_drop_releases_waiter_capacity_and_pending_key() {
        let coordinator = coordinator(2, 2);
        let registrations = coordinator.register(&["one".to_string()]).await.unwrap();
        drop(registrations);

        assert_eq!(coordinator.snapshot().waiters, 0);
        assert_eq!(coordinator.snapshot().pending_keys, 0);
        assert_eq!(coordinator.snapshot().retained_identifier_bytes, 0);
    }

    #[tokio::test]
    async fn dropped_claim_wakes_waiters_and_recovers_all_capacity() {
        let coordinator = coordinator(2, 2);
        let registrations = coordinator
            .register(&["one".to_string(), "two".to_string()])
            .await
            .unwrap();
        let claim = coordinator.claim().expect("keys should be claimable");
        drop(claim);

        for registration in registrations {
            assert_eq!(registration.receive().await, Err("claim abandoned"));
        }
        let snapshot = coordinator.snapshot();
        assert_eq!(snapshot.pending_keys, 0);
        assert_eq!(snapshot.in_flight_keys, 0);
        assert_eq!(snapshot.waiters, 0);
        assert_eq!(snapshot.retained_identifier_bytes, 0);
    }

    #[tokio::test]
    async fn registration_larger_than_one_batch_is_rejected_for_chunking() {
        let coordinator = coordinator(4, 4);
        let error = match coordinator
            .register(&["one".to_string(), "two".to_string(), "three".to_string()])
            .await
        {
            Ok(_) => panic!("oversized registration should be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.requested, 3);
    }
}
