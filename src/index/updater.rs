use std::{thread, time};
use {
    self::inscription_updater::InscriptionUpdater,
    super::{fetcher::Fetcher, *},
    bitcoin::hashes::hex::ToHex,
    bitcoin::{util::key::PublicKey, Script},
    core::str::FromStr,
    futures::future::try_join_all,
    sha2::{Digest, Sha256},
    std::sync::mpsc,
    tokio::sync::mpsc::{error::TryRecvError, Receiver, Sender},
};
mod inscription_updater;
#[derive(Copy, Clone)]
pub struct UTXO {
    address: [u8; 25],
}
#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub enum ACTBLK {
    Old,
    New(u64),
}
#[derive(Debug, Clone)]
pub struct ADDRSTATE {
    amt: u128,
    height: ACTBLK,
}

pub struct DEXSTATE {
    height: ACTBLK,
    number: u64,
    reserve0: u128,
    reserve1: u128,
}

impl DEXSTATE {
    pub fn get_id(tick0: &String, tick1: &String) -> Option<u64> {
        if tick0.is_empty() || tick1.is_empty() {
            return None;
        }
        let tick = format!("{tick0}{tick1}");
        let mut hasher = Sha256::new();
        hasher.update(tick);
        let id = u64::from_ne_bytes(hasher.finalize()[0..8].try_into().unwrap());

        return Some(id);
    }

    pub fn to_hex(id: u64) -> String {
        let ids = format!("{:X}", id);
        return ids;
    }

    pub fn from_hex(drc: &String) -> Option<u64> {
        match u64::from_str_radix(&drc, 16) {
            Ok(id) => Some(id),
            Err(_e) => None,
        }
    }
}
struct BlockData {
    header: BlockHeader,
    txdata: Vec<(Transaction, Txid)>,
}

impl From<Block> for BlockData {
    fn from(block: Block) -> Self {
        BlockData {
            header: block.header,
            txdata: block
                .txdata
                .into_iter()
                .map(|transaction| {
                    let txid = transaction.txid();
                    (transaction, txid)
                })
                .collect(),
        }
    }
}

pub(crate) struct Updater {
    range_cache: HashMap<OutPointValue, Vec<u8>>,
    height: u64,
    index_sats: bool,
    outputs_cached: u64,
    outputs_traversed: u64,
}

impl Updater {
    pub(crate) fn update(index: &Index) -> Result {
        let wtx = index.begin_write()?;

        let height = wtx
            .open_table(HEIGHT_TO_BLOCK_HASH)?
            .range(0..)?
            .rev()
            .next()
            .map(|(height, _hash)| height.value() + 1)
            .unwrap_or(0);

        let mut f = File::create("unix.txt").unwrap();
        let mut updater = Self {
            range_cache: HashMap::new(),
            height,
            index_sats: index.has_sat_index()?,
            outputs_cached: 0,
            outputs_traversed: 0,
        };

        updater.update_index(index, wtx, f)
    }

