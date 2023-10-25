use std::io::Write;

use super::*;
use crate::{
    index::entry::{DexEntry, DexInscription},
    inscription::ParsedInscription,
};
use num_integer::Roots;
use serde_json::Value;
use std::error::Error;
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
    Remove,
    Swap,
}

impl FromStr for InscriptionOp {
    type Err = ();

    fn from_str(input: &str) -> Result<InscriptionOp, Self::Err> {
        match input {
            "\"create\"" => Ok(InscriptionOp::Create),
            "\"mint\"" => Ok(InscriptionOp::Mint),
            "\"transfer\"" => Ok(InscriptionOp::Transfer),
            "\"add\"" => Ok(InscriptionOp::Add),
            "\"remove\"" => Ok(InscriptionOp::Remove),
            "\"swap\"" => Ok(InscriptionOp::Swap),
            _ => Err(()),
        }
    }
}

fn drc_mint_parse(tx: &Transaction, amount: u128) -> Result<(String, u128), Box<dyn Error>> {
    let input_num = tx.input.len();
    let output_num = tx.output.len();
    if input_num != 1 || output_num != 2 {
        return Err("only support one input and two output type")?;
    }
    let mint_addr = match Inscription::addr_from_pkscript(&tx.output[0].script_pubkey).unwrap() {
        Some(addr) => addr,
        None => return Err("failed to convert mint addr from pk_script")?,
    };
    let mint_times = tx.output[0].value / 100000;
    if mint_times > 30 {
        return Err("mint times need less than 30")?;
    }

    let mint_amt = amount * mint_times as u128;

    let manager_addr = match Inscription::addr_from_pkscript(&tx.output[1].script_pubkey).unwrap() {
        Some(addr) => addr,
        None => return Err("failed to convert manager addr from pk_script")?,
    };
    if manager_addr != "D92uJjQ9eHUcv2GjJUgp6m58V8wYvGV2g9".to_string() {
        return Err("manager address is wrong")?;
    }

    if tx.output[1].value < 50000000 * mint_times {
        return Err("total fee is wrong")?;
    }

    Ok((mint_addr, mint_amt))
}

