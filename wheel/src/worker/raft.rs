use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::future;
use itertools::Itertools;
use lazy_static::lazy_static;
use prost::Message;
use runkv_common::context::Context;
use runkv_common::Worker;
use runkv_storage::raft_log_store::entry::RaftLogBatchBuilder;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{trace, trace_span, warn};

use crate::components::fsm::Fsm;
use crate::components::raft_log_store::{encode_entry_data, RaftGroupLogStore};
use crate::components::raft_network::{RaftClient, RaftNetwork};
use crate::error::{Error, Result};

const RAFT_HEARTBEAT_TICK_DURATION: Duration = Duration::from_millis(100);

lazy_static! {
    static ref RAFT_LATENCY_HISTOGRAM_VEC: prometheus::HistogramVec =
        prometheus::register_histogram_vec!(
            "raft_latency_histogram_vec",
            "raft latency histogram vec",
            &["op", "node", "group", "raft_node"]
        )
        .unwrap();
    static ref RAFT_THROUGHPUT_GAUGE_VEC: prometheus::GaugeVec = prometheus::register_gauge_vec!(
        "raft_throughput_gauge_vec",
        "raft throughput gauge vec",
        &["op", "node", "group", "raft_node"]
    )
    .unwrap();
}

struct RaftMetrics {
    append_log_entries_latency_histogram: prometheus::Histogram,
    append_log_entries_throughput_gauge: prometheus::Gauge,

    apply_log_entries_latency_histogram: prometheus::Histogram,

    send_messages_latency_histogram: prometheus::Histogram,
    send_messages_throughput_gauge: prometheus::Gauge,

    handle_ready_latency_histogram: prometheus::Histogram,
    poll_channel_latency_histogram: prometheus::Histogram,
}

