use super::*;

pub mod epochs;
pub mod find;
mod index;
pub mod info;
pub mod list;
pub mod subsidy;
pub mod traits;

fn print_json(output: impl Serialize) -> Result {
    serde_json::to_writer_pretty(io::stdout(), &output)?;
    println!();
    Ok(())
}

#[derive(Debug, Parser)]
pub(crate) enum Subcommand {
    #[clap(about = "List the first satoshis of each reward epoch")]
    Epochs,
    #[clap(about = "Update the index")]
    Index,
}

impl Subcommand {
    pub(crate) fn run(self, options: Options) -> Result {
        match self {
            Self::Epochs => epochs::run(),
            Self::Index => index::run(options),
        }
    }
}
