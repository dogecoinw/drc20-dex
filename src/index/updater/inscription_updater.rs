use super::*;
use crate::{
    index::entry::{DexEntry, DexInscription},
    inscription::ParsedInscription,
};
use num_integer::Roots;
use serde_json::Value;

pub(super) struct Flotsam {
    inscription_id: InscriptionId,
    offset: u64,
    origin: Origin,
}

enum Origin {
    New(u64),
    Old(SatPoint),
}

#[derive(Debug, PartialEq)]
enum InscriptionOp {
    Create,
    Mint,
    Transfer,
    Add,
    Burn,
    Swap,
}

impl FromStr for InscriptionOp {
    type Err = ();

    fn from_str(input: &str) -> Result<InscriptionOp, Self::Err> {
        match input {
            "create" => Ok(InscriptionOp::Create),
            "mint" => Ok(InscriptionOp::Mint),
            "transfer" => Ok(InscriptionOp::Transfer),
            "add" => Ok(InscriptionOp::Add),
            "burn" => Ok(InscriptionOp::Burn),
            "swap" => Ok(InscriptionOp::Swap),
            _ => Err(()),
        }
    }
}

pub(super) struct InscriptionUpdater<'a, 'db, 'tx> {
    flotsam: Vec<Flotsam>,
    height: u64,
    id_to_satpoint: &'a mut Table<'db, 'tx, &'static InscriptionIdValue, &'static SatPointValue>,
    id_to_txids: &'a mut Table<'db, 'tx, &'static InscriptionIdValue, &'static [u8]>,
    txid_to_tx: &'a mut Table<'db, 'tx, &'static [u8], &'static [u8]>,
    value_receiver: &'a mut Receiver<u64>,
    id_to_entry: &'a mut Table<'db, 'tx, &'static InscriptionIdValue, InscriptionEntryValue>,
    lost_sats: u64,
    next_number: u64,
    number_to_id: &'a mut Table<'db, 'tx, u64, &'static InscriptionIdValue>,
    outpoint_to_value: &'a mut Table<'db, 'tx, &'static OutPointValue, u64>,
    outpoint_to_addr: &'a mut Table<'db, 'tx, &'static OutPointValue, &'static [u8; 25]>,
    reward: u64,
    sat_to_inscription_id: &'a mut Table<'db, 'tx, u128, &'static InscriptionIdValue>,
    satpoint_to_id: &'a mut Table<'db, 'tx, &'static SatPointValue, &'static InscriptionIdValue>,
    id_to_dex: &'a mut Table<'db, 'tx, u64, DexEntryValue>,
    drc_to_act: &'a mut MultimapTable<'db, 'tx, &'static str, &'static DexInscriptionValue>,
    timestamp: u32,
    value_cache: &'a mut HashMap<OutPoint, u64>,
    addr_cache: &'a mut HashMap<OutPoint, UTXO>,
}

impl<'a, 'db, 'tx> InscriptionUpdater<'a, 'db, 'tx> {
    pub(super) fn new(
        height: u64,
        id_to_satpoint: &'a mut Table<
            'db,
            'tx,
            &'static InscriptionIdValue,
            &'static SatPointValue,
        >,
        id_to_txids: &'a mut Table<'db, 'tx, &'static InscriptionIdValue, &'static [u8]>,
        txid_to_tx: &'a mut Table<'db, 'tx, &'static [u8], &'static [u8]>,
        value_receiver: &'a mut Receiver<u64>,
        id_to_entry: &'a mut Table<'db, 'tx, &'static InscriptionIdValue, InscriptionEntryValue>,
        lost_sats: u64,
        number_to_id: &'a mut Table<'db, 'tx, u64, &'static InscriptionIdValue>,
        outpoint_to_value: &'a mut Table<'db, 'tx, &'static OutPointValue, u64>,
        outpoint_to_addr: &'a mut Table<'db, 'tx, &'static OutPointValue, &[u8; 25]>,
        sat_to_inscription_id: &'a mut Table<'db, 'tx, u128, &'static InscriptionIdValue>,
        satpoint_to_id: &'a mut Table<
            'db,
            'tx,
            &'static SatPointValue,
            &'static InscriptionIdValue,
        >,
        id_to_dex: &'a mut Table<'db, 'tx, u64, DexEntryValue>,
        drc_to_act: &'a mut MultimapTable<'db, 'tx, &str, &'static DexInscriptionValue>,
        timestamp: u32,
        value_cache: &'a mut HashMap<OutPoint, u64>,
        addr_cache: &'a mut HashMap<OutPoint, UTXO>,
    ) -> Result<Self> {
        let next_number = number_to_id
            .iter()?
            .rev()
            .map(|(number, _id)| number.value() + 1)
            .next()
            .unwrap_or(0);

        Ok(Self {
            flotsam: Vec::new(),
            height,
            id_to_satpoint,
            id_to_txids,
            txid_to_tx,
            value_receiver,
            id_to_entry,
            lost_sats,
            next_number,
            number_to_id,
            outpoint_to_value,
            outpoint_to_addr,
            reward: Height(height).subsidy(),
            sat_to_inscription_id,
            satpoint_to_id,
            id_to_dex,
            drc_to_act,
            timestamp,
            value_cache,
            addr_cache,
        })
    }

