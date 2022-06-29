use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;

use core::ops::Deref;

use ic_ledger_core::block::{BlockType, EncodedBlock, HashOf};
use ledger_canister::{Block, BlockHeight, TipOfChainRes};
use log::{debug, error, info, trace};
use tokio::sync::RwLock;

use crate::blocks::Blocks;
use crate::blocks_access::BlocksAccess;
use crate::certification::{verify_block_hash, VerificationInfo};
use crate::errors::Error;
use crate::store::{BlockStoreError, HashedBlock};

// If pruning is enabled, instead of pruning after each new block
// we'll wait for PRUNE_DELAY blocks to accumulate and prune them in one go
const PRUNE_DELAY: u64 = 10000;

const PRINT_SYNC_PROGRESS_THRESHOLD: u64 = 1000;

/// The LedgerBlocksSynchronizer will use this to output the metrics while
/// synchronizing with the Leddger
pub trait LedgerBlocksSynchronizerMetrics {
    fn set_target_height(&self, height: u64);
    fn set_synced_height(&self, height: u64);
    fn set_verified_height(&self, height: u64);
}

struct NopMetrics {}

impl LedgerBlocksSynchronizerMetrics for NopMetrics {
    fn set_target_height(&self, _height: u64) {}
    fn set_synced_height(&self, _height: u64) {}
    fn set_verified_height(&self, _height: u64) {}
}

/// Downloads the blocks of the Ledger to either an in-memory store or to
/// a local sqlite store
pub struct LedgerBlocksSynchronizer<B>
where
    B: BlocksAccess,
{
    pub blockchain: RwLock<Blocks>,
    blocks_access: Option<Arc<B>>,
    // TODO: move store_max_blocks in sync or move up_to_block here
    store_max_blocks: Option<u64>,
    verification_info: Option<VerificationInfo>,
    metrics: Box<dyn LedgerBlocksSynchronizerMetrics + Send + Sync>,
}

impl<B: BlocksAccess> LedgerBlocksSynchronizer<B> {
    pub async fn new(
        blocks_access: Option<Arc<B>>,
        store_location: Option<&std::path::Path>,
        store_max_blocks: Option<u64>,
        verification_info: Option<VerificationInfo>,
        metrics: Box<dyn LedgerBlocksSynchronizerMetrics + Send + Sync>,
    ) -> Result<LedgerBlocksSynchronizer<B>, Error> {
        let mut blocks = match store_location {
            Some(loc) => Blocks::new_persistent(loc),
            None => Blocks::new_in_memory(),
        };

        if let Some(blocks_access) = &blocks_access {
            Self::verify_store(&blocks, blocks_access).await?;
            if let Some(verification_info) = &verification_info {
                // verify if we have the right certificate/we are connecting to the right
                // canister
                Self::verify_tip_of_chain(blocks_access, verification_info).await?;
            }
        }

        info!("Loading blocks from store");
        let num_loaded = blocks.load_from_store()?;

        info!(
            "Ledger client is up. Loaded {} blocks from store. First block at {}, last at {}",
            num_loaded,
            blocks
                .first()?
                .map(|x| format!("{}", x.index))
                .unwrap_or_else(|| "None".to_string()),
            blocks
                .last()?
                .map(|x| format!("{}", x.index))
                .unwrap_or_else(|| "None".to_string())
        );
        if let Some(x) = blocks.last()? {
            metrics.set_synced_height(x.index);
        }
        if let Some(x) = blocks.block_store.last_verified() {
            metrics.set_verified_height(x);
        }

        blocks.try_prune(&store_max_blocks, PRUNE_DELAY)?;

        Ok(Self {
            blockchain: RwLock::new(blocks),
            blocks_access,
            store_max_blocks,
            verification_info,
            metrics,
        })
    }

