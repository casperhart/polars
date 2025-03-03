use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use crossbeam_queue::ArrayQueue;
use polars_core::prelude::*;
use polars_core::schema::{Schema, SchemaExt};
use polars_core::series::IsSorted;
use polars_core::utils::accumulate_dataframes_vertical_unchecked;
use polars_core::{config, POOL};
use polars_expr::chunked_idx_table::{new_chunked_idx_table, ChunkedIdxTable};
use polars_expr::hash_keys::HashKeys;
use polars_io::pl_async::get_runtime;
use polars_ops::frame::{JoinArgs, JoinType, MaintainOrderJoin};
use polars_ops::prelude::TakeChunked;
use polars_ops::series::coalesce_columns;
use polars_utils::cardinality_sketch::CardinalitySketch;
use polars_utils::hashing::HashPartitioner;
use polars_utils::itertools::Itertools;
use polars_utils::pl_str::PlSmallStr;
use polars_utils::{format_pl_smallstr, IdxSize};
use rayon::prelude::*;

use crate::async_primitives::connector::{connector, Receiver, Sender};
use crate::async_primitives::wait_group::WaitGroup;
use crate::expression::StreamExpr;
use crate::morsel::{get_ideal_morsel_size, SourceToken};
use crate::nodes::compute_node_prelude::*;
use crate::nodes::in_memory_source::InMemorySourceNode;

static SAMPLE_LIMIT: LazyLock<usize> = LazyLock::new(|| {
    std::env::var("POLARS_JOIN_SAMPLE_LIMIT")
        .map(|limit| limit.parse().unwrap())
        .unwrap_or(10_000_000)
});

// If one side is this much bigger than the other side we'll always use the
// smaller side as the build side without checking cardinalities.
const LOPSIDED_SAMPLE_FACTOR: usize = 10;

/// A payload selector contains for each column whether that column should be
/// included in the payload, and if yes with what name.
fn compute_payload_selector(
    this: &Schema,
    other: &Schema,
    this_key_schema: &Schema,
    is_left: bool,
    args: &JoinArgs,
) -> PolarsResult<Vec<Option<PlSmallStr>>> {
    let should_coalesce = args.should_coalesce();

    this.iter_names()
        .enumerate()
        .map(|(i, c)| {
            let selector = if should_coalesce && this_key_schema.contains(c) {
                if is_left != (args.how == JoinType::Right) {
                    Some(c.clone())
                } else if args.how == JoinType::Full {
                    // We must keep the right-hand side keycols around for
                    // coalescing.
                    Some(format_pl_smallstr!("__POLARS_COALESCE_KEYCOL{i}"))
                } else {
                    None
                }
            } else if !other.contains(c) || is_left {
                Some(c.clone())
            } else {
                let suffixed = format_pl_smallstr!("{}{}", c, args.suffix());
                if other.contains(&suffixed) {
                    polars_bail!(Duplicate: "column with name '{suffixed}' already exists\n\n\
                    You may want to try:\n\
                    - renaming the column prior to joining\n\
                    - using the `suffix` parameter to specify a suffix different to the default one ('_right')")
                }
                Some(suffixed)
            };
            Ok(selector)
        })
        .collect()
}

/// Fixes names and does coalescing of columns post-join.
fn postprocess_join(df: DataFrame, params: &EquiJoinParams) -> DataFrame {
    if params.args.how == JoinType::Full && params.args.should_coalesce() {
        // TODO: don't do string-based column lookups for each dataframe, pre-compute coalesce indices.
        let mut key_idx = 0;
        df.get_columns()
            .iter()
            .filter_map(|c| {
                if let Some((key_name, _)) = params.left_key_schema.get_at_index(key_idx) {
                    if c.name() == key_name {
                        let other = df
                            .column(&format_pl_smallstr!("__POLARS_COALESCE_KEYCOL{key_idx}"))
                            .unwrap();
                        key_idx += 1;
                        return Some(coalesce_columns(&[c.clone(), other.clone()]).unwrap());
                    }
                }

                if c.name().starts_with("__POLARS_COALESCE_KEYCOL") {
                    return None;
                }

                Some(c.clone())
            })
            .collect()
    } else {
        df
    }
}

fn select_schema(schema: &Schema, selector: &[Option<PlSmallStr>]) -> Schema {
    schema
        .iter_fields()
        .zip(selector)
        .filter_map(|(f, name)| Some(f.with_name(name.clone()?)))
        .collect()
}

async fn select_keys(
    df: &DataFrame,
    key_selectors: &[StreamExpr],
    params: &EquiJoinParams,
    state: &ExecutionState,
) -> PolarsResult<HashKeys> {
    let mut key_columns = Vec::new();
    for (i, selector) in key_selectors.iter().enumerate() {
        // We use key columns entirely by position, and allow duplicate names,
        // so just assign arbitrary unique names.
        let unique_name = format_pl_smallstr!("__POLARS_KEYCOL_{i}");
        let s = selector.evaluate(df, state).await?;
        key_columns.push(s.into_column().with_name(unique_name));
    }
    let keys = DataFrame::new_with_broadcast_len(key_columns, df.height())?;
    Ok(HashKeys::from_df(
        &keys,
        params.random_state.clone(),
        params.args.nulls_equal,
        true,
    ))
}

fn select_payload(df: DataFrame, selector: &[Option<PlSmallStr>]) -> DataFrame {
    // Maintain height of zero-width dataframes.
    if df.width() == 0 {
        return df;
    }

    df.take_columns()
        .into_iter()
        .zip(selector)
        .filter_map(|(c, name)| Some(c.with_name(name.clone()?)))
        .collect()
}