impl RaftMetrics {
    fn new(node: u64, group: u64, raft_node: u64) -> Self {
        Self {
            append_log_entries_latency_histogram: RAFT_LATENCY_HISTOGRAM_VEC
                .get_metric_with_label_values(&[
                    "append_log_entries",
                    &node.to_string(),
                    &group.to_string(),
                    &raft_node.to_string(),
                ])
                .unwrap(),
            append_log_entries_throughput_gauge: RAFT_THROUGHPUT_GAUGE_VEC
                .get_metric_with_label_values(&[
                    "append_log_entries",
                    &node.to_string(),
                    &group.to_string(),
                    &raft_node.to_string(),
                ])
                .unwrap(),

            apply_log_entries_latency_histogram: RAFT_LATENCY_HISTOGRAM_VEC
                .get_metric_with_label_values(&[
                    "apply_log_entries",
                    &node.to_string(),
                    &group.to_string(),
                    &raft_node.to_string(),
                ])
                .unwrap(),

            send_messages_latency_histogram: RAFT_LATENCY_HISTOGRAM_VEC
                .get_metric_with_label_values(&[
                    "send_messages",
                    &node.to_string(),
                    &group.to_string(),
                    &raft_node.to_string(),
                ])
                .unwrap(),
            send_messages_throughput_gauge: RAFT_THROUGHPUT_GAUGE_VEC
                .get_metric_with_label_values(&[
                    "send_messages",
                    &node.to_string(),
                    &group.to_string(),
                    &raft_node.to_string(),
                ])
                .unwrap(),

            handle_ready_latency_histogram: RAFT_LATENCY_HISTOGRAM_VEC
                .get_metric_with_label_values(&[
                    "handle_ready",
                    &node.to_string(),
                    &group.to_string(),
                    &raft_node.to_string(),
                ])
                .unwrap(),
            poll_channel_latency_histogram: RAFT_LATENCY_HISTOGRAM_VEC
                .get_metric_with_label_values(&[
                    "poll_channel",
                    &node.to_string(),
                    &group.to_string(),
                    &raft_node.to_string(),
                ])
                .unwrap(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Proposal {
    pub data: Vec<u8>,
    pub context: Vec<u8>,
}

pub enum RaftStartMode {
    Initialize { peers: Vec<u64> },
    Restart { peers: Vec<u64> },
}

pub struct RaftWorkerOptions<RN: RaftNetwork, F: Fsm> {
    pub group: u64,
    pub node: u64,
    pub raft_node: u64,

    pub raft_start_mode: RaftStartMode,
    pub raft_log_store: RaftGroupLogStore,
    pub raft_logger: slog::Logger,
    pub raft_network: RN,

    pub proposal_rx: mpsc::UnboundedReceiver<Proposal>,

    pub fsm: F,
}

pub struct RaftWorker<RN, F>
where
    RN: RaftNetwork,
    F: Fsm,
{
    group: u64,
    node: u64,
    raft_node: u64,

    raft: raft::RawNode<RaftGroupLogStore>,
    raft_log_store: RaftGroupLogStore,
    _raft_network: RN,
    raft_soft_state: Option<raft::SoftState>,
    raft_clients: HashMap<u64, RN::RaftClient>,

    message_rx: mpsc::UnboundedReceiver<raft::prelude::Message>,
    proposal_rx: mpsc::UnboundedReceiver<Proposal>,

    fsm: F,

    metrics: RaftMetrics,
}

impl<RN, F> std::fmt::Debug for RaftWorker<RN, F>
where
    RN: RaftNetwork,
    F: Fsm,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RaftWorker")
            .field("node", &self.node)
            .field("group", &self.group)
            .field("raft_node", &self.raft_node)
            .finish()
    }
}

#[async_trait]
impl<RN, F> Worker for RaftWorker<RN, F>
where
    RN: RaftNetwork,
    F: Fsm,
{
    async fn run(&mut self) -> anyhow::Result<()> {
        // TODO: Gracefully kill.
        loop {
            match self.run_inner().await {
                Ok(_) => return Ok(()),
                Err(e) => warn!("error occur when raft worker running: {}", e),
            }
        }
    }
}

impl<RN, F> RaftWorker<RN, F>
where
    RN: RaftNetwork,
    F: Fsm,
{
    pub async fn build(options: RaftWorkerOptions<RN, F>) -> Result<Self> {
        let applied = match options.raft_start_mode {
            RaftStartMode::Initialize { .. } => 0,
            RaftStartMode::Restart { .. } => options.fsm.raft_applied_index().await?,
        };

        let raft_config = raft::Config {
            id: options.raft_node,
            // election_tick: todo!(),
            // heartbeat_tick: todo!(),
            applied,
            max_size_per_msg: 1 << 20,
            max_inflight_msgs: 256,
            check_quorum: true,
            pre_vote: true,
            // min_election_tick: todo!(),
            // max_election_tick: todo!(),
            read_only_option: raft::ReadOnlyOption::Safe,
            // skip_bcast_commit: todo!(),
            batch_append: true,
            // priority: todo!(),
            // max_uncommitted_size: todo!(),
            // max_committed_size_per_ready: todo!(),
            ..Default::default()
        };
        raft_config.validate().map_err(Error::err)?;

        let peers = match options.raft_start_mode {
            RaftStartMode::Initialize { ref peers } => peers.clone(),
            RaftStartMode::Restart { ref peers } => peers.clone(),
        };

        let raft_log_store = options.raft_log_store.clone();

        if let RaftStartMode::Initialize { .. } = options.raft_start_mode {
            let cs = raft::prelude::ConfState {
                voters: peers.clone(),
                ..Default::default()
            };
            raft_log_store.put_conf_state(&cs).await.unwrap();
        };

        let raft =
            raft::RawNode::new(&raft_config, raft_log_store.clone(), &options.raft_logger).await?;

        let message_rx = options
            .raft_network
            .take_message_rx(options.raft_node)
            .await?;

        let mut raft_clients = HashMap::default();
        for peer in peers {
            let client = options.raft_network.client(peer).await?;
            raft_clients.insert(peer, client);
        }

        Ok(Self {
            group: options.group,
            node: options.node,
            raft_node: options.raft_node,

            raft,
            raft_log_store,
            _raft_network: options.raft_network,
            raft_soft_state: None,
            raft_clients,

            fsm: options.fsm,

            proposal_rx: options.proposal_rx,
            message_rx,

            metrics: RaftMetrics::new(options.node, options.group, options.raft_node),
        })
    }

    async fn run_inner(&mut self) -> Result<()> {
        // // [`Interval`] with default [`MissedTickBehavior::Brust`].
        // let mut ticker = tokio::time::interval(RAFT_HEARTBEAT_TICK_DURATION);

        // loop {
        //     tokio::select! {
        //         biased;

        //         _ = ticker.tick() => {
        //             self.tick().await;
        //         }

        //         true = self.raft.has_ready() => {
        //             self.handle_ready().await?;

        //         }

        //         Some(msg) = self.message_rx.recv() => {
        //             self.step(msg).await?;
        //         }

        //         Some(proposal) = self.proposal_rx.recv() => {
        //             self.propose(proposal).await?;
        //         }

        //     }
        // }

        const MIN_LOOP_DURATION: Duration = Duration::from_millis(10);
        let mut remaining_timeout = RAFT_HEARTBEAT_TICK_DURATION;
        loop {
            let now = Instant::now();

            const BATCH_SIZE: usize = 128;
            let mut msgs = Vec::with_capacity(BATCH_SIZE);
            let mut proposals = Vec::with_capacity(BATCH_SIZE);

            let pool_channel_span = trace_span!("pool_channel_span");
            let pool_channel_span_guard = pool_channel_span.enter();
            let start_poll_channel = Instant::now();

            for _ in 0..BATCH_SIZE {
                match self.message_rx.try_recv() {
                    Ok(msg) => msgs.push(msg),
                    Err(mpsc::error::TryRecvError::Empty) => {}
                    Err(e) => return Err(Error::err(e)),
                }

                match self.proposal_rx.try_recv() {
                    Ok(proposal) => proposals.push(proposal),
                    Err(mpsc::error::TryRecvError::Empty) => {}
                    Err(e) => return Err(Error::err(e)),
                }
            }

            self.metrics
                .poll_channel_latency_histogram
                .observe(start_poll_channel.elapsed().as_secs_f64());
            drop(pool_channel_span_guard);

            for proposal in proposals {
                self.propose(proposal).await?;
            }

            for msg in msgs {
                self.step(msg).await?;
            }

            if self.raft.has_ready().await {
                self.handle_ready().await?;
            }

            let mut elapsed = now.elapsed();
            if elapsed < MIN_LOOP_DURATION {
                tokio::time::sleep(MIN_LOOP_DURATION - elapsed).await;
                elapsed = now.elapsed();
            }
            if elapsed >= remaining_timeout {
                remaining_timeout = RAFT_HEARTBEAT_TICK_DURATION;
                self.tick().await;
            } else {
                remaining_timeout -= elapsed;
            }
        }
    }

    // #[tracing::instrument(level = "trace")]
    async fn tick(&mut self) {
        self.raft.tick().await;
    }

    #[tracing::instrument(level = "trace", fields(request_id))]
    async fn propose(&mut self, proposal: Proposal) -> Result<()> {
        if cfg!(feature = "tracing") {
            let span = tracing::Span::current();
            let ctx: Context = bincode::deserialize(&proposal.context).map_err(Error::serde_err)?;
            span.follows_from(tracing::Id::from_u64(ctx.span_id));
            span.record("request_id", &ctx.request_id);
        }
        self.raft
            .propose(proposal.context, proposal.data)
            .await
            .map_err(Error::RaftError)
    }

    #[tracing::instrument(level = "trace")]
    async fn step(&mut self, msg: raft::prelude::Message) -> Result<()> {
        self.raft.step(msg).await.map_err(Error::RaftError)
    }

    #[tracing::instrument(level = "trace")]
    async fn handle_ready(&mut self) -> Result<()> {
        let start = Instant::now();

        let mut ready = self.raft.ready().await;

        // 0. Update soft state.
        if let Some(ss) = ready.ss() {
            self.raft_soft_state = Some(raft::SoftState {
                leader_id: ss.leader_id,
                raft_state: ss.raft_state,
            });
        }

        // 1. Send messages.
        self.send_messages(ready.take_messages()).await?;

        // 2. Apply snapshot if there is one.
        if !ready.snapshot().is_empty() {
            self.apply_snapshot(ready.snapshot()).await?;
        }

        // 3. Apply committed logs.
        self.apply_log_entries(ready.take_committed_entries())
            .await?;

        // 4. Append entries to log store.
        self.append_log_entries(ready.take_entries()).await?;

        // 5. Store `HardState` if needed.
        if let Some(hs) = ready.hs() {
            self.store_hard_state(hs).await?;
        }

        // 6. Send messages after persisting hard state.
        self.send_messages(ready.take_persisted_messages()).await?;

        // 7. Advance raft node and get `LightReady`.
        let mut ready = self.raft.advance(ready).await;

        // 8. Send messages of light ready.
        self.send_messages(ready.take_messages()).await?;

        // 9. Apply committed logs of light ready.
        self.apply_log_entries(ready.take_committed_entries())
            .await?;

        self.metrics
            .handle_ready_latency_histogram
            .observe(start.elapsed().as_secs_f64());

        Ok(())
    }

    #[tracing::instrument(level = "trace")]
    async fn send_messages(&mut self, messages: Vec<raft::prelude::Message>) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }

        if cfg!(feature = "tracing") {
            let span = tracing::Span::current();
            for msg in messages.iter() {
                if msg.msg_type() == raft::prelude::MessageType::MsgAppend {
                    for entry in msg.entries.iter() {
                        if entry.entry_type() == raft::prelude::EntryType::EntryNormal
                            && !entry.data.is_empty()
                        {
                            let ctx: Context =
                                bincode::deserialize(&entry.context).map_err(Error::serde_err)?;
                            span.follows_from(tracing::Id::from_u64(ctx.span_id));
                        }
                    }
                }
            }
        }

        let start = Instant::now();

        let mut bytes = 0;

        let mut raft_node_msgs = HashMap::new();
        for msg in messages {
            bytes += msg.encoded_len();
            let to = msg.to;
            raft_node_msgs
                .entry(to)
                .or_insert_with(|| Vec::with_capacity(16))
                .push(msg);
        }
        let futures = raft_node_msgs
            .into_iter()
            .map(|(raft_node, msgs)| {
                let mut client = self.raft_clients.get(&raft_node).unwrap().clone();
                async move { client.send(msgs).await }
            })
            .collect_vec();
        future::try_join_all(futures).await?;

        let elapsed = start.elapsed();
        self.metrics
            .send_messages_latency_histogram
            .observe(elapsed.as_secs_f64());
        self.metrics
            .send_messages_throughput_gauge
            .add(bytes as f64);
        Ok(())
    }