    async fn verify_store(blocks: &Blocks, canister_access: &B) -> Result<(), Error> {
        debug!("Verifying store...");
        let first_block = blocks.block_store.first()?;

        match blocks.block_store.get_at(0) {
            Ok(store_genesis) => {
                let genesis = canister_access
                    .query_raw_block(0)
                    .await
                    .map_err(Error::InternalError)?
                    .expect("Blockchain in the ledger canister is empty");

                if store_genesis.hash != Block::block_hash(&genesis) {
                    let msg = format!(
                        "Genesis block from the store is different than \
                        in the ledger canister. Store hash: {}, canister hash: {}",
                        store_genesis.hash,
                        Block::block_hash(&genesis)
                    );
                    error!("{}", msg);
                    return Err(Error::InternalError(msg));
                }
            }
            Err(BlockStoreError::NotFound(0)) => {
                if first_block.is_some() {
                    let msg = "Snapshot found, but genesis block not present in the store";
                    error!("{}", msg);
                    return Err(Error::InternalError(msg.to_string()));
                }
            }
            Err(e) => {
                let msg = format!("Error loading genesis block: {:?}", e);
                error!("{}", msg);
                return Err(Error::InternalError(msg));
            }
        }

        if first_block.is_some() && first_block.as_ref().unwrap().index > 0 {
            let first_block = first_block.unwrap();
            let queried_block = canister_access
                .query_raw_block(first_block.index)
                .await
                .map_err(Error::InternalError)?;
            if queried_block.is_none() {
                let msg = format!(
                    "Oldest block snapshot does not match the block on \
                    the blockchain. Block with this index not found: {}",
                    first_block.index
                );
                error!("{}", msg);
                return Err(Error::InternalError(msg));
            }
            let queried_block = queried_block.unwrap();
            if first_block.hash != Block::block_hash(&queried_block) {
                let msg = format!(
                    "Oldest block snapshot does not match the block on \
                    the blockchain. Index: {}, snapshot hash: {}, canister hash: {}",
                    first_block.index,
                    first_block.hash,
                    Block::block_hash(&queried_block)
                );
                error!("{}", msg);
                return Err(Error::InternalError(msg));
            }
        }
        debug!("Verifying store done");
        Ok(())
    }

    async fn verify_tip_of_chain(
        canister_access: &B,
        verification_info: &VerificationInfo,
    ) -> Result<(), Error> {
        let TipOfChainRes {
            tip_index,
            certification,
        } = canister_access
            .query_tip()
            .await
            .map_err(Error::InternalError)?;
        let tip_block = canister_access
            .query_raw_block(tip_index)
            .await
            .map_err(Error::InternalError)?
            .expect("Blockchain in the ledger canister is empty");
        verify_block_hash(
            &certification,
            Block::block_hash(&tip_block),
            verification_info,
        )
        .map_err(Error::InternalError)?;
        Ok(())
    }

