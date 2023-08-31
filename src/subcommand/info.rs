use super::*;

#[derive(Debug, Parser)]
pub(crate) struct Info {
    #[clap(long)]
    transactions: bool,
}

#[derive(Serialize, Deserialize)]
pub struct TransactionsOutput {
    pub start: u64,
    pub end: u64,
    pub count: u64,
    pub elapsed: f64,
}

impl Info {
    pub(crate) fn run(self, options: Options) -> Result {
        let index = Index::open(&options)?;
        index.update()?;
        let info = index.info()?;

        if self.transactions {
            let mut output = Vec::new();
            for window in info.transactions.windows(2) {
                let start = &window[0];
                let end = &window[1];
                output.push(TransactionsOutput {
                    start: start.starting_block_count,
                    end: end.starting_block_count,
                    count: end.starting_block_count - start.starting_block_count,
                    elapsed: (end.starting_timestamp - start.starting_timestamp) as f64
                        / 1000.0
                        / 60.0,
                });
            }
            print_json(output)?;
        } else {
            print_json(info)?;
        }

        Ok(())
    }
}
