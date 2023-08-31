use super::*;

#[derive(Debug, PartialEq, PartialOrd)]
pub enum Rarity {
    Common,
    Uncommon,
    Rare,
    Epic,
    Legendary,
    Mythic,
}

impl Display for Rarity {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Common => "common",
                Self::Uncommon => "uncommon",
                Self::Rare => "rare",
                Self::Epic => "epic",
                Self::Legendary => "legendary",
                Self::Mythic => "mythic",
            }
        )
    }
}

impl From<Sat> for Rarity {
    fn from(sat: Sat) -> Self {
        if sat.0 == 0 {
            return Self::Mythic;
        } else if sat == sat.epoch().starting_sat() {
            return Self::Legendary;
        } else if !sat.is_common() {
            return Self::Uncommon;
        } else {
            return Self::Common;
        }
    }
}

impl FromStr for Rarity {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "common" => Ok(Self::Common),
            "uncommon" => Ok(Self::Uncommon),
            "rare" => Ok(Self::Rare),
            "epic" => Ok(Self::Epic),
            "legendary" => Ok(Self::Legendary),
            "mythic" => Ok(Self::Mythic),
            _ => Err(anyhow!("invalid rarity: {s}")),
        }
    }
}

impl Serialize for Rarity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Rarity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(DeserializeFromStr::deserialize(deserializer)?.0)
    }
}
