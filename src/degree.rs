use super::*;

#[derive(PartialEq, Debug)]
pub(crate) struct Degree {
    pub(crate) hour: u64,
    pub(crate) minute: u64,
    pub(crate) second: u64,
    pub(crate) third: u64,
}

impl Display for Degree {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "{}°{}′{}″{}‴",
            self.hour, self.minute, self.second, self.third
        )
    }
}

impl From<Sat> for Degree {
    fn from(sat: Sat) -> Self {
        let height = sat.height().n();
        Degree {
            hour: height / (CYCLE_EPOCHS * SUBSIDY_HALVING_INTERVAL),
            minute: height % SUBSIDY_HALVING_INTERVAL,
            second: height % DIFFCHANGE_INTERVAL,
            third: sat.third(),
        }
    }
}
