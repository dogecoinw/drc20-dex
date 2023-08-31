use super::*;

#[derive(Debug, PartialEq, Copy, Clone, Eq, PartialOrd, Ord)]
pub struct SatPoint {
    pub(crate) outpoint: OutPoint,
    pub(crate) offset: u64,
}

impl Display for SatPoint {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}:{}", self.outpoint, self.offset)
    }
}

impl Encodable for SatPoint {
    fn consensus_encode<S: io::Write + ?Sized>(&self, s: &mut S) -> Result<usize, io::Error> {
        let len = self.outpoint.consensus_encode(s)?;
        Ok(len + self.offset.consensus_encode(s)?)
    }
}

impl Decodable for SatPoint {
    fn consensus_decode<D: io::Read + ?Sized>(
        d: &mut D,
    ) -> Result<Self, bitcoin::consensus::encode::Error> {
        Ok(SatPoint {
            outpoint: Decodable::consensus_decode(d)?,
            offset: Decodable::consensus_decode(d)?,
        })
    }
}

impl Serialize for SatPoint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SatPoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(DeserializeFromStr::deserialize(deserializer)?.0)
    }
}

impl FromStr for SatPoint {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (outpoint, offset) = s
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("invalid satpoint: {s}"))?;

        Ok(SatPoint {
            outpoint: outpoint.parse()?,
            offset: offset.parse()?,
        })
    }
}
