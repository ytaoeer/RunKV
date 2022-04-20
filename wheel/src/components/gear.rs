use std::io::Cursor;
use std::ops::Range;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tracing::trace;

use super::command::{Apply, Snapshot};
use super::fsm::Fsm;
use crate::error::{Error, Result};

#[derive(Clone)]
pub struct Gear {
    apply_tx: mpsc::UnboundedSender<Apply>,
    snapshot_tx: mpsc::UnboundedSender<Snapshot>,
}

impl Gear {
    pub fn new(
        apply_tx: mpsc::UnboundedSender<Apply>,
        snapshot_tx: mpsc::UnboundedSender<Snapshot>,
    ) -> Self {
        Self {
            apply_tx,
            snapshot_tx,
        }
    }
}

#[async_trait]
impl Fsm for Gear {
    async fn apply(&self, _group: u64, _index: u64, _request: &[u8]) -> Result<()> {
        Ok(())
    }

    async fn post_apply(&self, group: u64, range: Range<u64>) -> Result<()> {
        trace!(
            "notify apply: [group: {}] [range: [{}..{})]",
            group,
            range.start,
            range.end
        );
        self.apply_tx
            .send(Apply { group, range })
            .map_err(Error::err)?;
        Ok(())
    }

    async fn build_snapshot(&self, group: u64, index: u64) -> Result<Cursor<Vec<u8>>> {
        trace!("build snapshot");
        let (tx, rx) = oneshot::channel();
        self.snapshot_tx
            .send(Snapshot::Build {
                group,
                index,
                notifier: tx,
            })
            .map_err(Error::err)?;
        let snapshot = rx.await.map_err(Error::err)?;
        Ok(Cursor::new(snapshot))
    }

    async fn install_snapshot(
        &self,
        group: u64,
        index: u64,
        snapshot: &Cursor<Vec<u8>>,
    ) -> Result<()> {
        trace!("install snapshot: {:?}", snapshot);
        let (tx, rx) = oneshot::channel();
        self.snapshot_tx
            .send(Snapshot::Install {
                group,
                index,
                snapshot: snapshot.to_owned().into_inner(),
                notifier: tx,
            })
            .map_err(Error::err)?;
        rx.await.map_err(Error::err)?;
        Ok(())
    }
}
