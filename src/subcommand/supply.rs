use super::*;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Output {
    pub supply: u64,
    pub first: u64,
    pub last: u64,
    pub last_mined_in_block: u64,
}

pub(crate) fn run() -> Result {
    let mut last = 0;

    loop {
        if Height(last + 1).subsidy() == 0 {
            break;
        }
        last += 1;
    }

    print_json(Output {
        supply: Sat::SUPPLY,
        first: 0,
        last: Sat::SUPPLY - 1,
        last_mined_in_block: last,
    })?;

    Ok(())
}