    pub async fn read_blocks(&self) -> Box<dyn Deref<Target = Blocks> + '_> {
        Box::new(self.blockchain.read().await)
    }

    pub async fn sync_blocks(
        &self,
        stopped: Arc<AtomicBool>,
        up_to_block_included: Option<BlockHeight>,
    ) -> Result<(), Error> {
        let canister = self.blocks_access.as_ref().unwrap();
        let TipOfChainRes {
            tip_index,
            mut certification,
        } = canister.query_tip().await.map_err(Error::InternalError)?;
        self.metrics.set_target_height(tip_index);

        let mut blockchain = self.blockchain.write().await;

        let (last_block_hash, next_block_index) = match blockchain.synced_to() {
            Some((hash, index)) => (Some(hash), index + 1),
            None => (None, 0),
        };

        if next_block_index > tip_index {
            trace!(
                "Tip received from the Ledger is lower than what we already have (queried lagging replica?),
                Ledger tip index: {}, local copy tip index: {}",
                tip_index,
                next_block_index
            );
            return Ok(());
        }

        let up_to_block_included = tip_index.min(up_to_block_included.unwrap_or(u64::MAX));

        if up_to_block_included != tip_index {
            certification = None; // certification can be checked only with the last block
        }

        if next_block_index > up_to_block_included {
            return Ok(()); // nothing to do nor report, local copy has enough blocks
        }

        trace!(
            "Sync {} blocks from index: {}, ledger tip index: {}",
            up_to_block_included - next_block_index,
            next_block_index,
            tip_index
        );

        self.sync_range_of_blocks(
            Range {
                start: next_block_index,
                end: up_to_block_included + 1,
            },
            last_block_hash,
            stopped,
            certification,
            &mut *blockchain,
        )
        .await?;

        info!(
            "You are all caught up to block {}",
            blockchain.last()?.unwrap().index
        );

        blockchain.try_prune(&self.store_max_blocks, PRUNE_DELAY)
    }

    async fn sync_range_of_blocks(
        &self,
        range: Range<BlockHeight>,
        first_block_parent_hash: Option<HashOf<EncodedBlock>>,
        stopped: Arc<AtomicBool>,
        certification: Option<Vec<u8>>,
        blockchain: &mut Blocks,
    ) -> Result<(), Error> {
        let print_progress = if range.end - range.start >= PRINT_SYNC_PROGRESS_THRESHOLD {
            info!(
                "Syncing {} blocks. New tip will be {}",
                range.end - range.start,
                range.end,
            );
            true
        } else {
            false
        };

        let canister = self.blocks_access.as_ref().unwrap();
        let mut i = range.start;
        let mut last_block_hash = first_block_parent_hash;
        while i < range.end {
            if stopped.load(Relaxed) {
                return Err(Error::InternalError("Interrupted".to_string()));
            }

            debug!("Asking for blocks [{},{})", i, range.end);
            let batch = canister
                .clone()
                .multi_query_blocks(Range {
                    start: i,
                    end: range.end,
                })
                .await
                .map_err(Error::InternalError)?;

            debug!("Got batch of len: {}", batch.len());
            if batch.is_empty() {
                return Err(Error::InternalError(format!(
                    "Couldn't fetch blocks [{},{}) (batch result empty)",
                    i, range.end
                )));
            }

            let mut hashed_batch = Vec::new();
            hashed_batch.reserve_exact(batch.len());
            for raw_block in batch {
                let block = Block::decode(raw_block.clone())
                    .map_err(|err| Error::InternalError(format!("Cannot decode block: {}", err)))?;
                if block.parent_hash != last_block_hash {
                    let err_msg = format!(
                        "Block at {}: parent hash mismatch. Expected: {:?}, got: {:?}",
                        i, last_block_hash, block.parent_hash
                    );
                    error!("{}", err_msg);
                    return Err(Error::InternalError(err_msg));
                }
                let hb = HashedBlock::hash_block(raw_block, last_block_hash, i);
                if i == range.end - 1 {
                    if let Some(verification_info) = &self.verification_info {
                        verify_block_hash(&certification, hb.hash, verification_info)
                            .map_err(Error::InternalError)?;
                    }
                }
                last_block_hash = Some(hb.hash);
                hashed_batch.push(hb);
                i += 1;
            }

            blockchain.add_blocks_batch(hashed_batch)?;
            self.metrics.set_synced_height(i - 1);

            if print_progress && (i - range.start) % 10000 == 0 {
                info!("Synced up to {}", i - 1);
            }
        }

        blockchain.block_store.mark_last_verified(range.end - 1)?;
        self.metrics.set_verified_height(range.end - 1);
        Ok(())
    }
}

#[cfg(test)]
mod test {

    use std::ops::Range;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use async_trait::async_trait;
    use ic_ledger_core::block::{BlockType, EncodedBlock, HashOf};
    use ic_ledger_core::timestamp::TimeStamp;
    use ic_ledger_core::Tokens;
    use ic_types::PrincipalId;
    use ledger_canister::{AccountIdentifier, Block, BlockHeight, Memo, TipOfChainRes};

    use crate::blocks_access::BlocksAccess;
    use crate::ledger_blocks_sync::LedgerBlocksSynchronizer;

    use super::NopMetrics;

    struct RangeOfBlocks {
        pub blocks: Vec<EncodedBlock>,
    }

    impl RangeOfBlocks {
        pub fn new(blocks: Vec<EncodedBlock>) -> Self {
            Self { blocks }
        }
    }

    #[async_trait]
    impl BlocksAccess for RangeOfBlocks {
        async fn query_raw_block(
            &self,
            height: BlockHeight,
        ) -> Result<Option<EncodedBlock>, String> {
            Ok(self.blocks.get(height as usize).cloned())
        }

        async fn query_tip(&self) -> Result<TipOfChainRes, String> {
            if self.blocks.is_empty() {
                Err("Not tip".to_string())
            } else {
                Ok(TipOfChainRes {
                    certification: None,
                    tip_index: (self.blocks.len() - 1) as u64,
                })
            }
        }