    #[tracing::instrument(level = "trace")]
    async fn apply_snapshot(&mut self, snapshot: &raft::prelude::Snapshot) -> Result<()> {
        // Impl me!!!
        // Impl me!!!
        // Impl me!!!
        todo!()
    }

    #[tracing::instrument(level = "trace")]
    async fn apply_log_entries(&mut self, entries: Vec<raft::prelude::Entry>) -> Result<()> {
        let is_leader = match &self.raft_soft_state {
            None => false,
            Some(ss) => ss.raft_state == raft::StateRole::Leader,
        };

        let start = Instant::now();

        self.fsm.apply(self.group, is_leader, entries).await?;

        let elapsed = start.elapsed();

        self.metrics
            .apply_log_entries_latency_histogram
            .observe(elapsed.as_secs_f64());
        Ok(())
    }

    #[tracing::instrument(level = "trace")]
    async fn append_log_entries(&mut self, entries: Vec<raft::prelude::Entry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let start = Instant::now();
        let mut bytes = 0;
        let mut builder = RaftLogBatchBuilder::default();
        for entry in entries {
            if cfg!(feature = "tracing") && let raft::prelude::EntryType::EntryNormal = entry.entry_type() && !entry.data.is_empty() {
                let span = tracing::Span::current();
                let ctx: Context = bincode::deserialize(&entry.context).map_err(Error::serde_err)?;
                span.follows_from(tracing::Id::from_u64(ctx.span_id));
            }

            bytes += entry.encoded_len();
            let data = encode_entry_data(&entry);
            builder.add(
                self.raft_node,
                entry.term,
                entry.index,
                &entry.context,
                &data,
            );
        }
        let batches = builder.build();
        trace!(
            "raft::append_log_entries generated {} batches: {:?}",
            batches.len(),
            batches
        );
        self.raft_log_store.append(batches).await?;
        let elapsed = start.elapsed();
        self.metrics
            .append_log_entries_latency_histogram
            .observe(elapsed.as_secs_f64());
        self.metrics
            .append_log_entries_throughput_gauge
            .add(bytes as f64);
        Ok(())
    }

