use super::*;

#[derive(PartialEq, Debug)]
pub(crate) struct Decimal {
    height: Height,
    offset: u64,
}

impl From<Sat> for Decimal {
    fn from(sat: Sat) -> Self {
        Self {
            height: sat.height(),
            offset: sat.third(),
        }
    }
}

impl Display for Decimal {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}.{}", self.height, self.offset)
    }
}