fn estimate_cardinality(
    morsels: &[Morsel],
    key_selectors: &[StreamExpr],
    params: &EquiJoinParams,
    state: &ExecutionState,
) -> PolarsResult<usize> {
    // TODO: parallelize.
    let mut sketch = CardinalitySketch::new();
    for morsel in morsels {
        let hash_keys =
            get_runtime().block_on(select_keys(morsel.df(), key_selectors, params, state))?;
        hash_keys.sketch_cardinality(&mut sketch);
    }
    Ok(sketch.estimate())
}

struct BufferedStream {
    morsels: ArrayQueue<Morsel>,
    post_buffer_offset: MorselSeq,
}

impl BufferedStream {
    pub fn new(morsels: Vec<Morsel>, start_offset: MorselSeq) -> Self {
        // Relabel so we can insert into parallel streams later.
        let mut seq = start_offset;
        let queue = ArrayQueue::new(morsels.len().max(1));
        for mut morsel in morsels {
            morsel.set_seq(seq);
            queue.push(morsel).unwrap();
            seq = seq.successor();
        }

        Self {
            morsels: queue,
            post_buffer_offset: seq,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.morsels.is_empty()
    }

    #[expect(clippy::needless_lifetimes)]
    pub fn reinsert<'s, 'env>(
        &'s self,
        num_pipelines: usize,
        recv_port: Option<RecvPort<'_>>,
        scope: &'s TaskScope<'s, 'env>,
        join_handles: &mut Vec<JoinHandle<PolarsResult<()>>>,
    ) -> Option<Vec<Receiver<Morsel>>> {
        let receivers = if let Some(p) = recv_port {
            p.parallel().into_iter().map(Some).collect_vec()
        } else {
            (0..num_pipelines).map(|_| None).collect_vec()
        };

        let source_token = SourceToken::new();
        let mut out = Vec::new();
        for orig_recv in receivers {
            let (mut new_send, new_recv) = connector();
            out.push(new_recv);
            let source_token = source_token.clone();
            join_handles.push(scope.spawn_task(TaskPriority::High, async move {
                // Act like an InMemorySource node until cached morsels are consumed.
                let wait_group = WaitGroup::default();
                loop {
                    let Some(mut morsel) = self.morsels.pop() else {
                        break;
                    };
                    morsel.replace_source_token(source_token.clone());
                    morsel.set_consume_token(wait_group.token());
                    if new_send.send(morsel).await.is_err() {
                        return Ok(());
                    }
                    wait_group.wait().await;
                    // TODO: Unfortunately we can't actually stop here without
                    // re-buffering morsels from the stream that comes after.
                    // if source_token.stop_requested() {
                    //     break;
                    // }
                }

                if let Some(mut recv) = orig_recv {
                    while let Ok(mut morsel) = recv.recv().await {
                        if source_token.stop_requested() {
                            morsel.source_token().stop();
                        }
                        morsel.set_seq(morsel.seq().offset_by(self.post_buffer_offset));
                        if new_send.send(morsel).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(())
            }));
        }
        Some(out)
    }
}

impl Default for BufferedStream {
    fn default() -> Self {
        Self {
            morsels: ArrayQueue::new(1),
            post_buffer_offset: MorselSeq::default(),
        }
    }
}

impl Drop for BufferedStream {
    fn drop(&mut self) {
        POOL.install(|| {
            // Parallel drop as the state might be quite big.
            (0..self.morsels.len())
                .into_par_iter()
                .for_each(|_| drop(self.morsels.pop()));
        })
    }
}

#[derive(Default)]
struct SampleState {
    left: Vec<Morsel>,
    left_len: usize,
    right: Vec<Morsel>,
    right_len: usize,
}

impl SampleState {
    async fn sink(
        mut recv: Receiver<Morsel>,
        morsels: &mut Vec<Morsel>,
        len: &mut usize,
        this_final_len: Arc<AtomicUsize>,
        other_final_len: Arc<AtomicUsize>,
    ) -> PolarsResult<()> {
        while let Ok(mut morsel) = recv.recv().await {
            *len += morsel.df().height();
            if *len >= *SAMPLE_LIMIT
                || *len
                    >= other_final_len
                        .load(Ordering::Relaxed)
                        .saturating_mul(LOPSIDED_SAMPLE_FACTOR)
            {
                morsel.source_token().stop();
            }

            drop(morsel.take_consume_token());
            morsels.push(morsel);
        }
        this_final_len.store(*len, Ordering::Relaxed);
        Ok(())
    }

    fn try_transition_to_build(
        &mut self,
        recv: &[PortState],
        num_pipelines: usize,
        params: &mut EquiJoinParams,
        table: &mut Option<Box<dyn ChunkedIdxTable>>,
    ) -> PolarsResult<Option<BuildState>> {
        let left_saturated = self.left_len >= *SAMPLE_LIMIT;
        let right_saturated = self.right_len >= *SAMPLE_LIMIT;
        let left_done = recv[0] == PortState::Done || left_saturated;
        let right_done = recv[1] == PortState::Done || right_saturated;
        #[expect(clippy::nonminimal_bool)]
        let stop_sampling = (left_done && right_done)
            || (left_done && self.right_len >= LOPSIDED_SAMPLE_FACTOR * self.left_len)
            || (right_done && self.left_len >= LOPSIDED_SAMPLE_FACTOR * self.right_len);
        if !stop_sampling {
            return Ok(None);
        }

        if config::verbose() {
            eprintln!(
                "choosing equi-join build side, sample lengths are: {} vs. {}",
                self.left_len, self.right_len
            );
        }

        let estimate_cardinalities = || {
            let execution_state = ExecutionState::new();
            let left_cardinality = estimate_cardinality(
                &self.left,
                &params.left_key_selectors,
                params,
                &execution_state,
            )?;
            let right_cardinality = estimate_cardinality(
                &self.right,
                &params.right_key_selectors,
                params,
                &execution_state,
            )?;
            let norm_left_factor = self.left_len.min(*SAMPLE_LIMIT) as f64 / self.left_len as f64;
            let norm_right_factor =
                self.right_len.min(*SAMPLE_LIMIT) as f64 / self.right_len as f64;
            let norm_left_cardinality = (left_cardinality as f64 * norm_left_factor) as usize;
            let norm_right_cardinality = (right_cardinality as f64 * norm_right_factor) as usize;
            if config::verbose() {
                eprintln!("estimated cardinalities are: {norm_left_cardinality} vs. {norm_right_cardinality}");
            }
            PolarsResult::Ok((norm_left_cardinality, norm_right_cardinality))
        };

        let left_is_build = match (left_saturated, right_saturated) {
            (false, false) => {
                if self.left_len * LOPSIDED_SAMPLE_FACTOR < self.right_len
                    || self.left_len > self.right_len * LOPSIDED_SAMPLE_FACTOR
                {
                    // Don't bother estimating cardinality, just choose smaller as it's highly
                    // imbalanced.
                    self.left_len < self.right_len
                } else {
                    let (lc, rc) = estimate_cardinalities()?;
                    // Let's assume for now that per element building a
                    // table is 3x more expensive than a probe, with
                    // unique keys getting an additional 3x factor for
                    // having to update the hash table in addition to the probe.
                    let left_build_cost = self.left_len * 3 + 3 * lc;
                    let left_probe_cost = self.left_len;
                    let right_build_cost = self.right_len * 3 + 3 * rc;
                    let right_probe_cost = self.right_len;
                    left_build_cost + right_probe_cost < left_probe_cost + right_build_cost
                }
            },

            // Choose the unsaturated side, the saturated side could be
            // arbitrarily big.
            (false, true) => true,
            (true, false) => false,

            // Estimate cardinality and choose smaller.
            (true, true) => {
                let (lc, rc) = estimate_cardinalities()?;
                lc < rc
            },
        };

        if config::verbose() {
            eprintln!(
                "build side chosen: {}",
                if left_is_build { "left" } else { "right" }
            );
        }

        // Transition to building state.
        params.left_is_build = Some(left_is_build);
        *table = Some(if left_is_build {
            new_chunked_idx_table(params.left_key_schema.clone())
        } else {
            new_chunked_idx_table(params.right_key_schema.clone())
        });

        let mut sampled_build_morsels =
            BufferedStream::new(core::mem::take(&mut self.left), MorselSeq::default());
        let mut sampled_probe_morsels =
            BufferedStream::new(core::mem::take(&mut self.right), MorselSeq::default());
        if !left_is_build {
            core::mem::swap(&mut sampled_build_morsels, &mut sampled_probe_morsels);
        }

        let partitioner = HashPartitioner::new(num_pipelines, 0);
        let mut build_state = BuildState {
            partitions_per_worker: (0..num_pipelines).map(|_| Vec::new()).collect(),
            sampled_probe_morsels,
        };

        // Simulate the sample build morsels flowing into the build side.
        if !sampled_build_morsels.is_empty() {
            let state = ExecutionState::new();
            crate::async_executor::task_scope(|scope| {
                let mut join_handles = Vec::new();
                let receivers = sampled_build_morsels
                    .reinsert(num_pipelines, None, scope, &mut join_handles)
                    .unwrap();

                for (worker_ps, recv) in build_state.partitions_per_worker.iter_mut().zip(receivers)
                {
                    join_handles.push(scope.spawn_task(
                        TaskPriority::High,
                        BuildState::partition_and_sink(
                            recv,
                            worker_ps,
                            partitioner.clone(),
                            params,
                            &state,
                        ),
                    ));
                }

                polars_io::pl_async::get_runtime().block_on(async move {
                    for handle in join_handles {
                        handle.await?;
                    }
                    PolarsResult::Ok(())
                })
            })?;
        }

        Ok(Some(build_state))
    }
}

#[derive(Default)]
struct BuildPartition {
    hash_keys: Vec<HashKeys>,
    frames: Vec<(MorselSeq, DataFrame)>,
    sketch: Option<CardinalitySketch>,
}

#[derive(Default)]
struct BuildState {
    partitions_per_worker: Vec<Vec<BuildPartition>>,
    sampled_probe_morsels: BufferedStream,
}

impl BuildState {
    async fn partition_and_sink(
        mut recv: Receiver<Morsel>,
        partitions: &mut Vec<BuildPartition>,
        partitioner: HashPartitioner,
        params: &EquiJoinParams,
        state: &ExecutionState,
    ) -> PolarsResult<()> {
        let track_unmatchable = params.emit_unmatched_build();
        let mut partition_idxs = vec![Vec::new(); partitioner.num_partitions()];
        partitions.resize_with(partitioner.num_partitions(), BuildPartition::default);
        let mut sketches = vec![CardinalitySketch::default(); partitioner.num_partitions()];

        let (key_selectors, payload_selector);
        if params.left_is_build.unwrap() {
            payload_selector = &params.left_payload_select;
            key_selectors = &params.left_key_selectors;
        } else {
            payload_selector = &params.right_payload_select;
            key_selectors = &params.right_key_selectors;
        };

        while let Ok(morsel) = recv.recv().await {
            // Compute hashed keys and payload. We must rechunk the payload for
            // later chunked gathers.
            let hash_keys = select_keys(morsel.df(), key_selectors, params, state).await?;
            let mut payload = select_payload(morsel.df().clone(), payload_selector);
            payload.rechunk_mut();
            payload._deshare_views_mut();

            unsafe {
                hash_keys.gen_partition_idxs(
                    &partitioner,
                    &mut partition_idxs,
                    &mut sketches,
                    track_unmatchable,
                );
                for (p, idxs_in_p) in partitions.iter_mut().zip(&partition_idxs) {
                    let payload_for_partition = payload.take_slice_unchecked_impl(idxs_in_p, false);
                    p.hash_keys.push(hash_keys.gather(idxs_in_p));
                    p.frames.push((morsel.seq(), payload_for_partition));
                }
            }
        }

        for (p, sketch) in sketches.into_iter().enumerate() {
            partitions[p].sketch = Some(sketch);
        }

        Ok(())
    }

    fn finalize(&mut self, params: &EquiJoinParams, table: &dyn ChunkedIdxTable) -> ProbeState {
        // Transpose.
        let num_workers = self.partitions_per_worker.len();
        let num_partitions = self.partitions_per_worker[0].len();
        let mut results_per_partition = (0..num_partitions)
            .map(|_| Vec::with_capacity(num_workers))
            .collect_vec();
        for worker in self.partitions_per_worker.drain(..) {
            for (p, result) in worker.into_iter().enumerate() {
                results_per_partition[p].push(result);
            }
        }

        POOL.install(|| {
            let track_unmatchable = params.emit_unmatched_build();
            let table_per_partition: Vec<_> = results_per_partition
                .into_par_iter()
                .with_max_len(1)
                .map(|results| {
                    // Estimate sizes and cardinality.
                    let mut sketch = CardinalitySketch::new();
                    let mut num_frames = 0;
                    for result in &results {
                        sketch.combine(result.sketch.as_ref().unwrap());
                        num_frames += result.frames.len();
                    }

                    // Build table for this partition.
                    let mut combined_frames = Vec::with_capacity(num_frames);
                    let mut chunk_seq_ids = Vec::with_capacity(num_frames);
                    let mut table = table.new_empty();
                    table.reserve(sketch.estimate() * 5 / 4);
                    if params.preserve_order_build {
                        let mut combined = Vec::with_capacity(num_frames);
                        for result in results {
                            for (hash_keys, (seq, frame)) in
                                result.hash_keys.into_iter().zip(result.frames)
                            {
                                combined.push((seq, hash_keys, frame));
                            }
                        }

                        combined.sort_unstable_by_key(|c| c.0);
                        for (seq, hash_keys, frame) in combined {
                            // Zero-sized chunks can get deleted, so skip entirely to avoid messing
                            // up the chunk counter.
                            if frame.height() == 0 {
                                continue;
                            }

                            table.insert_key_chunk(hash_keys, track_unmatchable);
                            combined_frames.push(frame);
                            chunk_seq_ids.push(seq);
                        }
                    } else {
                        for result in results {
                            for (hash_keys, (_, frame)) in
                                result.hash_keys.into_iter().zip(result.frames)
                            {
                                // Zero-sized chunks can get deleted, so skip entirely to avoid messing
                                // up the chunk counter.
                                if frame.height() == 0 {
                                    continue;
                                }

                                table.insert_key_chunk(hash_keys, track_unmatchable);
                                combined_frames.push(frame);
                            }
                        }
                    }

                    let df = if combined_frames.is_empty() {
                        if params.left_is_build.unwrap() {
                            DataFrame::empty_with_schema(&params.left_payload_schema)
                        } else {
                            DataFrame::empty_with_schema(&params.right_payload_schema)
                        }
                    } else {
                        accumulate_dataframes_vertical_unchecked(combined_frames)
                    };
                    ProbeTable {
                        table,
                        df,
                        chunk_seq_ids,
                    }
                })
                .collect();

            ProbeState {
                table_per_partition,
                max_seq_sent: MorselSeq::default(),
                sampled_probe_morsels: core::mem::take(&mut self.sampled_probe_morsels),
            }
        })
    }
}

struct ProbeTable {
    // Important that df is not rechunked, the chunks it was inserted with
    // into the table must be preserved for chunked gathers.
    table: Box<dyn ChunkedIdxTable>,
    df: DataFrame,
    chunk_seq_ids: Vec<MorselSeq>,
}

struct ProbeState {
    table_per_partition: Vec<ProbeTable>,
    max_seq_sent: MorselSeq,
    sampled_probe_morsels: BufferedStream,
}

impl ProbeState {
    /// Returns the max morsel sequence sent.
    async fn partition_and_probe(
        mut recv: Receiver<Morsel>,
        mut send: Sender<Morsel>,
        partitions: &[ProbeTable],
        partitioner: HashPartitioner,
        params: &EquiJoinParams,
        state: &ExecutionState,
    ) -> PolarsResult<MorselSeq> {
        // TODO: shuffle after partitioning and keep probe tables thread-local.
        let mut partition_idxs = vec![Vec::new(); partitioner.num_partitions()];
        let mut table_match = Vec::new();
        let mut probe_match = Vec::new();
        let mut max_seq = MorselSeq::default();

        let probe_limit = get_ideal_morsel_size() as IdxSize;
        let mark_matches = params.emit_unmatched_build();
        let emit_unmatched = params.emit_unmatched_probe();

        let (key_selectors, payload_selector);
        if params.left_is_build.unwrap() {
            payload_selector = &params.right_payload_select;
            key_selectors = &params.right_key_selectors;
        } else {
            payload_selector = &params.left_payload_select;
            key_selectors = &params.left_key_selectors;
        };

        while let Ok(morsel) = recv.recv().await {
            // Compute hashed keys and payload.
            let (df, seq, src_token, wait_token) = morsel.into_inner();
            let hash_keys = select_keys(&df, key_selectors, params, state).await?;
            let mut payload = select_payload(df, payload_selector);
            let mut payload_rechunked = false; // We don't eagerly rechunk because there might be no matches.

            max_seq = seq;

            unsafe {
                // Partition and probe the tables.
                hash_keys.gen_partition_idxs(
                    &partitioner,
                    &mut partition_idxs,
                    &mut [],
                    emit_unmatched,
                );
                if params.preserve_order_probe {
                    // TODO: non-sort based implementation, can directly scatter
                    // after finding matches for each partition.
                    let mut out_per_partition = Vec::with_capacity(partitioner.num_partitions());
                    let name = PlSmallStr::from_static("__POLARS_PROBE_PRESERVE_ORDER_IDX");
                    for (p, idxs_in_p) in partitions.iter().zip(&partition_idxs) {
                        p.table.probe_subset(
                            &hash_keys,
                            idxs_in_p,
                            &mut table_match,
                            &mut probe_match,
                            mark_matches,
                            emit_unmatched,
                            IdxSize::MAX,
                        );

                        if table_match.is_empty() {
                            continue;
                        }

                        // Gather output and add to buffer.
                        let mut build_df = if emit_unmatched {
                            p.df.take_opt_chunked_unchecked(&table_match, false)
                        } else {
                            p.df.take_chunked_unchecked(&table_match, IsSorted::Not, false)
                        };

                        if !payload_rechunked {
                            // TODO: can avoid rechunk? We have to rechunk here or else we do it
                            // multiple times during the gather.
                            payload.rechunk_mut();
                            payload_rechunked = true;
                        }
                        let mut probe_df = payload.take_slice_unchecked_impl(&probe_match, false);

                        let mut out_df = if params.left_is_build.unwrap() {
                            build_df.hstack_mut_unchecked(probe_df.get_columns());
                            build_df
                        } else {
                            probe_df.hstack_mut_unchecked(build_df.get_columns());
                            probe_df
                        };

                        let idxs_ca =
                            IdxCa::from_vec(name.clone(), core::mem::take(&mut probe_match));
                        out_df.with_column_unchecked(idxs_ca.into_column());
                        out_per_partition.push(out_df);
                    }

                    if !out_per_partition.is_empty() {
                        let sort_options = SortMultipleOptions {
                            descending: vec![false],
                            nulls_last: vec![false],
                            multithreaded: false,
                            maintain_order: true,
                            limit: None,
                        };
                        let mut out_df =
                            accumulate_dataframes_vertical_unchecked(out_per_partition);
                        out_df.sort_in_place([name.clone()], sort_options).unwrap();
                        out_df.drop_in_place(&name).unwrap();
                        out_df = postprocess_join(out_df, params);

                        // TODO: break in smaller morsels.
                        let out_morsel = Morsel::new(out_df, seq, src_token.clone());
                        if send.send(out_morsel).await.is_err() {
                            break;
                        }
                    }
                } else {
                    let mut out_frames = Vec::new();
                    let mut out_len = 0;
                    for (p, idxs_in_p) in partitions.iter().zip(&partition_idxs) {
                        let mut offset = 0;
                        while offset < idxs_in_p.len() {
                            offset += p.table.probe_subset(
                                &hash_keys,
                                &idxs_in_p[offset..],
                                &mut table_match,
                                &mut probe_match,
                                mark_matches,
                                emit_unmatched,
                                probe_limit - out_len,
                            ) as usize;

                            if table_match.is_empty() {
                                continue;
                            }

                            // Gather output and send.
                            let mut build_df = if emit_unmatched {
                                p.df.take_opt_chunked_unchecked(&table_match, false)
                            } else {
                                p.df.take_chunked_unchecked(&table_match, IsSorted::Not, false)
                            };
                            if !payload_rechunked {
                                // TODO: can avoid rechunk? We have to rechunk here or else we do it
                                // multiple times during the gather.
                                payload.rechunk_mut();
                                payload_rechunked = true;
                            }
                            let mut probe_df =
                                payload.take_slice_unchecked_impl(&probe_match, false);

                            let out_df = if params.left_is_build.unwrap() {
                                build_df.hstack_mut_unchecked(probe_df.get_columns());
                                build_df
                            } else {
                                probe_df.hstack_mut_unchecked(build_df.get_columns());
                                probe_df
                            };
                            let out_df = postprocess_join(out_df, params);

                            out_len = out_len
                                .checked_add(out_df.height().try_into().unwrap())
                                .unwrap();
                            out_frames.push(out_df);

                            if out_len >= probe_limit {
                                out_len = 0;
                                let df =
                                    accumulate_dataframes_vertical_unchecked(out_frames.drain(..));
                                let out_morsel = Morsel::new(df, seq, src_token.clone());
                                if send.send(out_morsel).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }

                    if out_len > 0 {
                        let df = accumulate_dataframes_vertical_unchecked(out_frames.drain(..));
                        let out_morsel = Morsel::new(df, seq, src_token.clone());
                        if send.send(out_morsel).await.is_err() {
                            break;
                        }
                    }
                }
            }

            drop(wait_token);
        }

        Ok(max_seq)
    }

    fn ordered_unmatched(
        &mut self,
        partitioner: &HashPartitioner,
        params: &EquiJoinParams,
    ) -> DataFrame {
        let mut out_per_partition = Vec::with_capacity(partitioner.num_partitions());
        let seq_name = PlSmallStr::from_static("__POLARS_PROBE_PRESERVE_ORDER_SEQ");
        let idx_name = PlSmallStr::from_static("__POLARS_PROBE_PRESERVE_ORDER_IDX");
        let mut unmarked_idxs = Vec::new();
        unsafe {
            for p in self.table_per_partition.iter() {
                p.table.unmarked_keys(&mut unmarked_idxs, 0, IdxSize::MAX);

                // Gather and create full-null counterpart.
                let mut build_df =
                    p.df.take_chunked_unchecked(&unmarked_idxs, IsSorted::Not, false);
                let len = build_df.height();
                let mut out_df = if params.left_is_build.unwrap() {
                    let probe_df = DataFrame::full_null(&params.right_payload_schema, len);
                    build_df.hstack_mut_unchecked(probe_df.get_columns());
                    build_df
                } else {
                    let mut probe_df = DataFrame::full_null(&params.left_payload_schema, len);
                    probe_df.hstack_mut_unchecked(build_df.get_columns());
                    probe_df
                };

                // The indices are not ordered globally, but within each chunk they are, so sorting
                // by chunk sequence id, breaking ties by inner chunk idx works.
                let (chunk_seqs, idx_in_chunk) = unmarked_idxs
                    .iter()
                    .map(|chunk_id| {
                        let (chunk, idx_in_chunk) = chunk_id.extract();
                        (p.chunk_seq_ids[chunk as usize].to_u64(), idx_in_chunk)
                    })
                    .unzip();

                let chunk_seqs_ca = UInt64Chunked::from_vec(seq_name.clone(), chunk_seqs);
                let idxs_ca = IdxCa::from_vec(idx_name.clone(), idx_in_chunk);
                out_df.with_column_unchecked(chunk_seqs_ca.into_column());
                out_df.with_column_unchecked(idxs_ca.into_column());
                out_per_partition.push(out_df);
            }

            // Sort by chunk sequence id, then by inner chunk idx.
            let sort_options = SortMultipleOptions {
                descending: vec![false],
                nulls_last: vec![false],
                multithreaded: true,
                maintain_order: false,
                limit: None,
            };
            let mut out_df = accumulate_dataframes_vertical_unchecked(out_per_partition);
            out_df
                .sort_in_place([seq_name.clone(), idx_name.clone()], sort_options)
                .unwrap();
            out_df.drop_in_place(&seq_name).unwrap();
            out_df.drop_in_place(&idx_name).unwrap();
            out_df = postprocess_join(out_df, params);
            out_df
        }
    }
}

impl Drop for ProbeState {
    fn drop(&mut self) {
        POOL.install(|| {
            // Parallel drop as the state might be quite big.
            self.table_per_partition.par_drain(..).for_each(drop);
        })
    }
}

struct EmitUnmatchedState {
    partitions: Vec<ProbeTable>,
    active_partition_idx: usize,
    offset_in_active_p: usize,
    morsel_seq: MorselSeq,
}

impl EmitUnmatchedState {
    async fn emit_unmatched(
        &mut self,
        mut send: Sender<Morsel>,
        params: &EquiJoinParams,
        num_pipelines: usize,
    ) -> PolarsResult<()> {
        let total_len: usize = self
            .partitions
            .iter()
            .map(|p| p.table.num_keys() as usize)
            .sum();
        let ideal_morsel_count = (total_len / get_ideal_morsel_size()).max(1);
        let morsel_count = ideal_morsel_count.next_multiple_of(num_pipelines);
        let morsel_size = total_len.div_ceil(morsel_count).max(1);

        let wait_group = WaitGroup::default();
        let source_token = SourceToken::new();
        let mut unmarked_idxs = Vec::new();
        while let Some(p) = self.partitions.get(self.active_partition_idx) {
            loop {
                // Generate a chunk of unmarked key indices.
                self.offset_in_active_p += p.table.unmarked_keys(
                    &mut unmarked_idxs,
                    self.offset_in_active_p as IdxSize,
                    morsel_size as IdxSize,
                ) as usize;
                if unmarked_idxs.is_empty() {
                    break;
                }

                // Gather and create full-null counterpart.
                let out_df = unsafe {
                    let mut build_df =
                        p.df.take_chunked_unchecked(&unmarked_idxs, IsSorted::Not, false);
                    let len = build_df.height();
                    if params.left_is_build.unwrap() {
                        let probe_df = DataFrame::full_null(&params.right_payload_schema, len);
                        build_df.hstack_mut_unchecked(probe_df.get_columns());
                        build_df
                    } else {
                        let mut probe_df = DataFrame::full_null(&params.left_payload_schema, len);
                        probe_df.hstack_mut_unchecked(build_df.get_columns());
                        probe_df
                    }
                };
                let out_df = postprocess_join(out_df, params);

                // Send and wait until consume token is consumed.
                let mut morsel = Morsel::new(out_df, self.morsel_seq, source_token.clone());
                self.morsel_seq = self.morsel_seq.successor();
                morsel.set_consume_token(wait_group.token());
                if send.send(morsel).await.is_err() {
                    return Ok(());
                }

                wait_group.wait().await;
                if source_token.stop_requested() {
                    return Ok(());
                }
            }

            self.active_partition_idx += 1;
            self.offset_in_active_p = 0;
        }

        Ok(())
    }
}

enum EquiJoinState {
    Sample(SampleState),
    Build(BuildState),
    Probe(ProbeState),
    EmitUnmatchedBuild(EmitUnmatchedState),
    EmitUnmatchedBuildInOrder(InMemorySourceNode),
    Done,
}

struct EquiJoinParams {
    left_is_build: Option<bool>,
    preserve_order_build: bool,
    preserve_order_probe: bool,
    left_key_schema: Arc<Schema>,
    left_key_selectors: Vec<StreamExpr>,
    right_key_schema: Arc<Schema>,
    right_key_selectors: Vec<StreamExpr>,
    left_payload_select: Vec<Option<PlSmallStr>>,
    right_payload_select: Vec<Option<PlSmallStr>>,
    left_payload_schema: Schema,
    right_payload_schema: Schema,
    args: JoinArgs,
    random_state: PlRandomState,
}

impl EquiJoinParams {
    /// Should we emit unmatched rows from the build side?
    fn emit_unmatched_build(&self) -> bool {
        if self.left_is_build.unwrap() {
            self.args.how == JoinType::Left || self.args.how == JoinType::Full
        } else {
            self.args.how == JoinType::Right || self.args.how == JoinType::Full
        }
    }

    /// Should we emit unmatched rows from the probe side?
    fn emit_unmatched_probe(&self) -> bool {
        if self.left_is_build.unwrap() {
            self.args.how == JoinType::Right || self.args.how == JoinType::Full
        } else {
            self.args.how == JoinType::Left || self.args.how == JoinType::Full
        }
    }
}

pub struct EquiJoinNode {
    state: EquiJoinState,
    params: EquiJoinParams,
    num_pipelines: usize,
    table: Option<Box<dyn ChunkedIdxTable>>,
}

impl EquiJoinNode {
    pub fn new(
        left_input_schema: Arc<Schema>,
        right_input_schema: Arc<Schema>,
        left_key_schema: Arc<Schema>,
        right_key_schema: Arc<Schema>,
        left_key_selectors: Vec<StreamExpr>,
        right_key_selectors: Vec<StreamExpr>,
        args: JoinArgs,
    ) -> PolarsResult<Self> {
        let left_is_build = match args.maintain_order {
            MaintainOrderJoin::None => {
                if *SAMPLE_LIMIT == 0 {
                    Some(true)
                } else {
                    None
                }
            },
            MaintainOrderJoin::Left | MaintainOrderJoin::LeftRight => Some(false),
            MaintainOrderJoin::Right | MaintainOrderJoin::RightLeft => Some(true),
        };

        let table = left_is_build.map(|lib| {
            if lib {
                new_chunked_idx_table(left_key_schema.clone())
            } else {
                new_chunked_idx_table(right_key_schema.clone())
            }
        });

        let preserve_order_probe = args.maintain_order != MaintainOrderJoin::None;
        let preserve_order_build = matches!(
            args.maintain_order,
            MaintainOrderJoin::LeftRight | MaintainOrderJoin::RightLeft
        );

        let left_payload_select = compute_payload_selector(
            &left_input_schema,
            &right_input_schema,
            &left_key_schema,
            true,
            &args,
        )?;
        let right_payload_select = compute_payload_selector(
            &right_input_schema,
            &left_input_schema,
            &right_key_schema,
            false,
            &args,
        )?;

        let state = if left_is_build.is_some() {
            EquiJoinState::Build(BuildState::default())
        } else {
            EquiJoinState::Sample(SampleState::default())
        };

        let left_payload_schema = select_schema(&left_input_schema, &left_payload_select);
        let right_payload_schema = select_schema(&right_input_schema, &right_payload_select);
        Ok(Self {
            state,
            num_pipelines: 0,
            params: EquiJoinParams {
                left_is_build,
                preserve_order_build,
                preserve_order_probe,
                left_key_schema,
                left_key_selectors,
                right_key_schema,
                right_key_selectors,
                left_payload_select,
                right_payload_select,
                left_payload_schema,
                right_payload_schema,
                args,
                random_state: PlRandomState::new(),
            },
            table,
        })
    }
}

impl ComputeNode for EquiJoinNode {
    fn name(&self) -> &str {
        "equi_join"
    }

    fn initialize(&mut self, num_pipelines: usize) {
        self.num_pipelines = num_pipelines;
    }

    fn update_state(&mut self, recv: &mut [PortState], send: &mut [PortState]) -> PolarsResult<()> {
        assert!(recv.len() == 2 && send.len() == 1);

        // If the output doesn't want any more data, transition to being done.
        if send[0] == PortState::Done {
            self.state = EquiJoinState::Done;
        }

        // If we are sampling and both sides are done/filled, transition to building.
        if let EquiJoinState::Sample(sample_state) = &mut self.state {
            if let Some(build_state) = sample_state.try_transition_to_build(
                recv,
                self.num_pipelines,
                &mut self.params,
                &mut self.table,
            )? {
                self.state = EquiJoinState::Build(build_state);
            }
        }

        let build_idx = if self.params.left_is_build == Some(true) {
            0
        } else {
            1
        };
        let probe_idx = 1 - build_idx;

        // If we are building and the build input is done, transition to probing.
        if let EquiJoinState::Build(build_state) = &mut self.state {
            if recv[build_idx] == PortState::Done {
                self.state = EquiJoinState::Probe(
                    build_state.finalize(&self.params, self.table.as_deref().unwrap()),
                );
            }
        }

        // If we are probing and the probe input is done, emit unmatched if
        // necessary, otherwise we're done.
        if let EquiJoinState::Probe(probe_state) = &mut self.state {
            let samples_consumed = probe_state.sampled_probe_morsels.is_empty();
            if samples_consumed && recv[probe_idx] == PortState::Done {
                if self.params.emit_unmatched_build() {
                    if self.params.preserve_order_build {
                        let partitioner = HashPartitioner::new(self.num_pipelines, 0);
                        let unmatched = probe_state.ordered_unmatched(&partitioner, &self.params);
                        let mut src = InMemorySourceNode::new(
                            Arc::new(unmatched),
                            probe_state.max_seq_sent.successor(),
                        );
                        src.initialize(self.num_pipelines);
                        self.state = EquiJoinState::EmitUnmatchedBuildInOrder(src);
                    } else {
                        self.state = EquiJoinState::EmitUnmatchedBuild(EmitUnmatchedState {
                            partitions: core::mem::take(&mut probe_state.table_per_partition),
                            active_partition_idx: 0,
                            offset_in_active_p: 0,
                            morsel_seq: probe_state.max_seq_sent.successor(),
                        });
                    }
                } else {
                    self.state = EquiJoinState::Done;
                }
            }
        }

        // Finally, check if we are done emitting unmatched keys.
        if let EquiJoinState::EmitUnmatchedBuild(emit_state) = &mut self.state {
            if emit_state.active_partition_idx >= emit_state.partitions.len() {
                self.state = EquiJoinState::Done;
            }
        }

        match &mut self.state {
            EquiJoinState::Sample(sample_state) => {
                send[0] = PortState::Blocked;
                if recv[0] != PortState::Done {
                    recv[0] = if sample_state.left_len < *SAMPLE_LIMIT {
                        PortState::Ready
                    } else {
                        PortState::Blocked
                    };
                }
                if recv[1] != PortState::Done {
                    recv[1] = if sample_state.right_len < *SAMPLE_LIMIT {
                        PortState::Ready
                    } else {
                        PortState::Blocked
                    };
                }
            },
            EquiJoinState::Build(_) => {
                send[0] = PortState::Blocked;
                if recv[build_idx] != PortState::Done {
                    recv[build_idx] = PortState::Ready;
                }
                if recv[probe_idx] != PortState::Done {
                    recv[probe_idx] = PortState::Blocked;
                }
            },
            EquiJoinState::Probe(probe_state) => {
                if recv[probe_idx] != PortState::Done {
                    core::mem::swap(&mut send[0], &mut recv[probe_idx]);
                } else {
                    let samples_consumed = probe_state.sampled_probe_morsels.is_empty();
                    send[0] = if samples_consumed {
                        PortState::Done
                    } else {
                        PortState::Ready
                    };
                }
                recv[build_idx] = PortState::Done;
            },
            EquiJoinState::EmitUnmatchedBuild(_) => {
                send[0] = PortState::Ready;
                recv[build_idx] = PortState::Done;
                recv[probe_idx] = PortState::Done;
            },
            EquiJoinState::EmitUnmatchedBuildInOrder(src_node) => {
                recv[build_idx] = PortState::Done;
                recv[probe_idx] = PortState::Done;
                src_node.update_state(&mut [], &mut send[0..1])?;
                if send[0] == PortState::Done {
                    self.state = EquiJoinState::Done;
                }
            },
            EquiJoinState::Done => {
                send[0] = PortState::Done;
                recv[0] = PortState::Done;
                recv[1] = PortState::Done;
            },
        }
        Ok(())
    }

    fn is_memory_intensive_pipeline_blocker(&self) -> bool {
        matches!(
            self.state,
            EquiJoinState::Sample { .. } | EquiJoinState::Build { .. }
        )
    }

    fn spawn<'env, 's>(
        &'env mut self,
        scope: &'s TaskScope<'s, 'env>,
        recv_ports: &mut [Option<RecvPort<'_>>],
        send_ports: &mut [Option<SendPort<'_>>],
        state: &'s ExecutionState,
        join_handles: &mut Vec<JoinHandle<PolarsResult<()>>>,
    ) {
        assert!(recv_ports.len() == 2);
        assert!(send_ports.len() == 1);

        let build_idx = if self.params.left_is_build == Some(true) {
            0
        } else {
            1
        };
        let probe_idx = 1 - build_idx;

        match &mut self.state {
            EquiJoinState::Sample(sample_state) => {
                assert!(send_ports[0].is_none());
                let left_final_len = Arc::new(AtomicUsize::new(if recv_ports[0].is_none() {
                    sample_state.left_len
                } else {
                    usize::MAX
                }));
                let right_final_len = Arc::new(AtomicUsize::new(if recv_ports[1].is_none() {
                    sample_state.right_len
                } else {
                    usize::MAX
                }));

                if let Some(left_recv) = recv_ports[0].take() {
                    join_handles.push(scope.spawn_task(
                        TaskPriority::High,
                        SampleState::sink(
                            left_recv.serial(),
                            &mut sample_state.left,
                            &mut sample_state.left_len,
                            left_final_len.clone(),
                            right_final_len.clone(),
                        ),
                    ));
                }
                if let Some(right_recv) = recv_ports[1].take() {
                    join_handles.push(scope.spawn_task(
                        TaskPriority::High,
                        SampleState::sink(
                            right_recv.serial(),
                            &mut sample_state.right,
                            &mut sample_state.right_len,
                            right_final_len,
                            left_final_len,
                        ),
                    ));
                }
            },
            EquiJoinState::Build(build_state) => {
                assert!(send_ports[0].is_none());
                assert!(recv_ports[probe_idx].is_none());
                let receivers = recv_ports[build_idx].take().unwrap().parallel();

                build_state
                    .partitions_per_worker
                    .resize_with(self.num_pipelines, Vec::new);
                let partitioner = HashPartitioner::new(self.num_pipelines, 0);
                for (worker_ps, recv) in build_state.partitions_per_worker.iter_mut().zip(receivers)
                {
                    join_handles.push(scope.spawn_task(
                        TaskPriority::High,
                        BuildState::partition_and_sink(
                            recv,
                            worker_ps,
                            partitioner.clone(),
                            &self.params,
                            state,
                        ),
                    ));
                }
            },
            EquiJoinState::Probe(probe_state) => {
                assert!(recv_ports[build_idx].is_none());
                let senders = send_ports[0].take().unwrap().parallel();
                let receivers = probe_state
                    .sampled_probe_morsels
                    .reinsert(
                        self.num_pipelines,
                        recv_ports[probe_idx].take(),
                        scope,
                        join_handles,
                    )
                    .unwrap();

                let partitioner = HashPartitioner::new(self.num_pipelines, 0);
                let probe_tasks = receivers
                    .into_iter()
                    .zip(senders)
                    .map(|(recv, send)| {
                        scope.spawn_task(
                            TaskPriority::High,
                            ProbeState::partition_and_probe(
                                recv,
                                send,
                                &probe_state.table_per_partition,
                                partitioner.clone(),
                                &self.params,
                                state,
                            ),
                        )
                    })
                    .collect_vec();

                let max_seq_sent = &mut probe_state.max_seq_sent;
                join_handles.push(scope.spawn_task(TaskPriority::High, async move {
                    for probe_task in probe_tasks {
                        *max_seq_sent = (*max_seq_sent).max(probe_task.await?);
                    }
                    Ok(())
                }));
            },
            EquiJoinState::EmitUnmatchedBuild(emit_state) => {
                assert!(recv_ports[build_idx].is_none());
                assert!(recv_ports[probe_idx].is_none());
                let send = send_ports[0].take().unwrap().serial();
                join_handles.push(scope.spawn_task(
                    TaskPriority::Low,
                    emit_state.emit_unmatched(send, &self.params, self.num_pipelines),
                ));
            },
            EquiJoinState::EmitUnmatchedBuildInOrder(src_node) => {
                assert!(recv_ports[build_idx].is_none());
                assert!(recv_ports[probe_idx].is_none());
                src_node.spawn(scope, &mut [], send_ports, state, join_handles);
            },
            EquiJoinState::Done => unreachable!(),
        }
    }
}
