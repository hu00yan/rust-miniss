//! Bounded reference models for runtime safety protocols.
//!
//! These tests deliberately do not drive the real runtime. They keep the
//! normal test loop deterministic and fast while checking the state-machine
//! contracts that implementation-specific regression tests tend to miss.

use proptest::prelude::*;

const SHARD: u16 = 3;
const SLOT: u16 = 7;
const RUNTIME_GENERATION: u16 = 11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    runtime_generation: u16,
    shard: u16,
    slot: u16,
    generation: u16,
}

#[derive(Clone, Copy, Debug)]
enum TaskAction {
    Allocate,
    ShutdownCutoff,
    WakeCurrent,
    WakeStale,
    WrongRuntimeWake,
    WrongShardWake,
    Poll,
    CancelCurrent,
    CancelStale,
    WrongRuntimeCancel,
    WrongShardCancel,
    Complete,
    Cleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelOutcome {
    Accepted,
    AlreadyRequested,
    Stale,
    NotLive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdmissionOutcome {
    AcceptedBeforeCutoff,
    RejectedAfterCutoff,
}

#[derive(Debug, Default)]
struct TaskSlotModel {
    generation: u16,
    live: bool,
    running: bool,
    cancel_requested: bool,
    terminal: bool,
    ready_pending: bool,
    stale_key: Option<Key>,
    last_cancel_outcome: Option<CancelOutcome>,
    admission_closed: bool,
    last_admission_outcome: Option<AdmissionOutcome>,
}

impl TaskSlotModel {
    fn key(&self) -> Key {
        Key {
            runtime_generation: RUNTIME_GENERATION,
            shard: SHARD,
            slot: SLOT,
            generation: self.generation,
        }
    }

    fn wrong_runtime_key(&self) -> Key {
        Key {
            runtime_generation: RUNTIME_GENERATION.wrapping_add(1),
            ..self.key()
        }
    }

    fn wrong_shard_key(&self) -> Key {
        Key {
            shard: SHARD.wrapping_add(1),
            ..self.key()
        }
    }

    fn live_state(&self) -> (bool, bool, bool, bool, bool, u16) {
        (
            self.live,
            self.running,
            self.cancel_requested,
            self.terminal,
            self.ready_pending,
            self.generation,
        )
    }

    fn matches_live(&self, key: Key) -> bool {
        self.live && !self.terminal && key == self.key()
    }

    fn allocate(&mut self) {
        if self.admission_closed {
            self.last_admission_outcome = Some(AdmissionOutcome::RejectedAfterCutoff);
        } else if !self.live {
            self.last_admission_outcome = Some(AdmissionOutcome::AcceptedBeforeCutoff);
            self.live = true;
            self.running = false;
            self.cancel_requested = false;
            self.terminal = false;
            self.ready_pending = true;
            self.last_cancel_outcome = None;
        }
    }

    fn wake(&mut self, key: Key) {
        if self.matches_live(key) {
            // A bool models the coalesced notification: repeated wakes cannot
            // create more than one pending ready record.
            self.ready_pending = true;
        }
    }

    fn cancel(&mut self, key: Key) -> CancelOutcome {
        if self.matches_live(key) {
            if self.cancel_requested {
                return CancelOutcome::AlreadyRequested;
            }
            self.cancel_requested = true;
            self.ready_pending = true;
            CancelOutcome::Accepted
        } else if key.runtime_generation != RUNTIME_GENERATION
            || key.shard != SHARD
            || key.slot != SLOT
            || key.generation != self.generation
        {
            CancelOutcome::Stale
        } else {
            CancelOutcome::NotLive
        }
    }

    fn apply(&mut self, action: TaskAction) {
        self.last_cancel_outcome = None;
        self.last_admission_outcome = None;
        match action {
            TaskAction::Allocate => self.allocate(),
            TaskAction::ShutdownCutoff => self.admission_closed = true,
            TaskAction::WakeCurrent => self.wake(self.key()),
            TaskAction::WakeStale => {
                if let Some(key) = self.stale_key {
                    self.wake(key);
                }
            }
            TaskAction::WrongRuntimeWake => {
                let before = self.live_state();
                self.wake(self.wrong_runtime_key());
                assert_eq!(self.live_state(), before);
            }
            TaskAction::WrongShardWake => {
                let before = self.live_state();
                self.wake(self.wrong_shard_key());
                assert_eq!(self.live_state(), before);
            }
            TaskAction::Poll => {
                if self.live && !self.terminal && self.ready_pending {
                    self.ready_pending = false;
                    self.running = true;
                    if self.cancel_requested {
                        self.running = false;
                        self.terminal = true;
                    } else {
                        self.running = false;
                    }
                }
            }
            TaskAction::CancelCurrent => {
                self.last_cancel_outcome = Some(self.cancel(self.key()));
            }
            TaskAction::CancelStale => {
                if let Some(key) = self.stale_key {
                    self.last_cancel_outcome = Some(self.cancel(key));
                }
            }
            TaskAction::WrongRuntimeCancel => {
                let before = self.live_state();
                self.last_cancel_outcome = Some(self.cancel(self.wrong_runtime_key()));
                assert_eq!(self.last_cancel_outcome, Some(CancelOutcome::Stale));
                assert_eq!(self.live_state(), before);
            }
            TaskAction::WrongShardCancel => {
                let before = self.live_state();
                self.last_cancel_outcome = Some(self.cancel(self.wrong_shard_key()));
                assert_eq!(self.last_cancel_outcome, Some(CancelOutcome::Stale));
                assert_eq!(self.live_state(), before);
            }
            TaskAction::Complete => {
                if self.live && !self.terminal {
                    self.running = false;
                    self.ready_pending = false;
                    self.terminal = true;
                }
            }
            TaskAction::Cleanup => {
                if self.live && self.terminal {
                    self.stale_key = Some(self.key());
                    self.live = false;
                    self.running = false;
                    self.cancel_requested = false;
                    self.terminal = false;
                    self.ready_pending = false;
                    self.generation = self.generation.wrapping_add(1);
                }
            }
        }
    }

    fn check(&self) {
        assert!(!self.running || (self.live && !self.terminal));
        assert!(!self.ready_pending || (self.live && !self.terminal));
        assert!(!self.cancel_requested || self.live);
        assert!(!self.terminal || self.live);
        if self.last_cancel_outcome == Some(CancelOutcome::Accepted) {
            // Cancellation success is published only by this owner model after
            // it has performed the CancelRequested state transition.
            assert!(self.live && self.cancel_requested && self.ready_pending);
        }
        match self.last_admission_outcome {
            Some(AdmissionOutcome::AcceptedBeforeCutoff) => assert!(!self.admission_closed),
            Some(AdmissionOutcome::RejectedAfterCutoff) => assert!(self.admission_closed),
            None => {}
        }
        if let Some(stale) = self.stale_key {
            assert_eq!(stale.runtime_generation, RUNTIME_GENERATION);
            assert_eq!(stale.shard, SHARD);
            assert_eq!(stale.slot, SLOT);
            assert_ne!(stale.generation, self.generation);
        }
    }
}

fn task_action() -> impl Strategy<Value = TaskAction> {
    prop_oneof![
        Just(TaskAction::Allocate),
        Just(TaskAction::ShutdownCutoff),
        Just(TaskAction::WakeCurrent),
        Just(TaskAction::WakeStale),
        Just(TaskAction::WrongRuntimeWake),
        Just(TaskAction::WrongShardWake),
        Just(TaskAction::Poll),
        Just(TaskAction::CancelCurrent),
        Just(TaskAction::CancelStale),
        Just(TaskAction::WrongRuntimeCancel),
        Just(TaskAction::WrongShardCancel),
        Just(TaskAction::Complete),
        Just(TaskAction::Cleanup),
    ]
}

#[derive(Clone, Copy, Debug)]
enum OpAction {
    Allocate,
    Queue,
    RejectUnsupported,
    RejectSubmission,
    Submit,
    AbandonWaiter,
    RequestCancel,
    CompleteOk,
    CompleteCancelled,
    Deliver,
    Discard,
    Reclaim,
    InitializeByte,
    PublishByte,
    AcceptDescriptor,
    TransferAccepted,
    CloseAccepted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KernelState {
    Vacant,
    Allocated,
    Queued,
    Submitted,
    CancelRequested,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcceptedDescriptor {
    None,
    Owned,
    Transferred,
    Closed,
}

#[derive(Debug)]
struct OperationModel {
    generation: u16,
    kernel: KernelState,
    waiter_present: bool,
    terminal_result: bool,
    delivered: bool,
    discarded: bool,
    owns_descriptor: bool,
    owns_buffer: bool,
    owns_arguments: bool,
    token_exposed: bool,
    submitted_capacity: usize,
    initialized_len: usize,
    logical_len: usize,
    accepted_descriptor: AcceptedDescriptor,
    accepted_count: usize,
    transferred_count: usize,
    closed_count: usize,
}

impl Default for OperationModel {
    fn default() -> Self {
        Self {
            generation: 0,
            kernel: KernelState::Vacant,
            waiter_present: false,
            terminal_result: false,
            delivered: false,
            discarded: false,
            owns_descriptor: false,
            owns_buffer: false,
            owns_arguments: false,
            token_exposed: false,
            submitted_capacity: 0,
            initialized_len: 0,
            logical_len: 0,
            accepted_descriptor: AcceptedDescriptor::None,
            accepted_count: 0,
            transferred_count: 0,
            closed_count: 0,
        }
    }
}

impl OperationModel {
    fn apply(&mut self, action: OpAction) {
        match action {
            OpAction::Allocate if self.kernel == KernelState::Vacant => {
                self.kernel = KernelState::Allocated;
                self.waiter_present = true;
                self.owns_descriptor = true;
                self.owns_buffer = true;
                self.owns_arguments = true;
                self.submitted_capacity = 4;
            }
            OpAction::Queue if self.kernel == KernelState::Allocated => {
                self.kernel = KernelState::Queued;
                self.token_exposed = true;
            }
            OpAction::RejectUnsupported | OpAction::RejectSubmission
                if self.kernel == KernelState::Allocated
                    && self.accepted_descriptor == AcceptedDescriptor::None =>
            {
                // Rejection is pre-token and returns all internal ownership.
                self.kernel = KernelState::Vacant;
                self.waiter_present = false;
                self.owns_descriptor = false;
                self.owns_buffer = false;
                self.owns_arguments = false;
                self.submitted_capacity = 0;
                self.initialized_len = 0;
                self.logical_len = 0;
            }
            OpAction::Submit if self.kernel == KernelState::Queued => {
                self.kernel = KernelState::Submitted;
            }
            OpAction::AbandonWaiter
                if matches!(
                    self.kernel,
                    KernelState::Allocated
                        | KernelState::Queued
                        | KernelState::Submitted
                        | KernelState::CancelRequested
                ) =>
            {
                self.waiter_present = false;
            }
            OpAction::RequestCancel
                if matches!(self.kernel, KernelState::Queued | KernelState::Submitted) =>
            {
                self.kernel = KernelState::CancelRequested;
            }
            OpAction::CompleteOk
                if matches!(
                    self.kernel,
                    KernelState::Submitted | KernelState::CancelRequested
                ) =>
            {
                self.kernel = KernelState::Terminal;
                self.terminal_result = true;
            }
            OpAction::CompleteCancelled if self.kernel == KernelState::CancelRequested => {
                self.kernel = KernelState::Terminal;
                self.terminal_result = true;
            }
            OpAction::Deliver
                if self.kernel == KernelState::Terminal
                    && self.terminal_result
                    && self.waiter_present =>
            {
                self.delivered = true;
            }
            OpAction::Discard
                if self.kernel == KernelState::Terminal
                    && self.terminal_result
                    && !self.waiter_present =>
            {
                self.discarded = true;
            }
            OpAction::Reclaim
                if self.kernel == KernelState::Terminal
                    && (self.delivered || self.discarded)
                    && self.accepted_descriptor != AcceptedDescriptor::Owned =>
            {
                self.kernel = KernelState::Vacant;
                self.waiter_present = false;
                self.terminal_result = false;
                self.delivered = false;
                self.discarded = false;
                self.owns_descriptor = false;
                self.owns_buffer = false;
                self.owns_arguments = false;
                self.token_exposed = false;
                self.submitted_capacity = 0;
                self.initialized_len = 0;
                self.logical_len = 0;
                self.accepted_descriptor = AcceptedDescriptor::None;
                self.generation = self.generation.wrapping_add(1);
            }
            OpAction::InitializeByte
                if self.kernel != KernelState::Vacant
                    && self.initialized_len < self.submitted_capacity =>
            {
                self.initialized_len += 1;
            }
            OpAction::PublishByte
                if self.kernel == KernelState::Terminal
                    && self.logical_len < self.initialized_len =>
            {
                self.logical_len += 1;
            }
            OpAction::AcceptDescriptor
                if self.kernel != KernelState::Vacant
                    && self.accepted_descriptor == AcceptedDescriptor::None =>
            {
                self.accepted_descriptor = AcceptedDescriptor::Owned;
                self.accepted_count += 1;
            }
            OpAction::TransferAccepted if self.accepted_descriptor == AcceptedDescriptor::Owned => {
                self.accepted_descriptor = AcceptedDescriptor::Transferred;
                self.transferred_count += 1;
            }
            OpAction::CloseAccepted if self.accepted_descriptor == AcceptedDescriptor::Owned => {
                self.accepted_descriptor = AcceptedDescriptor::Closed;
                self.closed_count += 1;
            }
            _ => {}
        }
    }

    fn check(&self) {
        let live = self.kernel != KernelState::Vacant;
        assert_eq!(self.owns_descriptor, live);
        assert_eq!(self.owns_buffer, live);
        assert_eq!(self.owns_arguments, live);
        assert!(!self.token_exposed || live);
        assert!(self.logical_len <= self.initialized_len);
        assert!(self.initialized_len <= self.submitted_capacity);
        assert!(!self.terminal_result || self.kernel == KernelState::Terminal);
        assert!(!(self.delivered && self.discarded));
        assert!(!self.delivered || (self.terminal_result && self.waiter_present));
        assert!(!self.discarded || (self.terminal_result && !self.waiter_present));
        if matches!(
            self.kernel,
            KernelState::Queued | KernelState::Submitted | KernelState::CancelRequested
        ) {
            assert!(self.owns_descriptor && self.owns_buffer && self.owns_arguments);
        }
        if self.kernel == KernelState::Allocated {
            // Unsupported operations and submission rejection are representable
            // only before a public token exists.
            assert!(!self.token_exposed);
        }
        if self.accepted_descriptor != AcceptedDescriptor::None {
            assert!(live);
        }
        assert!(self.transferred_count + self.closed_count <= self.accepted_count);
        assert_eq!(
            self.accepted_count - self.transferred_count - self.closed_count,
            usize::from(self.accepted_descriptor == AcceptedDescriptor::Owned)
        );
    }

    fn owner_shutdown_finalize(&mut self) {
        if self.accepted_descriptor == AcceptedDescriptor::Owned {
            self.accepted_descriptor = AcceptedDescriptor::Closed;
            self.closed_count += 1;
        }
        if self.kernel != KernelState::Vacant {
            self.kernel = KernelState::Terminal;
            self.terminal_result = true;
            if self.waiter_present {
                self.delivered = true;
            } else {
                self.discarded = true;
            }
            self.apply(OpAction::Reclaim);
        }
        self.check();
        assert_eq!(self.kernel, KernelState::Vacant);
        assert!(!self.token_exposed);
        assert!(!self.owns_descriptor && !self.owns_buffer && !self.owns_arguments);
        assert_eq!(self.submitted_capacity, 0);
        assert_eq!(self.initialized_len, 0);
        assert_eq!(self.logical_len, 0);
        assert_eq!(self.accepted_descriptor, AcceptedDescriptor::None);
        assert_eq!(
            self.accepted_count,
            self.transferred_count + self.closed_count
        );
    }
}

fn op_action() -> impl Strategy<Value = OpAction> {
    prop_oneof![
        Just(OpAction::Allocate),
        Just(OpAction::Queue),
        Just(OpAction::RejectUnsupported),
        Just(OpAction::RejectSubmission),
        Just(OpAction::Submit),
        Just(OpAction::AbandonWaiter),
        Just(OpAction::RequestCancel),
        Just(OpAction::CompleteOk),
        Just(OpAction::CompleteCancelled),
        Just(OpAction::Deliver),
        Just(OpAction::Discard),
        Just(OpAction::Reclaim),
        Just(OpAction::InitializeByte),
        Just(OpAction::PublishByte),
        Just(OpAction::AcceptDescriptor),
        Just(OpAction::TransferAccepted),
        Just(OpAction::CloseAccepted),
    ]
}

/// A named replay case complements shrinking: it documents the shortest
/// lifecycle sequence that previously allowed a stale wake or token to affect
/// a replacement task.  Keep this deterministic case small so it remains a
/// useful smoke test when property tests are temporarily disabled.
#[test]
fn task_slot_stale_message_replay_is_harmless() {
    let mut model = TaskSlotModel::default();
    for action in [
        TaskAction::Allocate,
        TaskAction::Complete,
        TaskAction::Cleanup,
        TaskAction::Allocate,
        TaskAction::Poll,
        TaskAction::WakeStale,
        TaskAction::CancelStale,
    ] {
        model.apply(action);
        model.check();
    }
    assert!(model.live);
    assert_eq!(model.generation, 1);
    assert!(!model.cancel_requested);
    assert!(!model.ready_pending);
}

#[test]
fn operation_replay_reclaims_every_resource_after_waiter_abandonment() {
    let mut model = OperationModel::default();
    for action in [
        OpAction::Allocate,
        OpAction::Queue,
        OpAction::Submit,
        OpAction::AbandonWaiter,
        OpAction::RequestCancel,
        OpAction::CompleteCancelled,
        OpAction::Discard,
        OpAction::Reclaim,
    ] {
        model.apply(action);
        model.check();
    }
    model.owner_shutdown_finalize();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn task_slot_actions_preserve_identity_and_ready_invariants(
        actions in prop::collection::vec(task_action(), 0..32)
    ) {
        let mut model = TaskSlotModel::default();
        for action in actions {
            model.apply(action);
            model.check();
        }
    }

    #[test]
    fn operation_actions_retain_resources_until_terminal_reclamation(
        actions in prop::collection::vec(op_action(), 0..32)
    ) {
        let mut model = OperationModel::default();
        for action in actions {
            model.apply(action);
            model.check();
        }
        model.owner_shutdown_finalize();
    }
}
