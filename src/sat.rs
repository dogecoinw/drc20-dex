use super::*;

#[derive(Copy, Clone, Eq, PartialEq, Debug, Display, Ord, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Sat(pub u128);

impl Sat {
    pub(crate) fn n(self) -> u128 {
        self.0
    }

    pub(crate) fn height(self) -> Height {
        self.epoch().starting_height()
            + u64::try_from(self.epoch_position() / self.epoch().subsidy() as u128).unwrap()
    }

    pub(crate) fn epoch(self) -> Epoch {
        self.into()
    }

    pub(crate) fn third(self) -> u64 {
        u64::try_from(self.epoch_position() % self.epoch().subsidy() as u128).unwrap()
    }

    pub(crate) fn epoch_position(self) -> u128 {
        self.0 - self.epoch().starting_sat().0
    }

    pub(crate) fn decimal(self) -> Decimal {
        self.into()
    }

    pub(crate) fn rarity(self) -> Rarity {
        self.into()
    }

    pub(crate) fn is_common(self) -> bool {
        let epoch = self.epoch();
        (self.0 - epoch.starting_sat().0) % epoch.subsidy() as u128 != 0
    }

    fn from_decimal(decimal: &str) -> Result<Self> {
        let (height, offset) = decimal
            .split_once('.')
            .ok_or_else(|| anyhow!("missing period"))?;
        let height = Height(height.parse()?);
        let offset = offset.parse::<u64>()?;

        if offset >= height.subsidy() {
            bail!("invalid block offset");
        }

        Ok(height.starting_sat() + offset as u128)
    }
}

impl PartialEq<u128> for Sat {
    fn eq(&self, other: &u128) -> bool {
        self.0 == *other
    }
}

impl PartialOrd<u128> for Sat {
    fn partial_cmp(&self, other: &u128) -> Option<cmp::Ordering> {
        self.0.partial_cmp(other)
    }
}

impl Add<u128> for Sat {
    type Output = Self;

    fn add(self, other: u128) -> Sat {
        Sat(self.0 + other)
    }
}

impl AddAssign<u128> for Sat {
    fn add_assign(&mut self, other: u128) {
        *self = Sat(self.0 + other);
    }
}

impl FromStr for Sat {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.contains('.') {
            Self::from_decimal(s)
        } else {
            let sat = Self(s.parse()?);
            Ok(sat)
        }
    }
}