    #[tracing::instrument(level = "trace")]
    async fn store_hard_state(&mut self, hs: &raft::prelude::HardState) -> Result<()> {
        self.raft_log_store.put_hard_state(hs).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
    use runkv_common::tracing_slog_drain::TracingSlogDrain;
    use runkv_common::Worker;
    use runkv_storage::raft_log_store::log::Persist;
    use runkv_storage::raft_log_store::store::RaftLogStoreOptions;
    use runkv_storage::raft_log_store::RaftLogStore;
    use test_log::test;

    use super::*;
    use crate::components::fsm::tests::MockFsm;
    use crate::components::raft_network::tests::MockRaftNetwork;

    #[test(tokio::test)]
    async fn test_raft_basic() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().to_str().unwrap();
        let raft_logger = build_raft_logger();
        let raft_log_store = build_raft_log_store(path).await;
        raft_log_store.add_group(1).await.unwrap();
        raft_log_store.add_group(2).await.unwrap();
        raft_log_store.add_group(3).await.unwrap();
        let raft_network = MockRaftNetwork::default();
        raft_network
            .register(100, BTreeMap::from_iter([(1, 10), (2, 10), (3, 10)]))
            .await
            .unwrap();

        macro_rules! worker {
            ($id:expr) => {
                build_raft_worker(
                    100,
                    10,
                    $id,
                    vec![1, 2, 3],
                    RaftGroupLogStore::new($id, raft_log_store.clone()),
                    raft_logger.clone(),
                    raft_network.clone(),
                )
                .await
            };
        }

        let (proposal_tx_1, mut apply_rx_1) = worker!(1);
        let (_proposal_tx_2, mut apply_rx_2) = worker!(2);
        let (_proposal_tx_3, mut apply_rx_3) = worker!(3);

        tokio::time::sleep(Duration::from_secs(10)).await;

        let data = vec![b'd'; 16];
        let context = vec![b'c'; 16];

        proposal_tx_1
            .send(Proposal {
                data: data.clone(),
                context: context.clone(),
            })
            .unwrap();

        loop {
            let entry = tokio::select! {
                entry = apply_rx_1.recv() => entry,
                entry = apply_rx_2.recv() => entry,
                entry = apply_rx_3.recv() => entry,
            };
            let entry = entry.unwrap();
            if entry.entry_type() != raft::prelude::EntryType::EntryNormal || entry.data.is_empty()
            {
                continue;
            }
            assert_matches!(entry, raft::prelude::Entry {
                data: edata,
                context: econtext,
                ..
            } => {
                assert_eq!(edata, data);
                assert_eq!(econtext, context);
            });
            break;
        }
    }

