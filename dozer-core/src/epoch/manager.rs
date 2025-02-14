use dozer_types::log::info;
use dozer_types::node::{NodeHandle, SourceState, SourceStates};
use dozer_types::parking_lot::Mutex;
use std::ops::DerefMut;
use std::sync::{Arc, Barrier};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

use crate::checkpoint::{CheckpointFactory, CheckpointWriter};

use super::EpochCommonInfo;

#[derive(Debug)]
struct EpochManagerState {
    kind: EpochManagerStateKind,
    /// Initialized to 0.
    next_record_index_to_persist: usize,
    /// The instant when epoch manager decided to persist the last epoch. Initialized to the epoch manager's start time.
    last_persisted_epoch_decision_instant: SystemTime,
}

#[derive(Debug)]
enum EpochManagerStateKind {
    Closing {
        /// Current epoch id.
        epoch_id: u64,
        /// Whether we should tell the sources to terminate when this epoch closes.
        should_terminate: bool,
        /// Whether we should tell the sources to commit when this epoch closes.
        should_commit: bool,
        /// The collected source states.
        source_states: SourceStates,
        /// Sources wait on this barrier to synchronize an epoch close.
        barrier: Arc<Barrier>,
    },
    Closed {
        /// Whether sources should terminate.
        terminating: bool,
        /// The action to take, commit, commit and persist, or nothing.
        action: Action,
        /// Closed epoch id.
        epoch_id: u64,
        /// Collected source states.
        source_states: Arc<SourceStates>,
        /// Instant when the epoch was closed.
        instant: SystemTime,
        /// Number of sources that have confirmed the epoch close.
        num_source_confirmations: usize,
    },
}

impl EpochManagerStateKind {
    fn epoch_id(&self) -> u64 {
        match self {
            EpochManagerStateKind::Closing { epoch_id, .. }
            | EpochManagerStateKind::Closed { epoch_id, .. } => *epoch_id,
        }
    }
}

#[derive(Debug)]
enum Action {
    Commit,
    CommitAndPersist,
    Nothing,
}

impl Action {
    fn should_commit(&self) -> bool {
        matches!(self, Action::Commit | Action::CommitAndPersist)
    }

    fn should_persist(&self) -> bool {
        matches!(self, Action::CommitAndPersist)
    }
}

