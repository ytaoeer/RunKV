use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use itertools::Itertools;
use runkv_common::coding::CompressionAlgorithm;
use runkv_proto::exhauster::exhauster_service_server::ExhausterService;
use runkv_proto::exhauster::{CompactionRequest, CompactionResponse};
use runkv_proto::manifest::SstableInfo;
use runkv_storage::components::{
    CachePolicy, Sstable, SstableBuilder, SstableBuilderOptions, SstableStoreRef,
};
use runkv_storage::iterator::{BoxedIterator, Iterator, MergeIterator, Seek, SstableIterator};
use runkv_storage::utils::{sequence, user_key, value};
use tonic::{Request, Response, Status};
use tracing::{debug, trace};

use crate::compaction_filter::{CompactionFilter, DefaultCompactionFilter};
use crate::error::Result;
use crate::partitioner::{BoxedPartitioner, DefaultPartitioner, NoPartitioner};

fn internal(e: impl Into<Box<dyn std::error::Error>>) -> Status {
    Status::internal(e.into().to_string())
}

pub struct ExhausterOptions {
    pub node_id: u64,
    pub sstable_store: SstableStoreRef,
    pub sstable_sequential_id: u64,
}

pub struct Exhauster {
    options: ExhausterOptions,
    sstable_store: SstableStoreRef,
    sstable_sequential_id: AtomicU64,
}

impl Exhauster {
    pub fn new(options: ExhausterOptions) -> Self {
        Self {
            sstable_store: options.sstable_store.clone(),
            sstable_sequential_id: AtomicU64::new(options.sstable_sequential_id),
            options,
        }
    }
}

#[async_trait]
impl ExhausterService for Exhauster {
    async fn compaction(
        &self,
        request: Request<CompactionRequest>,
    ) -> core::result::Result<Response<CompactionResponse>, Status> {
        let req = request.into_inner();
        let mut old_sst_infos = Vec::with_capacity(req.sst_ids.len());
        let mut iters: Vec<BoxedIterator> = Vec::with_capacity(req.sst_ids.len());
        for sst_id in &req.sst_ids {
            let sst = self
                .sstable_store
                .sstable(*sst_id)
                .await
                .map_err(internal)?;
            old_sst_infos.push(SstableInfo {
                id: *sst_id,
                data_size: sst.data_size() as u64,
            });
            let iter = SstableIterator::new(self.sstable_store.clone(), sst, CachePolicy::Fill);
            iters.push(Box::new(iter));
        }
        let mut iter = MergeIterator::new(iters);
        let sstable_builder_options = SstableBuilderOptions {
            capacity: req.sstable_capacity as usize,
            block_capacity: req.block_capacity as usize,
            restart_interval: req.restart_interval as usize,
            bloom_false_positive: req.bloom_false_positive,
            compression_algorithm: CompressionAlgorithm::try_from(req.compression_algorithm as u8)
                .map_err(internal)?,
        };
        let mut sstable_builder = None;
        iter.seek(Seek::First).await.map_err(internal)?;
        let mut sst_id = 0;
        let mut compaction_filter =
            DefaultCompactionFilter::new(req.watermark, req.remove_tombstone);
        let partition_points = req
            .partition_points
            .into_iter()
            .map(Bytes::from)
            .collect_vec();
        let mut partitioner: BoxedPartitioner = if partition_points.is_empty() {
            Box::new(NoPartitioner::default())
        } else {
            Box::new(DefaultPartitioner::new(partition_points))
        };
        let mut new_sst_infos = Vec::with_capacity(req.sst_ids.len());
        let mut last_user_key = vec![];
        // Filter key value pairs.
        while iter.is_valid() {
            let uk = user_key(iter.key());
            let ts = sequence(iter.key());
            let v = value(iter.value());

            if sstable_builder.is_none() {
                sst_id = self.gen_sstable_id();
                sstable_builder = Some(SstableBuilder::new(sstable_builder_options.clone()));
            }
            if !sstable_builder.as_ref().unwrap().is_empty()
            // Pervent multi versions of one user key being split in multi ssts.
                && uk != last_user_key
                && (sstable_builder.as_ref().unwrap().approximate_len()
                    >= sstable_builder_options.capacity
                    || partitioner.partition(uk, v, ts))
            {
                let builder = sstable_builder.take().unwrap();
                let sst_info = self
                    .build_and_upload_sst(sst_id, builder)
                    .await
                    .map_err(internal)?;
                new_sst_infos.push(sst_info);
                continue;
            }
            let builder = sstable_builder.as_mut().unwrap();

            if compaction_filter.filter(uk, v, ts) {
                builder.add(uk, ts, v).map_err(internal)?;
                last_user_key = uk.to_vec();
            }
            iter.next().await.map_err(internal)?;
        }
        if let Some(builder) = sstable_builder.take() {
            let sst_info = self
                .build_and_upload_sst(sst_id, builder)
                .await
                .map_err(internal)?;
            new_sst_infos.push(sst_info);
        }
        let rsp = CompactionResponse {
            old_sst_infos,
            new_sst_infos,
        };
        Ok(Response::new(rsp))
    }
}

impl Exhauster {
    fn gen_sstable_id(&self) -> u64 {
        let sequential_id = self.sstable_sequential_id.fetch_add(1, Ordering::SeqCst);
        let node_id = self.options.node_id;
        (node_id << 32) | sequential_id
    }

    async fn build_and_upload_sst(
        &self,
        sst_id: u64,
        builder: SstableBuilder,
    ) -> Result<SstableInfo> {
        // TODO: Async upload.
        let (meta, data) = builder.build()?;
        let data_size = meta.data_size as u64;
        let sst = Sstable::new(sst_id, Arc::new(meta));
        trace!("build sst: {:#?}", sst);
        self.sstable_store
            .put(&sst, data, CachePolicy::Fill)
            .await?;
        debug!("sst {} uploaded", sst_id);
        Ok(SstableInfo {
            id: sst_id,
            data_size,
        })
    }
}
