use super::*;

#[derive(Copy, Clone, Debug, Display, FromStr, Ord, Eq, PartialEq, PartialOrd)]
pub(crate) struct Height(pub(crate) u64);

impl Height {
    pub(crate) fn n(self) -> u64 {
        self.0
    }

    pub(crate) fn subsidy(self) -> u64 {
        Epoch::from(self).subsidy()
    }

    pub(crate) fn starting_sat(self) -> Sat {
        let epoch = Epoch::from(self);
        let epoch_starting_sat = epoch.starting_sat();
        let epoch_starting_height = epoch.starting_height();
        epoch_starting_sat
            + ((self - epoch_starting_height.n()).n() as u128) * (epoch.subsidy() as u128)
    }
}

impl Add<u64> for Height {
    type Output = Self;

    fn add(self, other: u64) -> Height {
        Self(self.0 + other)
    }
}

impl Sub<u64> for Height {
    type Output = Self;

    fn sub(self, other: u64) -> Height {
        Self(self.0 - other)
    }
}

impl PartialEq<u64> for Height {
    fn eq(&self, other: &u64) -> bool {
        self.0 == *other
    }
}