    pub(super) fn index_transaction_inscriptions(
        &mut self,
        index: &Index,
        tx: &Transaction,
        txid: Txid,
        input_sat_ranges: Option<&VecDeque<(u128, u128)>>,
    ) -> Result<u64> {
        let mut inscriptions = Vec::new();

        let mut input_value = 0;
        for tx_in in &tx.input {
            if tx_in.previous_output.is_null() {
                input_value += Height(self.height).subsidy();
            } else {
                for (old_satpoint, inscription_id) in
                    Index::inscriptions_on_output(self.satpoint_to_id, tx_in.previous_output)?
                {
                    inscriptions.push(Flotsam {
                        offset: input_value + old_satpoint.offset,
                        inscription_id,
                        origin: Origin::Old(old_satpoint),
                    });
                }

                input_value += if let Some(value) = self.value_cache.remove(&tx_in.previous_output)
                {
                    self.addr_cache.remove(&tx_in.previous_output);
                    value
                } else if let Some(value) = self
                    .outpoint_to_value
                    .remove(&tx_in.previous_output.store())?
                {
                    self.outpoint_to_addr
                        .remove(&tx_in.previous_output.store())
                        .unwrap();
                    value.value()
                } else {
                    self.value_receiver.blocking_recv().ok_or_else(|| {
                        anyhow!(
                            "failed to get transaction for {}",
                            tx_in.previous_output.txid
                        )
                    })?
                }
            }
        }

        if inscriptions.iter().all(|flotsam| flotsam.offset != 0) {
            let previous_txid = tx.input[0].previous_output.txid;
            let previous_txid_bytes: [u8; 32] = previous_txid.into_inner();
            let mut txids_vec = vec![];

            match Inscription::from_transactions(&tx) {
                ParsedInscription::None => {
                    // todo: clean up db
                }

                ParsedInscription::Complete(inscription) => {
                    let mut tx_buf = vec![];
                    tx.consensus_encode(&mut tx_buf)?;
                    self.txid_to_tx
                        .insert(&txid.into_inner().as_slice(), tx_buf.as_slice())?;

                    let mut txid_vec = txid.into_inner().to_vec();
                    txids_vec.append(&mut txid_vec);

                    let mut inscription_id = [0_u8; 36];
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            txids_vec.as_ptr(),
                            inscription_id.as_mut_ptr(),
                            32,
                        )
                    }
                    self.id_to_txids
                        .insert(&inscription_id, txids_vec.as_slice())?;

                    let og_inscription_id = InscriptionId {
                        txid: Txid::from_slice(&txids_vec[0..32]).unwrap(),
                        index: 0,
                    };

                    inscriptions.push(Flotsam {
                        inscription_id: og_inscription_id,
                        offset: 0,
                        origin: Origin::New(
                            input_value - tx.output.iter().map(|txout| txout.value).sum::<u64>(),
                        ),
                    });
                    let content_str = inscription.content_type().unwrap().to_string();
                    if content_str == "text/plain;charset=utf-8" {
                        let body_str =
                            String::from_utf8(inscription.body().unwrap().to_vec()).unwrap();
                        let object: Value = serde_json::from_str(&body_str.to_owned()).unwrap();
                        if let Some(op) = object.get("op") {
                            match InscriptionOp::from_str(&op.to_string().to_owned()) {
                                Ok(InscriptionOp::Mint) => {
                                    // TODO:
                                }
                                Ok(InscriptionOp::Transfer) => {
                                    let mut drc = "".to_string();
                                    if let Some(tick) = object.get("tick") {
                                        let tick = tick.to_string();
                                        drc = tick.trim_matches('"').to_string();
                                    }

                                    let mut amount: u128 = 0;
                                    if let Some(amt) = object.get("amt") {
                                        let amt_str = amt.to_string();
                                        let amt_str = amt_str.trim_matches('"');
                                        amount = amt_str.parse().unwrap();
                                    }

                                    if let Some((src, dsts)) =
                                        Self::drc_transfer_parse(tx, index).unwrap()
                                    {
                                        for dst in dsts {
                                            match index.transfer_drc(
                                                drc.as_str(),
                                                src.as_str(),
                                                dst.as_str(),
                                                amount,
                                            ) {
                                                Ok(()) => {}
                                                Err(e) => {
                                                    println!("{}", e)
                                                }
                                            }
                                        }
                                    }
                                }
                                Ok(InscriptionOp::Create) => {
                                    let mut tick0 = "".to_string();
                                    let mut tick1: String = "".to_string();

                                    if let Some(tick) = object.get("tick0") {
                                        let tick = tick.to_string();
                                        tick0 = tick.trim_matches('"').to_string();
                                    }
                                    if let Some(tick) = object.get("tick1") {
                                        let tick = tick.to_string();
                                        tick1 = tick.trim_matches('"').to_string();
                                    }
                                    match index.get_dex_id(&tick0, &tick1) {
                                        None => {}
                                        Some(id) => {
                                            if self.id_to_dex.get(id)?.is_none() {
                                                self.id_to_dex
                                                    .insert(id, (self.height, 0, 0, 0))?;
                                            }
                                        }
                                    }
                                }
                                Ok(InscriptionOp::Add) => {
                                    let mut tick0 = "".to_string();
                                    let mut tick1: String = "".to_string();
                                    let mut amt0: u128 = 0;
                                    let mut amt1: u128 = 0;
                                    if let Some(tick) = object.get("tick0") {
                                        let tick = tick.to_string();
                                        tick0 = tick.trim_matches('"').to_string();
                                    }
                                    if let Some(tick) = object.get("tick1") {
                                        let tick = tick.to_string();
                                        tick1 = tick.trim_matches('"').to_string();
                                    }
                                    if let Some(amt) = object.get("amt0") {
                                        let amt_str = amt.to_string();
                                        let amt_str = amt_str.trim_matches('"');
                                        amt0 = amt_str.parse().unwrap();
                                    }
                                    if let Some(amt) = object.get("amt1") {
                                        let amt_str = amt.to_string();
                                        let amt_str = amt_str.trim_matches('"');
                                        amt1 = amt_str.parse().unwrap();
                                    }
                                    let addr = Self::drc_add_parse(&tx);
                                    match index.get_dex_id(&tick0, &tick1) {
                                        None => {}
                                        Some(id) => {
                                            if amt0 != 0 && amt1 != 0 {
                                                if let Some(mut state) =
                                                    Index::id_to_dex(index, id).unwrap()
                                                {
                                                    let liquidity = Self::add_liquidity(
                                                        amt0,
                                                        amt1,
                                                        &mut state.2,
                                                        &mut state.3,
                                                    );
                                                    state.0 = self.height;
                                                    state.1 += 1;
                                                    self.id_to_dex.insert(id, state)?;
                                                    if let Some(dex) =
                                                        index.get_dex_name(&tick0, &tick1)
                                                    {
                                                        index
                                                            .insert_drc_act(
                                                                dex.0.as_str(),
                                                                &DexInscription {
                                                                    addr: (addr),
                                                                    amt: (liquidity),
                                                                },
                                                            )
                                                            .unwrap();
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Ok(InscriptionOp::Swap) => {
                                    let mut tick0 = "".to_string();
                                    let mut tick1: String = "".to_string();
                                    let mut amt0: u128 = 0;
                                    let mut amt1: u128 = 0;
                                    if let Some(tick) = object.get("tick0") {
                                        let tick = tick.to_string();
                                        tick0 = tick.trim_matches('"').to_string();
                                    }
                                    if let Some(tick) = object.get("tick1") {
                                        let tick = tick.to_string();
                                        tick1 = tick.trim_matches('"').to_string();
                                    }
                                    if let Some(amt) = object.get("amt0") {
                                        let amt_str = amt.to_string();
                                        let amt_str = amt_str.trim_matches('"');
                                        amt0 = amt_str.parse().unwrap();
                                    }
                                    if let Some(amt) = object.get("amt1") {
                                        let amt_str = amt.to_string();
                                        let amt_str = amt_str.trim_matches('"');
                                        amt1 = amt_str.parse().unwrap();
                                    }
                                    let addr = Self::drc_add_parse(&tx);
                                    match index.get_dex_id(&tick0, &tick1) {
                                        None => {}
                                        Some(id) => {
                                            if let Some(mut state) =
                                                Index::id_to_dex(index, id).unwrap()
                                            {
                                                if let Some(dex) =
                                                    index.get_dex_name(&tick0, &tick1)
                                                {
                                                    if dex.1 {
                                                        let amt_out = Self::swap(
                                                            amt0,
                                                            &mut state.2,
                                                            &mut state.3,
                                                        );
                                                        if amt1 > amt_out {
                                                            state.0 = self.height;
                                                            state.1 += 1;
                                                            self.id_to_dex.insert(id, state)?;
                                                        }
                                                    } else {
                                                        let amt_out = Self::swap(
                                                            amt1,
                                                            &mut state.3,
                                                            &mut state.2,
                                                        );
                                                        if amt0 > amt_out {
                                                            state.0 = self.height;
                                                            state.1 += 1;
                                                            self.id_to_dex.insert(id, state)?;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Ok(InscriptionOp::Burn) => {
                                    let mut tick0 = "".to_string();
                                    let mut tick1: String = "".to_string();
                                    let mut amount: u128 = 0;

                                    if let Some(tick) = object.get("tick0") {
                                        let tick = tick.to_string();
                                        tick0 = tick.trim_matches('"').to_string();
                                    }
                                    if let Some(tick) = object.get("tick1") {
                                        let tick = tick.to_string();
                                        tick1 = tick.trim_matches('"').to_string();
                                    }
                                    if let Some(amt) = object.get("amt0") {
                                        let amt_str = amt.to_string();
                                        let amt_str = amt_str.trim_matches('"');
                                        amount = amt_str.parse().unwrap();
                                    }
                                    let addr = Self::drc_burn_parse(&tx);
                                    if let Some(id) = index.get_dex_id(&tick0, &tick1) {
                                        if let Some(drc) = index.get_dex_name(&tick0, &tick1) {
                                            let liquidity =
                                                index.get_drc_amt(&drc.0, &addr).unwrap().unwrap();
                                            if let Some(mut state) =
                                                Index::id_to_dex(index, id).unwrap()
                                            {
                                                if amount < liquidity {
                                                    let (amt0, amt1) = Self::remove_liquidity(
                                                        liquidity,
                                                        &mut state.2,
                                                        &mut state.3,
                                                    );
                                                    self.id_to_dex.insert(id, state)?;
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(_) => {}
                            }
                        }
                    }
                }
            }
        };

        let is_coinbase = tx
            .input
            .first()
            .map(|tx_in| tx_in.previous_output.is_null())
            .unwrap_or_default();

        if is_coinbase {
            inscriptions.append(&mut self.flotsam);
        }

        inscriptions.sort_by_key(|flotsam| flotsam.offset);
        let mut inscriptions = inscriptions.into_iter().peekable();

        let mut output_value = 0;
        for (vout, tx_out) in tx.output.iter().enumerate() {
            let end = output_value + tx_out.value;

            while let Some(flotsam) = inscriptions.peek() {
                if flotsam.offset >= end {
                    break;
                }

                let new_satpoint = SatPoint {
                    outpoint: OutPoint {
                        txid,
                        vout: vout.try_into().unwrap(),
                    },
                    offset: flotsam.offset - output_value,
                };

                self.update_inscription_location(
                    input_sat_ranges,
                    inscriptions.next().unwrap(),
                    new_satpoint,
                )?;
            }

            output_value = end;

            self.value_cache.insert(
                OutPoint {
                    vout: vout.try_into().unwrap(),
                    txid,
                },
                tx_out.value,
            );

            let addr: [u8; 25] = Self::script_to_address(&tx_out.script_pubkey);

            self.addr_cache.insert(
                OutPoint {
                    vout: vout.try_into().unwrap(),
                    txid,
                },
                UTXO { address: (addr) },
            );
        }

        if is_coinbase {
            for flotsam in inscriptions {
                let new_satpoint = SatPoint {
                    outpoint: OutPoint::null(),
                    offset: self.lost_sats + flotsam.offset - output_value,
                };
                self.update_inscription_location(input_sat_ranges, flotsam, new_satpoint)?;
            }

            Ok(self.reward - output_value)
        } else {
            self.flotsam.extend(inscriptions.map(|flotsam| Flotsam {
                offset: self.reward + flotsam.offset,
                ..flotsam
            }));
            self.reward += input_value - output_value;
            Ok(0)
        }
    }

    fn update_inscription_location(
        &mut self,
        input_sat_ranges: Option<&VecDeque<(u128, u128)>>,
        flotsam: Flotsam,
        new_satpoint: SatPoint,
    ) -> Result {
        let inscription_id = flotsam.inscription_id.store();

        match flotsam.origin {
            Origin::Old(old_satpoint) => {
                self.satpoint_to_id.remove(&old_satpoint.store())?;
            }
            Origin::New(fee) => {
                self.number_to_id
                    .insert(&self.next_number, &inscription_id)?;

                let mut sat = None;
                if let Some(input_sat_ranges) = input_sat_ranges {
                    let mut offset = 0;
                    for (start, end) in input_sat_ranges {
                        let size = end - start;
                        if offset + size > flotsam.offset as u128 {
                            let n = start + flotsam.offset as u128 - offset;
                            self.sat_to_inscription_id.insert(&n, &inscription_id)?;
                            sat = Some(Sat(n));
                            break;
                        }
                        offset += size;
                    }
                }

                self.id_to_entry.insert(
                    &inscription_id,
                    &InscriptionEntry {
                        fee,
                        height: self.height,
                        number: self.next_number,
                        sat,
                        timestamp: self.timestamp,
                    }
                    .store(),
                )?;

                self.next_number += 1;
            }
        }

        let new_satpoint = new_satpoint.store();

        self.satpoint_to_id.insert(&new_satpoint, &inscription_id)?;
        self.id_to_satpoint.insert(&inscription_id, &new_satpoint)?;

        Ok(())
    }

    fn drc_transfer_parse(
        tx: &Transaction,
        index: &Index,
    ) -> Result<Option<(String, Vec<String>)>> {
        let input_num = tx.input.len();
        let output_num = tx.output.len();

        let mut dsts = Vec::new();
        if input_num > 2 || input_num == 0 {
            // not transfer format
            return Ok(None);
        }
        if input_num == 2 && output_num == 5 {
            let src = match Inscription::addr_from_pkscript(&tx.output[1].script_pubkey).unwrap() {
                Some(src) => src,
                None => return Ok(None),
            };

            if let Some(dst) = Inscription::addr_from_pkscript(&tx.output[0].script_pubkey).unwrap()
            {
                dsts.push(dst.clone());
            }
            if let Some(mdr) = Inscription::addr_from_pkscript(&tx.output[0].script_pubkey).unwrap()
            {
                if mdr != "D87Nj1x9H4oczq5Kmb1jxhxpcqx2vELkqh".to_string() {
                    return Ok(None);
                }
            }
            // not transfer format
            return Ok(Some((src, dsts)));
        }
        if input_num == 1 {
            // normal transfer standard
            let src = match index
                .get_transaction(tx.input[0].previous_output.txid)?
                .and_then(|tx| {
                    Inscription::addr_from_sigscript(&tx.input[0].script_sig.clone()).unwrap()
                }) {
                Some(addr) => addr,
                None => return Ok(None),
            };
            for tx_out in &tx.output {
                if tx_out.value == 100000 {
                    if let Some(dst) =
                        Inscription::addr_from_pkscript(&tx_out.script_pubkey).unwrap()
                    {
                        dsts.push(dst);
                    }
                }
            }
            return Ok(Some((src, dsts)));
        }

        Ok(None)
    }
    fn drc_add_parse(tx: &Transaction) -> String {
        //todo:
        "".to_string()
    }
    fn drc_burn_parse(tx: &Transaction) -> String {
        //todo:
        "".to_string()
    }

    fn script_to_address(script: &Script) -> [u8; 25] {
        let mut address = [0u8; 25];
        let script_vec = script.as_bytes();
        if script.is_p2pkh() {
            address[0] = 0x1e;
            address[1..21].copy_from_slice(&script_vec[3..23]);
            let sum = &Sha256::digest(&Sha256::digest(&address[0..21])).to_vec()[0..4];
            address[21..25].copy_from_slice(sum);
            address
        } else if script.is_p2pk() {
            let pub_str = match script_vec[0] {
                33 => script_vec[1..34].to_hex(),
                65 => script_vec[1..66].to_hex(),
                _ => "".to_string(),
            };
            if pub_str.is_empty() {
                address
            } else {
                let pubkey = PublicKey::from_str(&pub_str).unwrap();
                address[0] = 0x1e;
                address[1..21].copy_from_slice(&pubkey.pubkey_hash().to_vec());
                let sum = &Sha256::digest(&Sha256::digest(&address[0..21])).to_vec()[0..4];
                address[21..25].copy_from_slice(sum);
                address
            }
        } else if script.is_p2sh() {
            address[0] = 0x16;
            address[1..21].copy_from_slice(&script_vec[2..22]);
            let sum = &Sha256::digest(&Sha256::digest(&address[0..21])).to_vec()[0..4];

            address[21..25].copy_from_slice(sum);

            address
        } else {
            address
        }
    }

    fn add_liquidity(amt0: u128, amt1: u128, reserve0: &mut u128, reserve1: &mut u128) -> u128 {
        const MINI_LIQUIDITY: u128 = 100;
        let liqudity = amt0 * *reserve1 + amt1 * *reserve0 + amt0 * amt1;

        *reserve0 += amt0;
        *reserve1 += amt1;
        if liqudity < MINI_LIQUIDITY {
            0
        } else {
            liqudity
        }
    }

    fn remove_liquidity(liquidity: u128, reserve0: &mut u128, reserve1: &mut u128) -> (u128, u128) {
        const MINI_RESERVE: u128 = 10;
        if *reserve0 * *reserve1 <= liquidity {
            *reserve0 = MINI_RESERVE;
            *reserve1 = MINI_RESERVE;
            return (0, 0);
        } else {
            let amt0 = ((liquidity * *reserve0) / *reserve1).sqrt();
            let amt1 = ((liquidity * *reserve1) / *reserve0).sqrt();

            if *reserve0 > amt0 + MINI_RESERVE {
                *reserve0 -= amt0;
            } else {
                *reserve0 = MINI_RESERVE;
            }

            if *reserve1 > amt1 + MINI_RESERVE {
                *reserve1 -= amt1;
            } else {
                *reserve1 = MINI_RESERVE;
            }

            return (amt0, amt1);
        }
    }

    fn swap(amtIn: u128, reserve0: &mut u128, reserve1: &mut u128) -> u128 {
        let amtOut;
        amtOut = (amtIn * *reserve1) / (*reserve0 + amtIn);

        return amtOut;
    }
}
