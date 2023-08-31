use {
    self::inscription_updater::InscriptionUpdater,
    super::{fetcher::Fetcher, *},
    base58::ToBase58,
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
    sat_ranges_since_flush: u64,
    outputs_cached: u64,
    outputs_inserted_since_flush: u64,
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

        wtx.open_table(WRITE_TRANSACTION_STARTING_BLOCK_COUNT_TO_TIMESTAMP)?
            .insert(
                &height,
                &SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|duration| duration.as_millis())
                    .unwrap_or(0),
            )?;

        let mut updater = Self {
            range_cache: HashMap::new(),
            height,
            index_sats: index.has_sat_index()?,
            sat_ranges_since_flush: 0,
            outputs_cached: 0,
            outputs_inserted_since_flush: 0,
            outputs_traversed: 0,
        };

        updater.update_index(index, wtx)
    }

    fn update_index<'index>(
        &mut self,
        index: &'index Index,
        mut wtx: WriteTransaction<'index>,
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
                self.commit(wtx, value_cache, addr_cache.clone())?;
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
                wtx.open_table(WRITE_TRANSACTION_STARTING_BLOCK_COUNT_TO_TIMESTAMP)?
                    .insert(
                        &self.height,
                        &SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .map(|duration| duration.as_millis())
                            .unwrap_or(0),
                    )?;
            }

            if INTERRUPTS.load(atomic::Ordering::Relaxed) > 0 {
                break;
            }
        }

        if uncommitted > 0 {
            self.commit(wtx, value_cache, addr_cache)?;
        }

        if let Some(progress_bar) = &mut progress_bar {
            progress_bar.finish_and_clear();
        }

        Ok(())
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

        let start = Instant::now();
        let mut sat_ranges_written = 0;
        let mut outputs_in_block = 0;

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
        let mut dex_to_state = wtx.open_table(DEX_TO_STATE)?;
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
            &mut dex_to_state,
            block.header.time,
            value_cache,
            addr_cache,
        )?;
        for (tx, txid) in block.txdata.iter().skip(1).chain(block.txdata.first()) {
            lost_sats += inscription_updater.index_transaction_inscriptions(tx, *txid, None)?;
        }

        statistic_to_count.insert(&Statistic::LostSats.key(), &lost_sats)?;

        height_to_block_hash.insert(&self.height, &block.header.block_hash().store())?;

        self.height += 1;
        self.outputs_traversed += outputs_in_block;

        log::info!(
            "Wrote {sat_ranges_written} sat ranges from {outputs_in_block} outputs in {} ms",
            (Instant::now() - start).as_millis(),
        );

        Ok(())
    }

    fn commit(
        &mut self,
        wtx: WriteTransaction,
        value_cache: HashMap<OutPoint, u64>,
        addr_cache: HashMap<OutPoint, UTXO>,
    ) -> Result {
        log::info!(
            "Committing at block height {}, {} outputs traversed, {} in map, {} cached",
            self.height,
            self.outputs_traversed,
            self.range_cache.len(),
            self.outputs_cached
        );

        if self.index_sats {
            log::info!(
                "Flushing {} entries ({:.1}% resulting from {} insertions) from memory to database",
                self.range_cache.len(),
                self.range_cache.len() as f64 / self.outputs_inserted_since_flush as f64 * 100.,
                self.outputs_inserted_since_flush,
            );

            let mut outpoint_to_sat_ranges = wtx.open_table(OUTPOINT_TO_SAT_RANGES)?;

            for (outpoint, sat_range) in self.range_cache.drain() {
                outpoint_to_sat_ranges.insert(&outpoint, sat_range.as_slice())?;
            }

            self.outputs_inserted_since_flush = 0;
        }

        {
            let mut outpoint_to_value = wtx.open_table(OUTPOINT_TO_VALUE)?;
            let mut outpoint_to_addr = wtx.open_table(OUTPOINT_TO_ADDRESS)?;
            for (outpoint, value) in value_cache {
                outpoint_to_value.insert(&outpoint.store(), &value)?;
            }
            for (outpoint, value) in addr_cache {
                outpoint_to_addr.insert(&outpoint.store(), &value.address)?;
            }
        }

        Index::increment_statistic(&wtx, Statistic::OutputsTraversed, self.outputs_traversed)?;
        self.outputs_traversed = 0;
        Index::increment_statistic(&wtx, Statistic::SatRanges, self.sat_ranges_since_flush)?;
        self.sat_ranges_since_flush = 0;
        Index::increment_statistic(&wtx, Statistic::Commits, 1)?;

        wtx.commit()?;
        Ok(())
    }
}
#[cfg(test)]
mod test {
    use anyhow::Ok;
    use base58::{FromBase58, ToBase58};
    use bitcoin::hashes::hex::{FromHex, ToHex};
    use bitcoin::util::key::PublicKey;
    use bitcoin::{Script, ScriptHash};
    use core::str::FromStr;
    use sha2::{Digest, Sha256};

