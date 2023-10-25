#![allow(
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::result_large_err
)]
#![deny(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use {
    self::{
        arguments::Arguments, blocktime::Blocktime, config::Config, decimal::Decimal,
        deserialize_from_str::DeserializeFromStr, epoch::Epoch, height::Height, index::Index,
        inscription::Inscription, inscription_id::InscriptionId, options::Options,
        subcommand::Subcommand,
    },
    anyhow::{anyhow, bail, Context, Error},
    bitcoin::{
        blockdata::constants::COIN_VALUE,
        consensus::{self, Decodable, Encodable},
        hash_types::BlockHash,
        hashes::Hash,
        Address, Amount, Block, Network, OutPoint, Script, Sequence, Transaction, TxIn, TxOut,
        Txid,
    },
    bitcoincore_rpc::{Client, RpcApi},
    chain::Chain,
    chrono::{DateTime, TimeZone, Utc},
    clap::{ArgGroup, Parser},
    derive_more::{Display, FromStr},
    serde::{Deserialize, Deserializer, Serialize, Serializer},
    std::{
        cmp,
        collections::{BTreeMap, HashSet, VecDeque},
        env,
        ffi::OsString,
        fmt::{self, Display, Formatter},
        fs::{self, File},
        io,
        net::{TcpListener, ToSocketAddrs},
        ops::{Add, AddAssign, Sub},
        path::{Path, PathBuf},
        process::{self, Command},
        str::FromStr,
        sync::{
            atomic::{self, AtomicU64},
            Arc, Mutex,
        },
        thread,
        time::{Duration, Instant, SystemTime},
    },
    tempfile::TempDir,
    tokio::{runtime::Runtime, task},
};

pub use crate::{fee_rate::FeeRate, rarity::Rarity, sat::Sat, sat_point::SatPoint};

macro_rules! tprintln {
    ($($arg:tt)*) => {

      if cfg!(test) {
        eprint!("==> ");
        eprintln!($($arg)*);
      }
    };
}

mod arguments;
mod blocktime;
mod chain;
mod config;
mod decimal;
mod deserialize_from_str;
mod epoch;
mod fee_rate;
mod height;
mod index;
mod inscription;
mod inscription_id;
mod options;
mod rarity;
mod sat;
mod sat_point;
pub mod subcommand;

type Result<T = (), E = Error> = std::result::Result<T, E>;

static INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static LISTENERS: Mutex<Vec<axum_server::Handle>> = Mutex::new(Vec::new());

fn integration_test() -> bool {
    env::var_os("ORD_INTEGRATION_TEST")
        .map(|value| value.len() > 0)
        .unwrap_or(false)
}

fn timestamp(seconds: u32) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds.into(), 0).unwrap()
}

const INTERRUPT_LIMIT: u64 = 5;

pub fn main() {
    env_logger::init();

    ctrlc::set_handler(move || {
    LISTENERS
      .lock()
      .unwrap()
      .iter()
      .for_each(|handle| handle.graceful_shutdown(Some(Duration::from_millis(100))));

    println!("Detected Ctrl-C, attempting to shut down ord gracefully. Press Ctrl-C {INTERRUPT_LIMIT} times to force shutdown.");

    let interrupts = INTERRUPTS.fetch_add(1, atomic::Ordering::Relaxed);

    if interrupts > INTERRUPT_LIMIT {
      process::exit(1);
    }
  })
  .expect("Error setting ctrl-c handler");

    if let Err(err) = Arguments::parse().run() {
        eprintln!("error: {err}");
        err.chain()
            .skip(1)
            .for_each(|cause| eprintln!("because: {cause}"));
        if env::var_os("RUST_BACKTRACE")
            .map(|val| val == "1")
            .unwrap_or_default()
        {
            eprintln!("{}", err.backtrace());
        }
        process::exit(1);
    }
}
