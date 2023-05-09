//! The calibration stage. The fuzzer measures the average exec time and the bitmap size.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{fmt::Debug, marker::PhantomData, time::Duration};

use hashbrown::HashSet;
use num_traits::Bounded;
use serde::{Deserialize, Serialize};

use crate::{
    bolts::{current_time, tuples::Named, AsIter},
    corpus::{Corpus, CorpusId, SchedulerTestcaseMetadata},
    events::{Event, EventFirer, LogSeverity},
    executors::{Executor, ExitKind, HasObservers},
    feedbacks::{map::MapFeedbackMetadata, HasObserverName},
    fuzzer::Evaluator,
    inputs::UsesInput,
    monitors::UserStats,
    observers::{MapObserver, ObserversTuple, UsesObserver},
    schedulers::powersched::SchedulerMetadata,
    stages::Stage,
    state::{HasClientPerfMonitor, HasCorpus, HasMetadata, HasNamedMetadata, UsesState},
    Error,
};

crate::impl_serdeany!(UnstableEntriesMetadata);
/// The metadata to keep unstable entries
/// In libafl, the stability is the number of the unstable entries divided by the size of the map
/// This is different from AFL++, which shows the number of the unstable entries divided by the number of filled entries.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UnstableEntriesMetadata {
    unstable_entries: HashSet<usize>,
    map_len: usize,
}

impl UnstableEntriesMetadata {
    #[must_use]
    /// Create a new [`struct@UnstableEntriesMetadata`]
    pub fn new(entries: HashSet<usize>, map_len: usize) -> Self {
        Self {
            unstable_entries: entries,
            map_len,
        }
    }

    /// Getter
    #[must_use]
    pub fn unstable_entries(&self) -> &HashSet<usize> {
        &self.unstable_entries
    }

    /// Getter
    #[must_use]
    pub fn map_len(&self) -> usize {
        self.map_len
    }
}

/// The calibration stage will measure the average exec time and the target's stability for this input.
#[derive(Clone, Debug)]
pub struct CalibrationStage<O, OT, S> {
    map_observer_name: String,
    map_name: String,
    stage_max: usize,
    track_stability: bool,
    phantom: PhantomData<(O, OT, S)>,
}

const CAL_STAGE_START: usize = 4; // AFL++'s CAL_CYCLES_FAST + 1
const CAL_STAGE_MAX: usize = 8; // AFL++'s CAL_CYCLES + 1

impl<O, OT, S> UsesState for CalibrationStage<O, OT, S>
where
    S: UsesInput,
{
    type State = S;
}