        async fn multi_query_blocks(
            self: Arc<Self>,
            range: Range<BlockHeight>,
        ) -> Result<Vec<EncodedBlock>, String> {
            Ok(self.blocks[range.start as usize..range.end as usize].to_vec())
        }
    }

    async fn new_ledger_blocks_synchronizer(
        blocks: Vec<EncodedBlock>,
    ) -> LedgerBlocksSynchronizer<RangeOfBlocks> {
        LedgerBlocksSynchronizer::new(
            Some(Arc::new(RangeOfBlocks::new(blocks))),
            /* store_location = */ None,
            /* store_max_blocks = */ None,
            /* verification_info = */ None,
            Box::new(NopMetrics {}),
        )
        .await
        .unwrap()
    }

    fn dummy_block(parent_hash: Option<HashOf<EncodedBlock>>) -> EncodedBlock {
        let operation = match parent_hash {
            Some(_) => {
                let from = AccountIdentifier::new(PrincipalId::new_anonymous(), None);
                let to = AccountIdentifier::new(PrincipalId::new_node_test_id(1), None);
                let amount = Tokens::from_e8s(100_000);
                let fee = Tokens::from_e8s(10_000);
                ledger_canister::Operation::Transfer {
                    from,
                    to,
                    amount,
                    fee,
                }
            }
            None => {
                let to = AccountIdentifier::new(PrincipalId::new_anonymous(), None);
                let amount = Tokens::from_e8s(100_000_000_000_000);
                ledger_canister::Operation::Mint { amount, to }
            }
        };
        let timestamp = TimeStamp::from_nanos_since_unix_epoch(
            1656347498000000000, /* 27 June 2022 18:31:38 GMT+02:00 DST */
        );
        Block::new(parent_hash, operation, Memo(0), timestamp, timestamp)
            .unwrap()
            .encode()
    }

    fn dummy_blocks(n: usize) -> Vec<EncodedBlock> {
        let mut res = vec![];
        let mut parent_hash = None;
        for _i in 0..n {
            let block = dummy_block(parent_hash);
            parent_hash = Some(Block::block_hash(&block));
            res.push(block);
        }
        res
    }

    #[tokio::test]
    async fn sync_empty_range_of_blocks() {
        let blocks_sync = new_ledger_blocks_synchronizer(vec![]).await;
        assert_eq!(None, blocks_sync.read_blocks().await.first().unwrap());
    }

    #[tokio::test]
    async fn sync_all_blocks() {
        let blocks = dummy_blocks(2);
        let blocks_sync = new_ledger_blocks_synchronizer(blocks.clone()).await;
        blocks_sync
            .sync_blocks(Arc::new(AtomicBool::new(false)), None)
            .await
            .unwrap();
        let actual_blocks = blocks_sync.read_blocks().await;
        // there isn't a blocks.len() to use, so we check that the last index + 1 gives error and then we check the blocks
        assert!(actual_blocks.get_verified_at(blocks.len() as u64).is_err());
        assert_eq!(
            blocks,
            vec![
                actual_blocks.get_verified_at(0).unwrap().block,
                actual_blocks.get_verified_at(1).unwrap().block
            ]
        )
    }

    #[tokio::test]
    async fn sync_blocks_in_2_steps() {
        let blocks = dummy_blocks(2);
        let blocks_sync = new_ledger_blocks_synchronizer(blocks.clone()).await;

        // sync 1
        blocks_sync
            .sync_blocks(Arc::new(AtomicBool::new(false)), Some(0))
            .await
            .unwrap();
        {
            let actual_blocks = blocks_sync.read_blocks().await;
            assert!(actual_blocks.get_verified_at(1).is_err());
            assert_eq!(
                *blocks.get(0).unwrap(),
                actual_blocks.get_verified_at(0).unwrap().block
            );
        }

        // sync 2
        blocks_sync
            .sync_blocks(Arc::new(AtomicBool::new(false)), Some(1))
            .await
            .unwrap();
        {
            let actual_blocks = blocks_sync.read_blocks().await;
            assert!(actual_blocks.get_verified_at(2).is_err());
            assert_eq!(
                *blocks.get(1).unwrap(),
                actual_blocks.get_verified_at(1).unwrap().block
            );
        }
    }
}