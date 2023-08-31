use {super::*, bitcoincore_rpc::Auth};

#[derive(Clone, Default, Debug, Parser)]
#[clap(group(
  ArgGroup::new("chains")
    .required(false)
    .args(&["chain-argument", "signet", "regtest", "testnet"]),
))]
pub(crate) struct Options {
    #[clap(long, help = "Load Dogecoin Core data dir from <DOGECOIN_DATA_DIR>.")]
    pub(crate) dogecoin_data_dir: Option<PathBuf>,
    #[clap(
        long = "chain",
        arg_enum,
        default_value = "mainnet",
        help = "Use <CHAIN>."
    )]
    pub(crate) chain_argument: Chain,
    #[clap(long, help = "Load configuration from <CONFIG>.")]
    pub(crate) config: Option<PathBuf>,
    #[clap(long, help = "Load configuration from <CONFIG_DIR>.")]
    pub(crate) config_dir: Option<PathBuf>,
    #[clap(long, help = "Load Dogecoin Core RPC cookie file from <COOKIE_FILE>.")]
    pub(crate) cookie_file: Option<PathBuf>,
    #[clap(long, help = "Authenticate to Dogecoin Core RPC with <RPC_PASS>.")]
    pub(crate) dogecoin_rpc_pass: Option<String>,
    #[clap(long, help = "Authenticate to Dogecoin Core RPC as <RPC_USER>.")]
    pub(crate) dogecoin_rpc_user: Option<String>,
    #[clap(long, help = "Store index in <DATA_DIR>.")]
    pub(crate) data_dir: Option<PathBuf>,
    #[clap(
        long,
        help = "Don't look for inscriptions below <FIRST_INSCRIPTION_HEIGHT>."
    )]
    pub(crate) first_inscription_height: Option<u64>,
    #[clap(long, help = "Limit index to <HEIGHT_LIMIT> blocks.")]
    pub(crate) height_limit: Option<u64>,
    #[clap(long, help = "Use index at <INDEX>.")]
    pub(crate) index: Option<PathBuf>,
    #[clap(long, help = "Track location of all satoshis.")]
    pub(crate) index_sats: bool,
    #[clap(long, short, help = "Use regtest. Equivalent to `--chain regtest`.")]
    pub(crate) regtest: bool,
    #[clap(long, help = "Connect to Dogecoin Core RPC at <RPC_URL>.")]
    pub(crate) rpc_url: Option<String>,
    #[clap(long, short, help = "Use signet. Equivalent to `--chain signet`.")]
    pub(crate) signet: bool,
    #[clap(long, short, help = "Use testnet. Equivalent to `--chain testnet`.")]
    pub(crate) testnet: bool,
    #[clap(long, default_value = "ord", help = "Use wallet named <WALLET>.")]
    pub(crate) wallet: String,
    //#[clap(long, help = "Rollback Database to <HEIGHT>.")]
    //pub(crate) rollback: Option<u64>,
}

impl Options {
    pub(crate) fn chain(&self) -> Chain {
        if self.signet {
            Chain::Signet
        } else if self.regtest {
            Chain::Regtest
        } else if self.testnet {
            Chain::Testnet
        } else {
            self.chain_argument
        }
    }

    pub(crate) fn first_inscription_height(&self) -> u64 {
        if self.chain() == Chain::Regtest {
            self.first_inscription_height.unwrap_or(0)
        } else if integration_test() {
            0
        } else {
            self.first_inscription_height
                .unwrap_or_else(|| self.chain().first_inscription_height())
        }
    }

    pub(crate) fn rpc_url(&self) -> String {
        self.rpc_url
            .clone()
            .unwrap_or_else(|| format!("127.0.0.1:{}", self.chain().default_rpc_port(),))
    }

    pub(crate) fn get_auth(&self) -> bitcoincore_rpc::Auth {
        if let Some(cookie_file) = &self.cookie_file {
            return Auth::CookieFile(cookie_file.clone());
        } else if let Some(user) = &self.dogecoin_rpc_user {
            if let Some(password) = &self.dogecoin_rpc_pass {
                return Auth::UserPass(user.clone(), password.clone());
            } else {
                panic!("BOTH RPC_USER + RPC_PASS must be set.");
            }
        } else {
            panic!("Either RPC_COOKIE or RPC_USER + RPC_PASS must be set.");
        }
    }
    pub(crate) fn data_dir(&self) -> Result<PathBuf> {
        let base = match &self.data_dir {
            Some(base) => base.clone(),
            None => dirs::data_dir()
                .ok_or_else(|| anyhow!("failed to retrieve data dir"))?
                .join("ord"),
        };

        Ok(self.chain().join_with_data_dir(&base))
    }

    pub(crate) fn load_config(&self) -> Result<Config> {
        match &self.config {
            Some(path) => Ok(serde_yaml::from_reader(File::open(path)?)?),
            None => match &self.config_dir {
                Some(dir) if dir.join("ord.yaml").exists() => {
                    Ok(serde_yaml::from_reader(File::open(dir.join("ord.yaml"))?)?)
                }
                Some(_) | None => Ok(Default::default()),
            },
        }
    }

    fn format_dogecoin_core_version(version: usize) -> String {
        format!(
            "{}.{}.{}.{}",
            version / 1000000,
            version % 1000000 / 10000,
            version % 10000 / 100,
            version % 100
        )
    }

    pub(crate) fn dogecoin_rpc_client(&self) -> Result<Client> {
        let rpc_url = self.rpc_url();
        let auth = self.get_auth();

        log::info!("Connecting to Dogecoin Core at {}", self.rpc_url());

        if let Auth::CookieFile(cookie_file) = &auth {
            log::info!(
                "Using credentials from cookie file at `{}`",
                cookie_file.display()
            );
        }

        let client = Client::new(&rpc_url, auth)?;

        let rpc_chain = match client.get_blockchain_info()?.chain.as_str() {
            "main" => Chain::Mainnet,
            "test" => Chain::Testnet,
            "regtest" => Chain::Regtest,
            "signet" => Chain::Signet,
            other => bail!("Dogecoin RPC server on unknown chain: {other}"),
        };

        let ord_chain = self.chain();

        if rpc_chain != ord_chain {
            bail!("Dogecoin RPC server is on {rpc_chain} but ord is on {ord_chain}");
        }

        Ok(client)
    }

    pub(crate) fn dogecoin_rpc_client_for_wallet_command(&self, create: bool) -> Result<Client> {
        let client = self.dogecoin_rpc_client()?;

        const MIN_VERSION: usize = 1140600;

        let dogecoin_version = client.version()?;
        if dogecoin_version < MIN_VERSION {
            bail!(
                "Dogecoin Core {} or newer required, current version is {}",
                Self::format_dogecoin_core_version(MIN_VERSION),
                Self::format_dogecoin_core_version(dogecoin_version),
            );
        }

        if !create {
            if !client.list_wallets()?.contains(&self.wallet) {
                client.load_wallet(&self.wallet)?;
            }

            let descriptors = client.list_descriptors(None)?.descriptors;

            let tr = descriptors
                .iter()
                .filter(|descriptor| descriptor.desc.starts_with("tr("))
                .count();

            let rawtr = descriptors
                .iter()
                .filter(|descriptor| descriptor.desc.starts_with("rawtr("))
                .count();

            if tr != 2 || descriptors.len() != 2 + rawtr {
                bail!("wallet \"{}\" contains unexpected output descriptors, and does not appear to be an `ord` wallet, create a new wallet with `ord wallet create`", self.wallet);
            }
        }

        Ok(client)
    }
}