    pub fn checksum(data: &[u8]) -> Vec<u8> {
        Sha256::digest(&Sha256::digest(&data)).to_vec()
    }
    fn script_to_address(script: &Script) -> String {
        let mut address = [0u8; 25];
        let script_vec = script.as_bytes();
        if script.is_p2pkh() {
            address[0] = 0x1e;
            address[1..21].copy_from_slice(&script_vec[3..23]);
            let sum = &checksum(&address[0..21])[0..4];

            address[21..25].copy_from_slice(sum);

            address.to_base58()
        } else if script.is_p2pk() {
            let pub_str = match script_vec[0] {
                33 => script_vec[1..34].to_hex(),
                65 => script_vec[1..66].to_hex(),
                _ => "".to_string(),
            };
            if pub_str.is_empty() {
                "".to_string()
            } else {
                let pubkey = PublicKey::from_str(&pub_str).unwrap();
                address[0] = 0x1e;
                address[1..21].copy_from_slice(&pubkey.pubkey_hash().to_vec());
                let sum = &checksum(&address[0..21])[0..4];
                address[21..25].copy_from_slice(sum);
                address.to_base58()
            }
        } else if script.is_p2sh() {
            address[0] = 0x16;
            address[1..21].copy_from_slice(&script_vec[2..22]);
            let sum = &checksum(&address[0..21])[0..4];

            address[21..25].copy_from_slice(sum);

            address.to_base58()
        } else {
            "".to_string()
        }
    }
    #[test]
    fn script_to_address_test() {
        // p2sh
        let mut script =
            Script::from_hex("a914cb38234a098595a560005a04e10798cf2b66fef387").unwrap();

        println!("{:?}", script);
        let addr = script_to_address(&script);
        assert_eq!("AAxo5rgaAyRRxVi3h1QjqZ5Q8RXqx4DNUs", addr);

        // p2pk
        script = Script::from_hex(
            "210338bf57d51a50184cf5ef0dc42ecd519fb19e24574c057620262cc1df94da2ae5ac",
        )
        .unwrap();

        println!("{:?}", script);
        let addr = script_to_address(&script);
        assert_eq!("DLAznsPDLDRgsVcTFWRMYMG5uH6GddDtv8", addr);

        // p2pkh
        script = Script::from_hex("76a914bc91bcb2b7b6fcb76976c21cc5fb04c3de1e5f9288ac").unwrap();

        println!("{:?}", script);
        let addr = script_to_address(&script);
        assert_eq!("DNLAAKAJZf4h8pheVSquisEyKCqDNDRGYm", addr);
    }
    #[test]
    fn pubkey_to_address_test() {
        let mut address = [0u8; 25];
        let pubkey = PublicKey::from_str(
            "0338bf57d51a50184cf5ef0dc42ecd519fb19e24574c057620262cc1df94da2ae5",
        )
        .unwrap();
        address[0] = 0x1e;
        address[1..21].copy_from_slice(&pubkey.pubkey_hash().to_vec());
        let sum = &checksum(&address[0..21])[0..4];
        address[21..25].copy_from_slice(sum);
        println!("{}", address.to_base58());
    }
}