impl<E, EM, O, OT, Z> Stage<E, EM, Z> for CalibrationStage<O, OT, E::State>
where
    E: Executor<EM, Z> + HasObservers<Observers = OT>,
    EM: EventFirer<State = E::State>,
    O: MapObserver,
    for<'de> <O as MapObserver>::Entry: Serialize + Deserialize<'de> + 'static,
    OT: ObserversTuple<E::State>,
    E::State: HasCorpus + HasMetadata + HasClientPerfMonitor + HasNamedMetadata,
    Z: Evaluator<E, EM, State = E::State>,
{
    #[inline]
    #[allow(
        clippy::let_and_return,
        clippy::too_many_lines,
        clippy::cast_precision_loss
    )]
    fn perform(
        &mut self,
        fuzzer: &mut Z,
        executor: &mut E,
        state: &mut E::State,
        mgr: &mut EM,
        corpus_idx: CorpusId,
    ) -> Result<(), Error> {
        // Run this stage only once for each corpus entry and only if we haven't already inspected it
        {
            let corpus = state.corpus().get(corpus_idx)?.borrow();
            // println!("calibration; corpus.scheduled_count() : {}", corpus.scheduled_count());

            if corpus.scheduled_count() > 0 {
                return Ok(());
            }
        }

        let mut iter = self.stage_max;

        let input = state.corpus().cloned_input_for_id(corpus_idx)?;

        // Run once to get the initial calibration map
        executor.observers_mut().pre_exec_all(state, &input)?;

        let mut start = current_time();

        let exit_kind = executor.run_target(fuzzer, state, mgr, &input)?;
        let mut total_time = if exit_kind == ExitKind::Ok {
            current_time() - start
        } else {
            mgr.log(
                state,
                LogSeverity::Warn,
                "Corpus entry errored on execution!".into(),
            )?;
            // assume one second as default time
            Duration::from_secs(1)
        };

        executor
            .observers_mut()
            .post_exec_all(state, &input, &exit_kind)?;

        let map_first = &executor
            .observers()
            .match_name::<O>(&self.map_observer_name)
            .ok_or_else(|| Error::key_not_found("MapObserver not found".to_string()))?
            .to_vec();

        let mut unstable_entries: Vec<usize> = vec![];
        let map_len: usize = map_first.len();
        // Run CAL_STAGE_START - 1 times, increase by 2 for every time a new
        // run is found to be unstable or to crash with CAL_STAGE_MAX total runs.
        let mut i = 1;
        let mut has_errors = false;

        while i < iter {
            let input = state.corpus().cloned_input_for_id(corpus_idx)?;

            executor.observers_mut().pre_exec_all(state, &input)?;
            start = current_time();

            let exit_kind = executor.run_target(fuzzer, state, mgr, &input)?;
            if exit_kind != ExitKind::Ok {
                if !has_errors {
                    mgr.log(
                        state,
                        LogSeverity::Warn,
                        "Corpus entry errored on execution!".into(),
                    )?;

                    has_errors = true;
                }

                if iter < CAL_STAGE_MAX {
                    iter += 2;
                };
            };

            total_time += current_time() - start;

            executor
                .observers_mut()
                .post_exec_all(state, &input, &exit_kind)?;

            if self.track_stability {
                let map = &executor
                    .observers()
                    .match_name::<O>(&self.map_observer_name)
                    .ok_or_else(|| Error::key_not_found("MapObserver not found".to_string()))?
                    .to_vec();

                let history_map = &mut state
                    .named_metadata_map_mut()
                    .get_mut::<MapFeedbackMetadata<O::Entry>>(&self.map_name)
                    .unwrap()
                    .history_map;

                if history_map.len() < map_len {
                    history_map.resize(map_len, O::Entry::default());
                }

                for (idx, (first, (cur, history))) in map_first
                    .iter()
                    .zip(map.iter().zip(history_map.iter_mut()))
                    .enumerate()
                {
                    if *first != *cur && *history != O::Entry::max_value() {
                        *history = O::Entry::max_value();
                        unstable_entries.push(idx);
                    };
                }

                if !unstable_entries.is_empty() && iter < CAL_STAGE_MAX {
                    iter += 2;
                }
            }
            i += 1;
        }

        if !unstable_entries.is_empty() {
            // If we see new stable entries executing this new corpus entries, then merge with the existing one
            if state.has_metadata::<UnstableEntriesMetadata>() {
                let existing = state
                    .metadata_map_mut()
                    .get_mut::<UnstableEntriesMetadata>()
                    .unwrap();
                for item in unstable_entries {
                    existing.unstable_entries.insert(item); // Insert newly found items
                }
                existing.map_len = map_len;
            } else {
                state.add_metadata::<UnstableEntriesMetadata>(UnstableEntriesMetadata::new(
                    HashSet::from_iter(unstable_entries),
                    map_len,
                ));
            }
        };

        // If weighted scheduler or powerscheduler is used, update it
        if state.has_metadata::<SchedulerMetadata>() {
            let map = executor
                .observers()
                .match_name::<O>(&self.map_observer_name)
                .ok_or_else(|| Error::key_not_found("MapObserver not found".to_string()))?;

            let bitmap_size = map.count_bytes();

            let psmeta = state
                .metadata_map_mut()
                .get_mut::<SchedulerMetadata>()
                .unwrap();
            let handicap = psmeta.queue_cycles();

            psmeta.set_exec_time(psmeta.exec_time() + total_time);
            psmeta.set_cycles(psmeta.cycles() + (iter as u64));
            psmeta.set_bitmap_size(psmeta.bitmap_size() + bitmap_size);
            psmeta.set_bitmap_size_log(psmeta.bitmap_size_log() + libm::log2(bitmap_size as f64));
            psmeta.set_bitmap_entries(psmeta.bitmap_entries() + 1);

            let mut testcase = state.corpus().get(corpus_idx)?.borrow_mut();

            testcase.set_exec_time(total_time / (iter as u32));
            // log::trace!("time: {:#?}", testcase.exec_time());

            // If the testcase doesn't have its own `SchedulerTestcaseMetadata`, create it.
            let data = if let Ok(metadata) = testcase.metadata_mut::<SchedulerTestcaseMetadata>() {
                metadata
            } else {
                let depth = if let Some(parent_id) = testcase.parent_id() {
                    if let Some(parent_metadata) = (*state.corpus().get(parent_id)?)
                        .borrow()
                        .metadata_map()
                        .get::<SchedulerTestcaseMetadata>()
                    {
                        parent_metadata.depth() + 1
                    } else {
                        0
                    }
                } else {
                    0
                };
                testcase.add_metadata(SchedulerTestcaseMetadata::new(depth));
                testcase
                    .metadata_mut::<SchedulerTestcaseMetadata>()
                    .unwrap()
            };

            data.set_cycle_and_time((total_time, iter));
            data.set_bitmap_size(bitmap_size);
            data.set_handicap(handicap);
        }

        // Send the stability event to the broker
        if let Some(meta) = state.metadata_map().get::<UnstableEntriesMetadata>() {
            let unstable_entries = meta.unstable_entries().len();
            let map_len = meta.map_len();
            mgr.fire(
                state,
                Event::UpdateUserStats {
                    name: "stability".to_string(),
                    value: UserStats::Ratio((map_len - unstable_entries) as u64, map_len as u64),
                    phantom: PhantomData,
                },
            )?;
        }

        Ok(())
    }
}

impl<O, OT, S> CalibrationStage<O, OT, S>
where
    O: MapObserver,
    OT: ObserversTuple<S>,
    S: HasCorpus + HasMetadata + HasNamedMetadata,
{
    /// Create a new [`CalibrationStage`].
    #[must_use]
    pub fn new<F>(map_feedback: &F) -> Self
    where
        F: HasObserverName + Named + UsesObserver<S, Observer = O>,
        for<'it> O: AsIter<'it, Item = O::Entry>,
    {
        Self {
            map_observer_name: map_feedback.observer_name().to_string(),
            map_name: map_feedback.name().to_string(),
            stage_max: CAL_STAGE_START,
            track_stability: true,
            phantom: PhantomData,
        }
    }

    /// Create a new [`CalibrationStage`], but without checking stability.
    #[must_use]
    pub fn ignore_stability<F>(map_feedback: &F) -> Self
    where
        F: HasObserverName + Named + UsesObserver<S, Observer = O>,
        for<'it> O: AsIter<'it, Item = O::Entry>,
    {
        Self {
            map_observer_name: map_feedback.observer_name().to_string(),
            map_name: map_feedback.name().to_string(),
            stage_max: CAL_STAGE_START,
            track_stability: false,
            phantom: PhantomData,
        }
    }
}
