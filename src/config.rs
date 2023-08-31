use super::*;

#[derive(Deserialize, Default, PartialEq, Debug)]
pub(crate) struct Config {
    pub(crate) hidden: HashSet<InscriptionId>,
}

impl Config {
    pub(crate) fn is_hidden(&self, inscription_id: InscriptionId) -> bool {
        self.hidden.contains(&inscription_id)
    }
}