    fn build_raft_logger() -> slog::Logger {
        slog::Logger::root(TracingSlogDrain, slog::o!("namespace" => "raft"))
    }

    async fn build_raft_log_store(path: &str) -> RaftLogStore {
        let options = RaftLogStoreOptions {
            node: 0,
            log_dir_path: path.to_string(),
            log_file_capacity: 64 << 20,
            block_cache_capacity: 64 << 20,
            persist: Persist::Sync,
        };
        RaftLogStore::open(options).await.unwrap()
    }

    async fn build_raft_worker<RN: RaftNetwork>(
        group: u64,
        node: u64,
        raft_node: u64,
        peers: Vec<u64>,
        raft_log_store: RaftGroupLogStore,
        raft_logger: slog::Logger,
        raft_network: RN,
    ) -> (
        mpsc::UnboundedSender<Proposal>,
        mpsc::UnboundedReceiver<raft::prelude::Entry>,
    ) {
        let (proposal_tx, proposal_rx) = mpsc::unbounded_channel();
        let (fsm, apply_rx) = MockFsm::new(true);
        let options = RaftWorkerOptions {
            group,
            node,
            raft_node,
            raft_start_mode: RaftStartMode::Initialize { peers },
            raft_log_store,
            raft_logger,
            raft_network,

            proposal_rx,

            fsm,
        };
        let mut worker = RaftWorker::build(options).await.unwrap();
        let _handle = tokio::spawn(async move { worker.run().await });
        (proposal_tx, apply_rx)
    }
}