    fn update_index<'index>(
        &mut self,
        index: &'index Index,
        mut wtx: WriteTransaction<'index>,
        mut f: File,
    ) -> Result {
        let starting_height = index.client.get_block_count()? + 1;

        let mut progress_bar = if cfg!(test)
            || log_enabled!(log::Level::Info)
            || starting_height <= self.height
            || integration_test()
        {
            None
        } else {
            let progress_bar = ProgressBar::new(starting_height);
            progress_bar.set_position(self.height);
            progress_bar.set_style(
                ProgressStyle::with_template("[indexing blocks] {wide_bar} {pos}/{len}").unwrap(),
            );
            Some(progress_bar)
        };

        let rx = Self::fetch_blocks_from(index, self.height, self.index_sats)?;

        let (mut outpoint_sender, mut value_receiver) = Self::spawn_fetcher(index)?;

        let mut uncommitted = 0;
        let mut value_cache = HashMap::new();
        let mut addr_cache = HashMap::new();
        let mut drc_state = match Self::get_drc(index) {
            Some(act_state) => act_state,
            None => HashMap::new(),
        };
        let mut dex_state = match Self::get_dex(index) {
            Some(dex_state) => dex_state,
            None => HashMap::new(),
        };

        loop {
            let block = match rx.recv() {
                Ok(block) => block,
                Err(mpsc::RecvError) => break,
            };

            self.index_block(
                index,
                &mut outpoint_sender,
                &mut value_receiver,
                &mut wtx,
                block,
                &mut value_cache,
                &mut addr_cache,
                &mut drc_state,
                &mut dex_state,
                &mut f,
            )?;

            if let Some(progress_bar) = &mut progress_bar {
                progress_bar.inc(1);

                if progress_bar.position() > progress_bar.length().unwrap() {
                    if let Ok(count) = index.client.get_block_count() {
                        progress_bar.set_length(count + 1);
                    } else {
                        log::warn!("Failed to fetch latest block height");
                    }
                }
            }

            uncommitted += 1;

            if uncommitted >= 500 {
                self.commit(wtx, &mut drc_state)?;
                value_cache = HashMap::new();
                uncommitted = 0;
                wtx = index.begin_write()?;
                let height = wtx
                    .open_table(HEIGHT_TO_BLOCK_HASH)?
                    .range(0..)?
                    .rev()
                    .next()
                    .map(|(height, _hash)| height.value() + 1)
                    .unwrap_or(0);
                if height != self.height {
                    // another update has run between committing and beginning the new
                    // write transaction
                    break;
                }
            }

            if INTERRUPTS.load(atomic::Ordering::Relaxed) > 0 {
                break;
            }
        }

        if uncommitted > 0 {
            self.commit(wtx, &mut drc_state)?;
        }

        if let Some(progress_bar) = &mut progress_bar {
            progress_bar.finish_and_clear();
        }

        Ok(())
    }

    pub(crate) fn get_drc(
        index: &Index,
    ) -> Option<HashMap<String, HashMap<String, Vec<ADDRSTATE>>>> {
        let rtx = index.database.begin_read().unwrap();
        let rtbl = match rtx.open_multimap_table(DRC_TO_ACCOUNT) {
            Err(_) => return None,
            Ok(rtbl) => rtbl,
        };
        if rtbl.is_empty().unwrap() {
            return None;
        }
        let mut iter = rtbl.iter().unwrap();
        let mut act_state = HashMap::new();
        loop {
            if let Some((key, value)) = iter.next() {
                let drc = key.value().to_string();
                let res = Index::get_vec(&rtbl, &drc);
                let mut addr_state = HashMap::new();
                for item in res {
                    let s = DexInscription::load(item);

                    addr_state.insert(
                        s.addr,
                        vec![ADDRSTATE {
                            amt: s.amt,
                            height: ACTBLK::Old,
                        }],
                    );
                }
                act_state.insert(drc, addr_state);
            } else {
                break;
            }
        }

        Some(act_state)
    }

    pub(crate) fn get_dex(index: &Index) -> Option<HashMap<u64, Vec<DEXSTATE>>> {
        let rtx = index.database.begin_read().unwrap();
        let rtbl = match rtx.open_table(ID_TO_DEX) {
            Err(_) => return None,
            Ok(rtbl) => rtbl,
        };
        if rtbl.is_empty().unwrap() {
            return None;
        }
        let mut iter = rtbl.iter().unwrap();
        let mut dex_state = HashMap::new();
        loop {
            if let Some((key, value)) = iter.next() {
                let id = key.value();
                let content = value.value();

                dex_state.insert(
                    id,
                    vec![DEXSTATE {
                        height: ACTBLK::Old,
                        number: content.1,
                        reserve0: content.2,
                        reserve1: content.3,
                    }],
                );
            } else {
                break;
            }
        }
        Some(dex_state)
    }

    fn fetch_blocks_from(
        index: &Index,
        mut height: u64,
        index_sats: bool,
    ) -> Result<mpsc::Receiver<BlockData>> {
        let (tx, rx) = mpsc::sync_channel(32);

        let height_limit = index.height_limit;

        let client = Client::new(&index.rpc_url, index.auth.clone())
            .context("failed to connect to RPC URL")?;

        let first_inscription_height = index.first_inscription_height;

        thread::spawn(move || loop {
            if let Some(height_limit) = height_limit {
                if height >= height_limit {
                    break;
                }
            }

            loop {
                if let Ok(count) = client.get_block_count() {
                    if height > count {
                        log::info!(
                            "delay 120 blocks to syn: ord height {}, chain height {}",
                            height,
                            count
                        );
                        thread::sleep(Duration::from_secs(120));
                    } else {
                        log::info!(
                            "blocks to syn: ord height {}, chain height {}",
                            height,
                            count
                        );
                        break;
                    }
                } else {
                    log::warn!("Failed to fetch latest block height, waiting to retry");
                    thread::sleep(Duration::from_secs(30));
                }
            }

            match Self::get_block_with_retries(
                &client,
                height,
                index_sats,
                first_inscription_height,
            ) {
                Ok(Some(block)) => {
                    if let Err(err) = tx.send(block.into()) {
                        log::info!("Block receiver disconnected: {err}");
                        break;
                    }
                    height += 1;
                }
                Ok(None) => break,
                Err(err) => {
                    log::error!("failed to fetch block {height}: {err}");
                    break;
                }
            }
        });

        Ok(rx)
    }

    fn get_block_with_retries(
        client: &Client,
        height: u64,
        index_sats: bool,
        first_inscription_height: u64,
    ) -> Result<Option<Block>> {
        let mut errors = 0;
        loop {
            match client
                .get_block_hash(height)
                .into_option()
                .and_then(|option| {
                    option
                        .map(|hash| {
                            if index_sats || height >= first_inscription_height {
                                Ok(client.get_block(&hash)?)
                            } else {
                                Ok(Block {
                                    header: client.get_block_header(&hash)?,
                                    txdata: Vec::new(),
                                })
                            }
                        })
                        .transpose()
                }) {
                Err(err) => {
                    if cfg!(test) {
                        return Err(err);
                    }

                    errors += 1;
                    let seconds = 1 << errors;
                    log::warn!("failed to fetch block {height}, retrying in {seconds}s: {err}");

                    if seconds > 120 {
                        log::error!("would sleep for more than 120s, giving up");
                        return Err(err);
                    }

                    thread::sleep(Duration::from_secs(seconds));
                }
                Ok(result) => return Ok(result),
            }
        }
    }

    fn spawn_fetcher(index: &Index) -> Result<(Sender<OutPoint>, Receiver<u64>)> {
        let fetcher = Fetcher::new(&index.rpc_url, index.auth.clone())?;

        // Not sure if any block has more than 20k inputs, but none so far after first inscription block
        const CHANNEL_BUFFER_SIZE: usize = 20_000;
        let (outpoint_sender, mut outpoint_receiver) =
            tokio::sync::mpsc::channel::<OutPoint>(CHANNEL_BUFFER_SIZE);
        let (value_sender, value_receiver) = tokio::sync::mpsc::channel::<u64>(CHANNEL_BUFFER_SIZE);

        // Batch 2048 missing inputs at a time. Arbitrarily chosen for now, maybe higher or lower can be faster?
        // Did rudimentary benchmarks with 1024 and 4096 and time was roughly the same.
        const BATCH_SIZE: usize = 2048;
        // Default rpcworkqueue in bitcoind is 16, meaning more than 16 concurrent requests will be rejected.
        // Since we are already requesting blocks on a separate thread, and we don't want to break if anything
        // else runs a request, we keep this to 12.
        const PARALLEL_REQUESTS: usize = 12;

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                loop {
                    let Some(outpoint) = outpoint_receiver.recv().await else {
                        log::debug!("Outpoint channel closed");
                        return;
                    };
                    // There's no try_iter on tokio::sync::mpsc::Receiver like std::sync::mpsc::Receiver.
                    // So we just loop until BATCH_SIZE doing try_recv until it returns None.
                    let mut outpoints = vec![outpoint];
                    for _ in 0..BATCH_SIZE - 1 {
                        let Ok(outpoint) = outpoint_receiver.try_recv() else {
                            break;
                        };
                        outpoints.push(outpoint);
                    }
                    // Break outpoints into chunks for parallel requests
                    let chunk_size = (outpoints.len() / PARALLEL_REQUESTS) + 1;
                    let mut futs = Vec::with_capacity(PARALLEL_REQUESTS);
                    for chunk in outpoints.chunks(chunk_size) {
                        let txids = chunk.iter().map(|outpoint| outpoint.txid).collect();

                        let fut = fetcher.get_transactions(txids);
                        futs.push(fut);
                    }
                    let txs = match try_join_all(futs).await {
                        Ok(txs) => txs,
                        Err(e) => {
                            log::error!("Couldn't receive txs {e}");
                            return;
                        }
                    };

                    // Send all tx output values back in order
                    for (i, tx) in txs.iter().flatten().enumerate() {
                        let tx_out = &tx.output[usize::try_from(outpoints[i].vout).unwrap()];

                        let Ok(_) = value_sender.send(tx_out.value).await else {
                            log::error!("Value channel closed unexpectedly");
                            return;
                        };
                    }
                }
            })
        });

        Ok((outpoint_sender, value_receiver))
    }

    fn index_block(
        &mut self,
        index: &Index,
        outpoint_sender: &mut Sender<OutPoint>,
        value_receiver: &mut Receiver<u64>,
        wtx: &mut WriteTransaction,
        block: BlockData,
        value_cache: &mut HashMap<OutPoint, u64>,
        addr_cache: &mut HashMap<OutPoint, UTXO>,
        drc_state: &mut HashMap<String, HashMap<String, Vec<ADDRSTATE>>>,
        dex_state: &mut HashMap<u64, Vec<DEXSTATE>>,
        f: &mut File,
    ) -> Result<()> {
        // If value_receiver still has values something went wrong with the last block
        // Could be an assert, shouldn't recover from this and commit the last block
        let Err(TryRecvError::Empty) = value_receiver.try_recv() else {
            return Err(anyhow!("Previous block did not consume all input values"));
        };

        let mut outpoint_to_value = wtx.open_table(OUTPOINT_TO_VALUE)?;
        let mut outpoint_to_addr = wtx.open_table(OUTPOINT_TO_ADDRESS)?;

        let index_inscriptions = self.height >= index.first_inscription_height;

        if index_inscriptions {
            thread::sleep(Duration::MAX);
            // Send all missing input outpoints to be fetched right away
            let txids = block
                .txdata
                .iter()
                .map(|(_, txid)| txid)
                .collect::<HashSet<_>>();

            for (tx, _) in &block.txdata {
                for input in &tx.input {
                    let prev_output = input.previous_output;
                    //We don't need coinbase input value
                    if prev_output.is_null() {
                        continue;
                    }
                    // We don't need input values from txs earlier in the block, since they'll be added to value_cache
                    // when the tx is indexed
                    if txids.contains(&prev_output.txid) {
                        continue;
                    }
                    // We don't need input values we already have in our value_cache from earlier blocks
                    if value_cache.contains_key(&prev_output) {
                        continue;
                    }
                    // We don't need input values we already have in our outpoint_to_value table from earlier blocks that
                    // were committed to db already
                    if outpoint_to_value.get(&prev_output.store())?.is_some() {
                        continue;
                    }
                    // We don't know the value of this tx input. Send this outpoint to background thread to be fetched
                    outpoint_sender.blocking_send(prev_output)?;
                }
            }
        }

        let mut height_to_block_hash = wtx.open_table(HEIGHT_TO_BLOCK_HASH)?;

        let time = timestamp(block.header.time);

        log::info!(
            "Block {} at {} with {} transactions…",
            self.height,
            time,
            block.txdata.len()
        );

        if let Some(prev_height) = self.height.checked_sub(1) {
            let prev_hash = height_to_block_hash.get(&prev_height)?.unwrap();

            if prev_hash.value() != block.header.prev_blockhash.as_ref() {
                index.reorged.store(true, atomic::Ordering::Relaxed);
                return Err(anyhow!("reorg detected at or before {prev_height}"));
            }
        }

        let mut inscription_id_to_inscription_entry =
            wtx.open_table(INSCRIPTION_ID_TO_INSCRIPTION_ENTRY)?;
        let mut inscription_id_to_satpoint = wtx.open_table(INSCRIPTION_ID_TO_SATPOINT)?;
        let mut inscription_id_to_txids = wtx.open_table(INSCRIPTION_ID_TO_TXIDS)?;
        let mut inscription_txid_to_tx = wtx.open_table(INSCRIPTION_TXID_TO_TX)?;
        let mut inscription_number_to_inscription_id =
            wtx.open_table(INSCRIPTION_NUMBER_TO_INSCRIPTION_ID)?;
        let mut sat_to_inscription_id = wtx.open_table(SAT_TO_INSCRIPTION_ID)?;
        let mut satpoint_to_inscription_id = wtx.open_table(SATPOINT_TO_INSCRIPTION_ID)?;
        let mut statistic_to_count = wtx.open_table(STATISTIC_TO_COUNT)?;

        let mut lost_sats = statistic_to_count
            .get(&Statistic::LostSats.key())?
            .map(|lost_sats| lost_sats.value())
            .unwrap_or(0);
        let mut id_to_dex = wtx.open_table(ID_TO_DEX)?;
        let mut drc_to_act = wtx.open_multimap_table(DRC_TO_ACCOUNT)?;
        let mut inscription_updater = InscriptionUpdater::new(
            self.height,
            &mut inscription_id_to_satpoint,
            &mut inscription_id_to_txids,
            &mut inscription_txid_to_tx,
            value_receiver,
            &mut inscription_id_to_inscription_entry,
            lost_sats,
            &mut inscription_number_to_inscription_id,
            &mut outpoint_to_value,
            &mut outpoint_to_addr,
            &mut sat_to_inscription_id,
            &mut satpoint_to_inscription_id,
            &mut id_to_dex,
            &mut drc_to_act,
            block.header.time,
            value_cache,
            addr_cache,
            drc_state,
            dex_state,
        )?;
        for (tx, txid) in block.txdata.iter().skip(1).chain(block.txdata.first()) {
            lost_sats +=
                inscription_updater.index_transaction_inscriptions(index, tx, *txid, None, f)?;
        }

        //statistic_to_count.insert(&Statistic::LostSats.key(), &lost_sats)?;

        height_to_block_hash.insert(&self.height, &block.header.block_hash().store())?;

        self.height += 1;

        Ok(())
    }

    fn commit(
        &mut self,
        wtx: WriteTransaction,
        drc_state: &mut HashMap<String, HashMap<String, Vec<ADDRSTATE>>>,
    ) -> Result {
        log::info!(
            "Committing at block height {}, {} outputs traversed, {} in map, {} cached",
            self.height,
            self.outputs_traversed,
            self.range_cache.len(),
            self.outputs_cached
        );
        let mut addr_map_new = HashMap::new();
        let mut addr_map_old = HashMap::new();
        let height_update = ACTBLK::New(self.height);
        let key = "UNIX";
        let addr_state = drc_state.get_mut(key).expect("failed to get UNIX");
        for (addr, amt_vec) in addr_state.into_iter() {
            if amt_vec.len() == 1 && amt_vec[0].height == ACTBLK::Old {
                continue;
            }
            let item_old = amt_vec[0].clone();
            let mut item_pre = amt_vec[0].clone();
            loop {
                if amt_vec[0].height < height_update {
                    item_pre = amt_vec[0].clone();
                    amt_vec.remove(0);
                }
                if amt_vec.is_empty() || amt_vec[0].height >= height_update {
                    if item_pre.height != ACTBLK::Old {
                        addr_map_new.insert(addr.clone(), item_pre.amt);
                        if item_old.height == ACTBLK::Old {
                            addr_map_old.insert(addr, item_old.amt);
                        }
                    }

                    break;
                }
            }
        }

        {
            let mut wtbl = wtx.open_multimap_table(DRC_TO_ACCOUNT).unwrap();
            for (addr, amt) in addr_map_new.into_iter() {
                println!("insert {}:{}", addr, amt);
                wtbl.insert(
                    key,
                    &DexInscription::store(DexInscription {
                        addr: addr.clone(),
                        amt: amt,
                    }),
                )
                .unwrap();
                if let Some(amt_old) = addr_map_old.get(&addr) {
                    println!("remove {}:{}", addr, amt_old);
                    wtbl.remove(
                        key,
                        &DexInscription::store(DexInscription {
                            addr: addr,
                            amt: *amt_old,
                        }),
                    )
                    .unwrap();
                }
            }
        }

        wtx.commit()?;
        Ok(())
    }
}