fn drc_transfer_parse(
    tx: &Transaction,
    index: &Index,
) -> Result<(String, Vec<String>), Box<dyn Error>> {
    let input_num = tx.input.len();
    let output_num = tx.output.len();

    let mut dsts = Vec::new();

    if input_num == 2 && output_num == 5 {
        let src = match Inscription::addr_from_pkscript(&tx.output[1].script_pubkey).unwrap() {
            Some(src) => src,
            None => return Err("failed to convert addr from pk_script")?,
        };

        if let Some(dst) = Inscription::addr_from_pkscript(&tx.output[0].script_pubkey).unwrap() {
            dsts.push(dst.clone());
        }
        if let Some(mdr) = Inscription::addr_from_pkscript(&tx.output[3].script_pubkey).unwrap() {
            if mdr != "D87Nj1x9H4oczq5Kmb1jxhxpcqx2vELkqh".to_string() {
                return Err("manager addr is wrong")?;
            }
        }
        // not transfer format
        return Ok((src, dsts));
    }
    if input_num == 1 {
        // normal transfer standard
        let src = match index
            .get_transaction(tx.input[0].previous_output.txid)?
            .and_then(|tx| {
                Inscription::addr_from_sigscript(&tx.input[0].script_sig.clone()).unwrap()
            }) {
            Some(addr) => addr,
            None => return Err("failed to convert addr from pk_script")?,
        };
        for tx_out in &tx.output {
            if tx_out.value == 100000 {
                if let Some(dst) = Inscription::addr_from_pkscript(&tx_out.script_pubkey).unwrap() {
                    dsts.push(dst);
                }
            }
        }
        return Ok((src, dsts));
    }

    return Err("format of tx doesn't support")?;
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
    drc_state: &'a mut HashMap<String, HashMap<String, Vec<ADDRSTATE>>>,
    dex_state: &'a mut HashMap<u64, Vec<DEXSTATE>>,
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
        drc_state: &'a mut HashMap<String, HashMap<String, Vec<ADDRSTATE>>>,
        dex_state: &'a mut HashMap<u64, Vec<DEXSTATE>>,
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
            drc_state,
            dex_state,
        })
    }

    pub(super) fn index_transaction_inscriptions(
        &mut self,
        index: &Index,
        tx: &Transaction,
        txid: Txid,
        input_sat_ranges: Option<&VecDeque<(u128, u128)>>,
        f: &mut File,
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

        match Inscription::from_transactions(&tx) {
            ParsedInscription::None => {
                // todo: clean up db
            }

            ParsedInscription::Complete(inscription) => {
                let mut b_record = false;
                let content_str = inscription.content_type().unwrap().to_string();
                if content_str == "text/plain;charset=utf-8" {
                    let body = String::from_utf8(inscription.body().unwrap().to_vec()).unwrap();
                    let body_str = &body;

                    let object: Value = serde_json::from_str(body_str).unwrap();

                    if let Some(op) = object.get("op") {
                        match InscriptionOp::from_str(&op.to_string().to_owned()) {
                            Ok(InscriptionOp::Mint) => {
                                match Self::drc_mint_op(&object, tx, self.drc_state) {
                                    Err(_e) => (),
                                    Ok((drc, addr, amt)) => {
                                        /*println!(
                                            "insert database {} for {} at {}",
                                            amt,
                                            addr,
                                            tx.txid().to_string()
                                        );
                                        let buf = format!(
                                            "{} for {} at {}\n",
                                            amt,
                                            addr,
                                            tx.txid().to_string()
                                        );
                                        f.write_all(buf.as_bytes()).unwrap();*/
                                        let drc_value =
                                            self.drc_state.get_mut(drc.as_str()).unwrap();
                                        match drc_value.get_mut(&addr) {
                                            Some(addr_state) => {
                                                let amt_pre = addr_state.last().unwrap().amt;
                                                addr_state.push(ADDRSTATE {
                                                    amt: amt + amt_pre,
                                                    height: (ACTBLK::New(self.height)),
                                                });
                                            }
                                            None => {
                                                drc_value.insert(
                                                    addr,
                                                    vec![ADDRSTATE {
                                                        amt,
                                                        height: (ACTBLK::New(self.height)),
                                                    }],
                                                );
                                            }
                                        }
                                        b_record = true;
                                    }
                                }
                            }
                            Ok(InscriptionOp::Transfer) => {
                                match Self::drc_transfer_op(index, &object, tx, &self.drc_state) {
                                    Ok((drc, src_addr, dsts_addr, amount)) => {
                                        // println!(
                                        //     "transfer {} of {} from {} to {:?} ",
                                        //     amount, drc, src_addr, dsts_addr,
                                        // );
                                        // let buf = format!("{}\n", tx.txid().to_string());
                                        // f.write_all(buf.as_bytes()).unwrap();

                                        let drc_value =
                                            self.drc_state.get_mut(drc.as_str()).unwrap();
                                        let mut total_amt = 0;
                                        for dst_addr in dsts_addr {
                                            match drc_value.get_mut(&dst_addr) {
                                                Some(addr_state) => {
                                                    let amt_pre = addr_state.last().unwrap().amt;
                                                    addr_state.push(ADDRSTATE {
                                                        amt: amt_pre + amount,
                                                        height: (ACTBLK::New(self.height)),
                                                    })
                                                }
                                                None => {
                                                    drc_value.insert(
                                                        dst_addr,
                                                        vec![ADDRSTATE {
                                                            amt: amount,
                                                            height: ACTBLK::New(self.height),
                                                        }],
                                                    );
                                                }
                                            }
                                            total_amt += amount;
                                        }
                                        let src_value = drc_value.get_mut(&src_addr).unwrap();
                                        src_value.push(ADDRSTATE {
                                            amt: src_value.last().unwrap().amt - total_amt,
                                            height: ACTBLK::New(self.height),
                                        });
                                        b_record = true;
                                    }
                                    Err(_e) => (),
                                }
                            }
                            Ok(InscriptionOp::Create) => match Self::dex_create_op(&object) {
                                Ok(id) => {
                                    if self.dex_state.get(&id).is_none() {
                                        self.dex_state.insert(
                                            id,
                                            vec![DEXSTATE {
                                                height: ACTBLK::New(self.height),
                                                number: 0,
                                                reserve0: 0,
                                                reserve1: 0,
                                            }],
                                        );
                                        let drc_id = DEXSTATE::to_hex(id);
                                        let account_null = DexInscription::load([0; 41]);
                                        let mut account_map = HashMap::new();
                                        account_map.insert(
                                            account_null.addr,
                                            vec![ADDRSTATE {
                                                amt: account_null.amt,
                                                height: ACTBLK::Old,
                                            }],
                                        );
                                        self.drc_state.insert(drc_id, account_map);
                                        b_record = true;
                                    } else {
                                        println!("the pairs does exist");
                                    }
                                }
                                Err(_e) => (),
                            },
                            Ok(InscriptionOp::Add) => match Self::dex_add_op(
                                &object,
                                tx,
                                &self.dex_state,
                                &self.drc_state,
                            ) {
                                Err(_e) => (),
                                Ok((id, tick0, tick1, amt0, amt1, addr)) => {
                                    let dex_state_vec = self
                                        .dex_state
                                        .get_mut(&id)
                                        .expect("failed to get dex state");
                                    let dex_state_last = dex_state_vec
                                        .last()
                                        .expect("failed to get current dex state");
                                    match Self::add_liquidity(
                                        amt0,
                                        amt1,
                                        dex_state_last.reserve0,
                                        dex_state_last.reserve1,
                                    ) {
                                        None => (),
                                        Some((res0, res1, liq)) => {
                                            dex_state_vec.push(DEXSTATE {
                                                height: ACTBLK::New(self.height),
                                                number: (dex_state_last.number + 1),
                                                reserve0: (res0),
                                                reserve1: (res1),
                                            });
                                            let drc_state = self
                                                .drc_state
                                                .get_mut(&DEXSTATE::to_hex(id))
                                                .unwrap();
                                            match drc_state.get_mut(&addr) {
                                                None => {
                                                    drc_state.insert(
                                                        addr,
                                                        vec![ADDRSTATE {
                                                            amt: liq,
                                                            height: ACTBLK::Old,
                                                        }],
                                                    );
                                                }
                                                Some(addr_vec) => {
                                                    addr_vec.push(ADDRSTATE {
                                                        amt: liq,
                                                        height: ACTBLK::New(self.height),
                                                    });
                                                }
                                            }
                                            b_record = true;
                                        }
                                    }
                                } //
                            },
                            Ok(InscriptionOp::Swap) => match Self::dex_swap_op(
                                &object,
                                tx,
                                &self.dex_state,
                                &self.drc_state,
                            ) {
                                Err(_e) => (),
                                Ok((id, tick0, tick1, amt0, amt1, addr, b_rev)) => {
                                    let dex_state_vec: &mut Vec<DEXSTATE> = self
                                        .dex_state
                                        .get_mut(&id)
                                        .expect("failed to get dex state");
                                    let dex_state_last = dex_state_vec
                                        .last()
                                        .expect("failed to get current dex state");
                                    let amt_in;
                                    let res0;
                                    let res1;
                                    if b_rev == true {
                                        amt_in = amt1;
                                        res0 = dex_state_last.reserve1;
                                        res1 = dex_state_last.reserve0;
                                    } else {
                                        amt_in = amt0;
                                        res0 = dex_state_last.reserve0;
                                        res1 = dex_state_last.reserve1;
                                    }
                                    let [tick0_state, tick1_state] =
                                        self.drc_state.get_many_mut([&tick0, &tick1]).unwrap();
                                    let (tick0_vec, tick1_vec) = (
                                        tick0_state.get_mut(&addr).unwrap(),
                                        tick1_state.get_mut(&addr).unwrap(),
                                    );

                                    let tick0_last = tick0_vec.last().unwrap();
                                    let tick1_last = tick1_vec.last().unwrap();
                                    match Self::swap(amt_in, res0, res1) {
                                        None => (),
                                        Some((amt_out, res0, res1)) => {
                                            if b_rev == true {
                                                tick0_vec.push(ADDRSTATE {
                                                    amt: (tick0_last.amt + amt_out),
                                                    height: ACTBLK::New(self.height),
                                                });
                                                tick0_vec.push(ADDRSTATE {
                                                    amt: (tick1_last.amt - amt_in),
                                                    height: ACTBLK::New(self.height),
                                                });
                                                dex_state_vec.push(DEXSTATE {
                                                    height: ACTBLK::New(self.height),
                                                    number: dex_state_last.number + 1,
                                                    reserve0: res1,
                                                    reserve1: res0,
                                                });
                                            } else {
                                                tick0_vec.push(ADDRSTATE {
                                                    amt: (tick0_last.amt - amt_in),
                                                    height: ACTBLK::New(self.height),
                                                });
                                                tick0_vec.push(ADDRSTATE {
                                                    amt: (tick1_last.amt + amt_out),
                                                    height: ACTBLK::New(self.height),
                                                });
                                                dex_state_vec.push(DEXSTATE {
                                                    height: ACTBLK::New(self.height),
                                                    number: dex_state_last.number + 1,
                                                    reserve0: res0,
                                                    reserve1: res1,
                                                });
                                            }
                                            b_record = true;
                                        }
                                    }
                                }
                            },
                            Ok(InscriptionOp::Remove) => {
                                match Self::dex_remove_op(
                                    &object,
                                    tx,
                                    self.dex_state,
                                    self.drc_state,
                                ) {
                                    Err(_e) => (),
                                    Ok((id, tick0, tick1, addr, liq)) => {
                                        let dex_state_vec: &mut Vec<DEXSTATE> = self
                                            .dex_state
                                            .get_mut(&id)
                                            .expect("failed to get dex state");
                                        let dex_state_last = dex_state_vec
                                            .last()
                                            .expect("failed to get current dex state");
                                        let mut res0 = dex_state_last.reserve0;
                                        let mut res1 = dex_state_last.reserve1;
                                        let [tick0_state, tick1_state] =
                                            self.drc_state.get_many_mut([&tick0, &tick1]).unwrap();
                                        let (tick0_vec, tick1_vec) = (
                                            tick0_state.get_mut(&addr).unwrap(),
                                            tick1_state.get_mut(&addr).unwrap(),
                                        );
                                        let tick0_last = tick0_vec.last().unwrap();
                                        let tick1_last = tick1_vec.last().unwrap();
                                        match Self::remove_liquidity(liq, &mut res0, &mut res1) {
                                            None => (),
                                            Some((amt0, amt1)) => {
                                                tick0_vec.push(ADDRSTATE {
                                                    amt: (tick0_last.amt + amt0),
                                                    height: ACTBLK::New(self.height),
                                                });
                                                tick1_vec.push(ADDRSTATE {
                                                    amt: (tick1_last.amt + amt1),
                                                    height: ACTBLK::New(self.height),
                                                });
                                                dex_state_vec.push(DEXSTATE {
                                                    height: ACTBLK::New(self.height),
                                                    number: (dex_state_last.number + 1),
                                                    reserve0: res0,
                                                    reserve1: res1,
                                                });
                                                b_record = true;
                                            }
                                        }
                                    }
                                }
                            }
                            Err(_) => {}
                        }
                    }
                }
                if b_record {
                    let mut txids_vec = vec![];
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
                }
            }
        }

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

    fn drc_mint_op(
        object: &Value,
        tx: &Transaction,
        drc_state: &HashMap<String, HashMap<String, Vec<ADDRSTATE>>>,
    ) -> Result<(String, String, u128), Box<dyn Error>> {
        let mut drc = "".to_string();
        if let Some(tick) = object.get("tick") {
            let tick = tick.to_string();
            drc = tick.trim_matches('"').to_string();
        } else {
            return Err("failed to get tick")?;
        }

        let mut amount: u128 = 0;
        if let Some(amt) = object.get("amt") {
            let amt_str = amt.to_string();
            let amt_str = amt_str.trim_matches('"');
            amount = amt_str.parse().unwrap();
        } else {
            println!("amount error");
            return Err("failed to get amount")?;
        }

        if !drc_state.get(drc.as_str()).is_none() && amount != 0 {
            match drc_mint_parse(tx, amount) {
                Ok((addr, amt)) => return Ok((drc, addr, amt)),
                Err(e) => return Err(e),
            }
        } else {
            return Err("drc20 doesn't exits")?;
        }
    }

    fn drc_transfer_op(
        index: &Index,
        object: &Value,
        tx: &Transaction,
        drc_state: &HashMap<String, HashMap<String, Vec<ADDRSTATE>>>,
    ) -> Result<(String, String, Vec<String>, u128), Box<dyn Error>> {
        let mut drc = "".to_string();
        if let Some(tick) = object.get("tick") {
            let tick = tick.to_string();
            drc = tick.trim_matches('"').to_string();
        } else {
            return Err("failed to get tick")?;
        }

        let amount: u128;
        if let Some(amt) = object.get("amt") {
            let amt_str = amt.to_string();
            let amt_str = amt_str.trim_matches('"');
            amount = amt_str.parse().unwrap();
        } else {
            return Err("failed to get amount")?;
        }
        let addr_state = drc_state.get(drc.as_str());
        if !addr_state.is_none() && amount != 0 {
            match drc_transfer_parse(tx, index) {
                Ok((src, dsts)) => {
                    let total_amt = dsts.len() as u128 * amount;

                    match addr_state.unwrap().get(&src) {
                        None => Err("src doesn't exits")?,
                        Some(addr_vec) => {
                            if addr_vec.last().unwrap().amt >= total_amt {
                                return Ok((drc, src, dsts, amount));
                            } else {
                                return Err("balance of src is not enough")?;
                            }
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        } else {
            return Err("drc20 doesn't exits")?;
        }
    }

    fn dex_create_op(object: &Value) -> Result<u64, Box<dyn Error>> {
        let mut tick0 = "".to_string();
        let mut tick1: String = "".to_string();

        if let Some(tick) = object.get("tick0") {
            let tick = tick.to_string();
            tick0 = tick.trim_matches('"').to_string();
        } else {
            return Err("failed to parse tick0")?;
        }
        if let Some(tick) = object.get("tick1") {
            let tick = tick.to_string();
            tick1 = tick.trim_matches('"').to_string();
        } else {
            return Err("failed to parse tick1")?;
        }
        match DEXSTATE::get_id(&tick0, &tick1) {
            None => {
                return Err("failed to generate dex id")?;
            }
            Some(id) => {
                return Ok(id);
            }
        }
    }

    fn dex_add_op(
        object: &Value,
        tx: &Transaction,
        dex_state: &HashMap<u64, Vec<DEXSTATE>>,
        drc_state: &HashMap<String, HashMap<String, Vec<ADDRSTATE>>>,
    ) -> Result<(u64, String, String, u128, u128, String), Box<dyn Error>> {
        let mut tick0;
        let mut tick1: String;
        let mut amt0: u128;
        let mut amt1: u128;
        let id: u64;
        let mut addr: String = "".to_string();
        if let Some(tick) = object.get("tick0") {
            let tick = tick.to_string();
            tick0 = tick.trim_matches('"').to_string();
        } else {
            return Err("failed to parse tick0")?;
        }
        if let Some(tick) = object.get("tick1") {
            let tick = tick.to_string();
            tick1 = tick.trim_matches('"').to_string();
        } else {
            return Err("failed to parse tick1")?;
        }
        if let Some(amt) = object.get("amt0") {
            let amt_str = amt.to_string();
            let amt_str = amt_str.trim_matches('"');
            amt0 = amt_str.parse().unwrap();
        } else {
            return Err("failed to parse amt0")?;
        }
        if let Some(amt) = object.get("amt1") {
            let amt_str = amt.to_string();
            let amt_str = amt_str.trim_matches('"');
            amt1 = amt_str.parse().unwrap();
        } else {
            return Err("failed to parse amt1")?;
        }
        if let Some(tid) = DEXSTATE::get_id(&tick0, &tick1) {
            if dex_state.get(&tid).is_none() {
                if let Some(tid) = DEXSTATE::get_id(&tick1, &tick0) {
                    std::mem::swap(&mut tick0, &mut tick1);
                    let amt = amt0;
                    amt0 = amt1;
                    amt1 = amt;
                    if drc_state.get(&DEXSTATE::to_hex(tid)).is_none() {
                        return Err("liquidity of dex doesn't exist")?;
                    }
                    id = tid;
                } else {
                    return Err("dex doesn't exist")?;
                }
            } else {
                if drc_state.get(&DEXSTATE::to_hex(tid)).is_none() {
                    return Err("liquidity of dex doesn't exist")?;
                }
                id = tid;
            }
        } else {
            return Err("failed to get dex id")?;
        }
        if let Ok(data) = Inscription::addr_from_sigscript(&tx.input[0].script_sig.clone()) {
            match data {
                Some(data) => {
                    addr = data;
                }
                None => {
                    return Err("addr is none")?;
                }
            }
        } else {
            Err("failed to parse address from transaction")?;
        }
        let tick0_state = drc_state.get(tick0.as_str());
        if !tick0_state.is_none() && amt0 != 0 {
            match tick0_state.unwrap().get(&addr) {
                None => Err("src doesn't exits")?,
                Some(addr_vec) => {
                    if addr_vec.last().unwrap().amt < amt0 {
                        return Err("balance of src is not enough")?;
                    }
                }
            }
        } else {
            return Err("something wrong in tick0")?;
        }
        let tick1_state = drc_state.get(tick1.as_str());
        if !tick1_state.is_none() && amt1 != 0 {
            match tick1_state.unwrap().get(&addr) {
                None => Err("src doesn't exits")?,
                Some(addr_vec) => {
                    if addr_vec.last().unwrap().amt < amt1 {
                        return Err("balance of src is not enough")?;
                    }
                }
            }
        } else {
            return Err("something wrong in tick1")?;
        }

        return Ok((id, tick0, tick1, amt0, amt1, addr));
    }

    fn dex_swap_op(
        object: &Value,
        tx: &Transaction,
        dex_state: &HashMap<u64, Vec<DEXSTATE>>,
        drc_state: &HashMap<String, HashMap<String, Vec<ADDRSTATE>>>,
    ) -> Result<(u64, String, String, u128, u128, String, bool), Box<dyn Error>> {
        let mut tick0;
        let mut tick1: String;
        let mut amt0: u128;
        let mut amt1: u128;
        let id: u64;
        let b_rev: bool;
        let mut addr: String = "".to_string();
        if let Some(tick) = object.get("tick0") {
            let tick = tick.to_string();
            tick0 = tick.trim_matches('"').to_string();
        } else {
            return Err("failed to parse tick0")?;
        }
        if let Some(tick) = object.get("tick1") {
            let tick = tick.to_string();
            tick1 = tick.trim_matches('"').to_string();
        } else {
            return Err("failed to parse tick1")?;
        }
        if let Some(amt) = object.get("amt0") {
            let amt_str = amt.to_string();
            let amt_str = amt_str.trim_matches('"');
            amt0 = amt_str.parse().unwrap();
        } else {
            return Err("failed to parse amt0")?;
        }
        if let Some(amt) = object.get("amt1") {
            let amt_str = amt.to_string();
            let amt_str = amt_str.trim_matches('"');
            amt1 = amt_str.parse().unwrap();
        } else {
            return Err("failed to parse amt1")?;
        }
        if let Some(tid) = DEXSTATE::get_id(&tick0, &tick1) {
            if dex_state.get(&tid).is_none() {
                if let Some(tid) = DEXSTATE::get_id(&tick1, &tick0) {
                    let mut tick = tick0;
                    tick0 = tick1;
                    tick1 = tick;
                    let mut amt = amt0;
                    amt0 = amt1;
                    amt1 = amt;
                    if drc_state.get(&DEXSTATE::to_hex(tid)).is_none() {
                        return Err("liquidity of dex doesn't exist")?;
                    }
                    id = tid;
                    b_rev = true;
                } else {
                    return Err("dex doesn't exist")?;
                }
            } else {
                if drc_state.get(&DEXSTATE::to_hex(tid)).is_none() {
                    return Err("liquidity of dex doesn't exist")?;
                }
                id = tid;
                b_rev = false;
            }
        } else {
            return Err("failed to get dex id")?;
        }
        if let Ok(data) = Inscription::addr_from_sigscript(&tx.input[0].script_sig.clone()) {
            match data {
                Some(data) => {
                    addr = data;
                }
                None => {
                    return Err("addr is none")?;
                }
            }
        } else {
            Err("failed to parse address from transaction")?;
        }
        let tick0_state = drc_state.get(tick0.as_str());
        if !tick0_state.is_none() && amt0 != 0 {
            match tick0_state.unwrap().get(&addr) {
                None => Err("src doesn't exits")?,
                Some(addr_vec) => {
                    if addr_vec.last().unwrap().amt < amt0 {
                        return Err("balance of src is not enough")?;
                    }
                }
            }
        } else {
            return Err("something wrong in tick0")?;
        }
        let tick1_state = drc_state.get(tick1.as_str());
        if tick1_state.is_none() {
            return Err("something wrong in tick1")?;
        }

        return Ok((id, tick0, tick1, amt0, amt1, addr, b_rev));
    }

    fn dex_remove_op(
        object: &Value,
        tx: &Transaction,
        dex_state: &HashMap<u64, Vec<DEXSTATE>>,
        drc_state: &HashMap<String, HashMap<String, Vec<ADDRSTATE>>>,
    ) -> Result<(u64, String, String, String, u128), Box<dyn Error>> {
        let mut tick0;
        let mut tick1: String;
        let mut liq: u128;

        let id: u64;
        let b_rev: bool;
        let mut addr: String = "".to_string();
        if let Some(tick) = object.get("tick0") {
            let tick = tick.to_string();
            tick0 = tick.trim_matches('"').to_string();
        } else {
            return Err("failed to parse tick0")?;
        }
        if let Some(tick) = object.get("tick1") {
            let tick = tick.to_string();
            tick1 = tick.trim_matches('"').to_string();
        } else {
            return Err("failed to parse tick1")?;
        }
        if let Some(amt) = object.get("amt") {
            let amt_str = amt.to_string();
            let amt_str = amt_str.trim_matches('"');
            liq = amt_str.parse().unwrap();
        } else {
            return Err("failed to parse amt")?;
        }

        if let Some(tid) = DEXSTATE::get_id(&tick0, &tick1) {
            if dex_state.get(&tid).is_none() {
                if let Some(tid) = DEXSTATE::get_id(&tick1, &tick0) {
                    let mut tick = tick0;
                    tick0 = tick1;
                    tick1 = tick;

                    id = tid;
                } else {
                    return Err("dex doesn't exist")?;
                }
            } else {
                id = tid;
            }
        } else {
            return Err("failed to get dex id")?;
        }
        if let Ok(data) = Inscription::addr_from_sigscript(&tx.input[0].script_sig.clone()) {
            match data {
                Some(data) => {
                    addr = data;
                }
                None => {
                    return Err("addr is none")?;
                }
            }
        } else {
            Err("failed to parse address from transaction")?;
        }
        let liq_state = drc_state.get(&DEXSTATE::to_hex(id));
        if !liq_state.is_none() && liq != 0 {
            match liq_state.unwrap().get(&addr) {
                None => Err("src doesn't exits")?,
                Some(addr_vec) => {
                    if addr_vec.last().unwrap().amt >= liq {
                        return Err("liquidity of src is not enough")?;
                    }
                }
            }
        } else {
            return Err("something wrong in tick0")?;
        }

        return Ok((id, tick0, tick1, addr, liq));
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

    fn add_liquidity(
        amt0: u128,
        amt1: u128,
        reserve0: u128,
        reserve1: u128,
    ) -> Option<(u128, u128, u128)> {
        const MINI_LIQUIDITY: u128 = 100;
        let liqudity = amt0 * reserve1 + amt1 * reserve0 + amt0 * amt1;

        let res0 = reserve0 + amt0;
        let res1 = reserve1 + amt1;

        if liqudity < MINI_LIQUIDITY {
            None
        } else {
            Some((res0, res1, liqudity))
        }
    }

    fn remove_liquidity(
        liquidity: u128,
        reserve0: &mut u128,
        reserve1: &mut u128,
    ) -> Option<(u128, u128)> {
        const MINI_RESERVE: u128 = 10;
        if *reserve0 * *reserve1 <= liquidity {
            None
        } else {
            let amt0 = ((liquidity * *reserve0) / *reserve1).sqrt();
            let amt1 = ((liquidity * *reserve1) / *reserve0).sqrt();

            if *reserve0 > amt0 + MINI_RESERVE {
                *reserve0 -= amt0;
            } else {
                return None;
            }

            if *reserve1 > amt1 + MINI_RESERVE {
                *reserve1 -= amt1;
            } else {
                return None;
            }

            return Some((amt0, amt1));
        }
    }

    fn swap(amt_in: u128, reserve0: u128, reserve1: u128) -> Option<(u128, u128, u128)> {
        let amt_out = (amt_in * reserve1) / (reserve0 + amt_in);
        let res0 = reserve0 + amt_in;
        if reserve1 > amt_out {
            let res1 = reserve1 - amt_out;
            Some((res0, res1, amt_out))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        self::index::updater::inscription_updater,
        super::*,
        bitcoin::secp256k1::rand::{self, RngCore},
        tempfile::TempDir,
    };

    fn index_build() -> Result<Index> {
        let rpc_url = "39.98.228.222:9141";

        let tempdir = TempDir::new().unwrap();
        let cookie_file = tempdir.path().join("cookie");
        fs::write(&cookie_file, "admin:dogewow2023").unwrap();

        let command: Vec<OsString> = vec![
            "ord".into(),
            "--rpc-url".into(),
            rpc_url.into(),
            "--data-dir".into(),
            tempdir.path().into(),
            "--cookie-file".into(),
            cookie_file.into(),
        ];
        let args: Vec<OsString> = Vec::new();
        let options = Options::try_parse_from(command.into_iter().chain(args)).unwrap();
        let index = Index::open(&options)?;

        Ok(index)
    }

    fn empty_map() -> Option<HashMap<String, HashMap<String, Vec<ADDRSTATE>>>> {
        let mut act_map = HashMap::new();
        let mut addr_map = HashMap::new();
        addr_map.insert(
            "".to_string(),
            vec![ADDRSTATE {
                amt: 0,
                height: ACTBLK::Old,
            }],
        );
        act_map.insert("UNIX".to_string(), addr_map);
        Some(act_map)
    }

    #[test]
    fn test_parse_mint_tx() {
        let index = index_build().expect("failed to generate index");
        let tx_str = "f7501f2d8a73db6da05e1d1d8fbea7d450ab0e87b23791562fb3a92a7229efb3";
        let txid = bitcoin::Txid::from_str(&tx_str).unwrap();
        let tx = index
            .client
            .get_raw_transaction(&txid)
            .expect("failed to get transaction");
        match Inscription::from_transactions(&tx) {
            ParsedInscription::None => println!("failed to get inscription"),
            ParsedInscription::Complete(inscription) => {
                let body = String::from_utf8(inscription.body().unwrap().to_vec()).unwrap();
                let object: Value = serde_json::from_str(&body).unwrap();
                println!("{:?}", object);
                let drc_state = empty_map().unwrap();
                let res = InscriptionUpdater::drc_mint_op(&object, &tx, &drc_state);
                println!("res: {:?}", res);
            }
        }
    }
    #[test]
    fn test_parse_transfer_tx() {
        let index = index_build().expect("failed to generate index");
        let tx_str = "685d3b8da7171e9d641da6765a84afc9e4f03297e19a2131c84bae71581df684";
        let txid = bitcoin::Txid::from_str(&tx_str).unwrap();
        let tx = index
            .client
            .get_raw_transaction(&txid)
            .expect("failed to get transaction");
        match Inscription::from_transactions(&tx) {
            ParsedInscription::None => println!("failed to get inscription"),
            ParsedInscription::Complete(inscription) => {
                let body = String::from_utf8(inscription.body().unwrap().to_vec()).unwrap();
                let object: Value = serde_json::from_str(&body).unwrap();
                println!("{:?}", object);
                let mut drc_state = empty_map().unwrap();
                let mut addr_state = HashMap::new();
                addr_state.insert(
                    "D6MsJWXewKcnVXvdbe8VLJAFH98Bmvk9CP".to_string(),
                    vec![ADDRSTATE {
                        amt: 3600000,
                        height: ACTBLK::Old,
                    }],
                );
                drc_state.insert("UNIX".to_string(), addr_state);
                let (drc, src_addr, dsts_addr, amount) =
                    InscriptionUpdater::drc_transfer_op(&index, &object, &tx, &drc_state).unwrap();
                let height = 100;
                let drc_value = drc_state.get_mut(drc.as_str()).unwrap();
                let mut total_amt = 0;
                for dst_addr in dsts_addr {
                    match drc_value.get_mut(&dst_addr) {
                        Some(addr_state) => {
                            let amt_pre = addr_state.last().unwrap().amt;
                            addr_state.push(ADDRSTATE {
                                amt: amt_pre + amount,
                                height: (ACTBLK::New(height)),
                            })
                        }
                        None => {
                            drc_value.insert(
                                dst_addr,
                                vec![ADDRSTATE {
                                    amt: amount,
                                    height: ACTBLK::New(height),
                                }],
                            );
                        }
                    }
                    total_amt += amount;
                }
                let src_value = drc_value.get_mut(&src_addr).unwrap();
                src_value.push(ADDRSTATE {
                    amt: src_value.last().unwrap().amt - total_amt,
                    height: ACTBLK::New(height),
                });
                println!("addr_state: {:?}", drc_state.get("UNIX").unwrap());
            }
        }
    }
}