impl EpochManagerStateKind {
    fn new_closing(epoch_id: u64, num_sources: usize) -> EpochManagerStateKind {
        EpochManagerStateKind::Closing {
            epoch_id,
            should_terminate: true,
            should_commit: false,
            source_states: Default::default(),
            barrier: Arc::new(Barrier::new(num_sources)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EpochManagerOptions {
    pub max_num_records_before_persist: usize,
    pub max_interval_before_persist_in_seconds: u64,
    pub enable_app_checkpoints: bool,
}

impl Default for EpochManagerOptions {
    fn default() -> Self {
        Self {
            max_num_records_before_persist: 100_000,
            max_interval_before_persist_in_seconds: 60,
            enable_app_checkpoints: false,
        }
    }
}

#[derive(Debug)]
pub struct EpochManager {
    num_sources: usize,
    checkpoint_factory: Arc<CheckpointFactory>,
    options: EpochManagerOptions,
    state: Mutex<EpochManagerState>,
}

#[derive(Debug, Clone)]
/// When all sources agrees on closing an epoch, the `EpochManager` will make decisions on how to close this epoch and return this struct.
pub struct ClosedEpoch {
    pub should_terminate: bool,
    /// `Some` if the epoch should be committed.
    pub common_info: Option<EpochCommonInfo>,
    pub decision_instant: SystemTime,
}

impl EpochManager {
    pub fn new(
        num_sources: usize,
        epoch_id: u64,
        checkpoint_factory: Arc<CheckpointFactory>,
        options: EpochManagerOptions,
    ) -> Self {
        debug_assert!(num_sources > 0);
        let next_record_index_to_persist = 0;
        Self {
            num_sources,
            checkpoint_factory,
            options,
            state: Mutex::new(EpochManagerState {
                kind: EpochManagerStateKind::new_closing(epoch_id, num_sources),
                next_record_index_to_persist,
                last_persisted_epoch_decision_instant: SystemTime::now(),
            }),
        }
    }

    pub fn epoch_id(&self) -> u64 {
        self.state.lock().kind.epoch_id()
    }

    /// Waits for the epoch to close until all sources do so.
    ///
    /// Returns whether the participant should terminate, the epoch id if the source should commit, and the instant when the decision was made.
    ///
    /// # Arguments
    ///
    /// - `request_termination`: Whether the source wants to terminate. The `EpochManager` checks if all sources want to terminate and returns `true` if so.
    /// - `request_commit`: Whether the source wants to commit. The `EpochManager` checks if any source wants to commit and returns `Some` if so.
    pub fn wait_for_epoch_close(
        &self,
        source_state: (NodeHandle, SourceState),
        request_termination: bool,
        request_commit: bool,
    ) -> ClosedEpoch {
        let barrier = loop {
            let mut state = self.state.lock();
            match &mut state.kind {
                EpochManagerStateKind::Closing {
                    should_terminate,
                    should_commit,
                    source_states,
                    barrier,
                    ..
                } => {
                    // If anyone doesn't want to terminate, we don't terminate.
                    *should_terminate = *should_terminate && request_termination;
                    // If anyone wants to commit, we commit.
                    *should_commit = *should_commit || request_commit;
                    // Collect source states.
                    source_states.insert(source_state.0, source_state.1);
                    break barrier.clone();
                }
                EpochManagerStateKind::Closed { .. } => {
                    // This thread wants to close a new epoch while some other thread hasn't got confirmation of last epoch closing.
                    // Just release the lock and put this thread to sleep.
                    drop(state);
                    sleep(Duration::from_millis(1));
                }
            }
        };

        barrier.wait();

        let mut state = self.state.lock();
        let state = state.deref_mut();
        if let EpochManagerStateKind::Closing {
            epoch_id,
            should_terminate,
            should_commit,
            source_states,
            ..
        } = &mut state.kind
        {
            let instant = SystemTime::now();
            let action = if *should_commit {
                let num_records = 0;
                if num_records - state.next_record_index_to_persist
                    >= self.options.max_num_records_before_persist
                    || instant
                        .duration_since(state.last_persisted_epoch_decision_instant)
                        .unwrap_or(Duration::from_secs(0))
                        >= Duration::from_secs(self.options.max_interval_before_persist_in_seconds)
                {
                    state.next_record_index_to_persist = num_records;
                    state.last_persisted_epoch_decision_instant = instant;
                    info!(
                        "Persisting epoch {}, source states: {:?}",
                        epoch_id, source_states
                    );
                    Action::CommitAndPersist
                } else {
                    Action::Commit
                }
            } else {
                Action::Nothing
            };

            state.kind = EpochManagerStateKind::Closed {
                terminating: *should_terminate,
                action,
                epoch_id: *epoch_id,
                source_states: Arc::new(std::mem::take(source_states)),
                instant,
                num_source_confirmations: 0,
            };
        }

        match &mut state.kind {
            EpochManagerStateKind::Closed {
                terminating,
                action,
                epoch_id,
                source_states,
                instant,
                num_source_confirmations,
            } => {
                let common_info = action.should_commit().then(|| {
                    let checkpoint_writer = (action.should_persist()
                        && self.options.enable_app_checkpoints
                        && is_restartable(source_states))
                    .then(|| {
                        Arc::new(CheckpointWriter::new(
                            self.checkpoint_factory.clone(),
                            *epoch_id,
                        ))
                    });
                    let sink_persist_queue = action
                        .should_persist()
                        .then(|| self.checkpoint_factory.queue().clone());
                    EpochCommonInfo {
                        id: *epoch_id,
                        checkpoint_writer,
                        sink_persist_queue,
                        source_states: source_states.clone(),
                    }
                });

                let result = ClosedEpoch {
                    should_terminate: *terminating,
                    common_info,
                    decision_instant: *instant,
                };

                *num_source_confirmations += 1;
                if *num_source_confirmations == self.num_sources {
                    // This thread is the last one in this critical area.
                    state.kind = EpochManagerStateKind::new_closing(
                        if action.should_commit() {
                            *epoch_id + 1
                        } else {
                            *epoch_id
                        },
                        self.num_sources,
                    );
                }

                result
            }
            EpochManagerStateKind::Closing { .. } => {
                unreachable!("We just modified `EpochManagerState` to `Closed`")
            }
        }
    }
}

fn is_restartable(source_states: &SourceStates) -> bool {
    source_states
        .values()
        .all(|source_state| source_state != &SourceState::NonRestartable)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ops::Deref, thread::scope};

    use dozer_log::tokio;
    use tempdir::TempDir;

    use crate::checkpoint::create_checkpoint_factory_for_test;

    use super::*;

    const NUM_THREADS: u16 = 10;

    async fn create_epoch_manager(
        num_sources: usize,
        options: EpochManagerOptions,
    ) -> (TempDir, EpochManager) {
        let (temp_dir, checkpoint_factory, _) = create_checkpoint_factory_for_test().await;

        let epoch_manager = EpochManager::new(num_sources, 0, checkpoint_factory, options);

        (temp_dir, epoch_manager)
    }

    fn run_epoch_manager(
        epoch_manager: &EpochManager,
        termination_gen: &(impl Fn(u16) -> bool + Sync),
        commit_gen: &(impl Fn(u16) -> bool + Sync),
        source_state_gen: &(impl Fn(u16) -> (NodeHandle, SourceState) + Sync),
    ) -> ClosedEpoch {
        scope(|scope| {
            let handles = (0..NUM_THREADS)
                .map(|index| {
                    scope.spawn(move || {
                        epoch_manager.wait_for_epoch_close(
                            source_state_gen(index),
                            termination_gen(index),
                            commit_gen(index),
                        )
                    })
                })
                .collect::<Vec<_>>();
            let results = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>();

            let first = results.first().unwrap();
            for result in &results {
                assert_eq!(result.should_terminate, first.should_terminate);
                assert_eq!(result.common_info.is_some(), first.common_info.is_some());
                if let Some(common_info) = &result.common_info {
                    assert_eq!(common_info.id, first.common_info.as_ref().unwrap().id);
                }
                assert_eq!(result.decision_instant, first.decision_instant);
            }
            results.into_iter().next().unwrap()
        })
    }

    fn generate_source_state(index: u16) -> (NodeHandle, SourceState) {
        (
            NodeHandle::new(Some(index), index.to_string()),
            SourceState::NotStarted,
        )
    }

    #[tokio::test]
    async fn test_epoch_manager() {
        let (_temp_dir, epoch_manager) =
            create_epoch_manager(NUM_THREADS as usize, Default::default()).await;

        // All sources have no new data, epoch should not be closed.
        let ClosedEpoch { common_info, .. } = run_epoch_manager(
            &epoch_manager,
            &|_| false,
            &|_| false,
            &generate_source_state,
        );
        assert!(common_info.is_none());

        // One source has new data, epoch should be closed.
        let ClosedEpoch { common_info, .. } = run_epoch_manager(
            &epoch_manager,
            &|_| false,
            &|index| index == 0,
            &generate_source_state,
        );
        let common_info = common_info.unwrap();
        assert_eq!(common_info.id, 0);
        assert_eq!(
            common_info.source_states.deref(),
            &(0..NUM_THREADS)
                .map(generate_source_state)
                .collect::<HashMap<_, _>>()
        );

        // All but one source requests termination, should not terminate.
        let ClosedEpoch {
            should_terminate, ..
        } = run_epoch_manager(
            &epoch_manager,
            &|index| index != 0,
            &|_| false,
            &generate_source_state,
        );
        assert!(!should_terminate);

        // All sources requests termination, should terminate.
        let ClosedEpoch {
            should_terminate, ..
        } = run_epoch_manager(
            &epoch_manager,
            &|_| true,
            &|_| false,
            &generate_source_state,
        );
        assert!(should_terminate);
    }

    #[tokio::test]
    async fn test_epoch_manager_persist_message() {
        let (_temp_dir, epoch_manager) = create_epoch_manager(
            1,
            EpochManagerOptions {
                max_num_records_before_persist: 1,
                max_interval_before_persist_in_seconds: 1,
                enable_app_checkpoints: true,
            },
        )
        .await;

        // Epoch manager must be used from non-tokio threads.
        let source_state = generate_source_state(0);
        std::thread::spawn(move || {
            // No record, no persist.
            let epoch = epoch_manager.wait_for_epoch_close(source_state.clone(), false, true);
            let common_info = epoch.common_info.unwrap();
            assert!(common_info.checkpoint_writer.is_none());
            assert!(common_info.sink_persist_queue.is_none());

            // One record, persist.
            let epoch = epoch_manager.wait_for_epoch_close(source_state.clone(), false, true);
            let common_info = epoch.common_info.unwrap();
            assert!(common_info.checkpoint_writer.is_some());
            assert!(common_info.sink_persist_queue.is_some());

            // Time passes, persist.
            std::thread::sleep(Duration::from_secs(1));
            let epoch = epoch_manager.wait_for_epoch_close(source_state.clone(), false, true);
            let common_info = epoch.common_info.unwrap();
            assert!(common_info.checkpoint_writer.is_some());
            assert!(common_info.sink_persist_queue.is_some());
        })
        .join()
        .unwrap();

        // Also test the case where checkpoints are disabled.
        let (_temp_dir, epoch_manager) = create_epoch_manager(
            1,
            EpochManagerOptions {
                max_num_records_before_persist: 1,
                max_interval_before_persist_in_seconds: 1,
                enable_app_checkpoints: false,
            },
        )
        .await;

        let source_state = generate_source_state(0);
        std::thread::spawn(move || {
            // No record, no persist.
            let epoch = epoch_manager.wait_for_epoch_close(source_state.clone(), false, true);
            let common_info = epoch.common_info.unwrap();
            assert!(common_info.checkpoint_writer.is_none());
            assert!(common_info.sink_persist_queue.is_none());

            // One record, persist.
            let epoch = epoch_manager.wait_for_epoch_close(source_state.clone(), false, true);
            let common_info = epoch.common_info.unwrap();
            assert!(common_info.checkpoint_writer.is_none());
            assert!(common_info.sink_persist_queue.is_some());

            // Time passes, persist.
            std::thread::sleep(Duration::from_secs(1));
            let epoch = epoch_manager.wait_for_epoch_close(source_state.clone(), false, true);
            let common_info = epoch.common_info.unwrap();
            assert!(common_info.checkpoint_writer.is_none());
            assert!(common_info.sink_persist_queue.is_some());
        })
        .join()
        .unwrap();
    }
}
