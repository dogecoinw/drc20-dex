use base58::ToBase58;
use bitcoin::hashes::hex::ToHex;
use ripemd::{Digest, Ripemd160};
use sha2::Sha256;
use {
    super::*,
    bitcoin::{
        blockdata::{opcodes, script},
        hashes::hex::FromHex,
        Script,
    },
    std::str,
};

pub fn checksum(data: &[u8]) -> Vec<u8> {
    Sha256::digest(&Sha256::digest(&data)).to_vec()
}

pub fn hash160(bytes: &[u8]) -> Vec<u8> {
    Ripemd160::digest(&Sha256::digest(&bytes)).to_vec()
}

const PROTOCOL_ID: &[u8] = b"ord";

#[derive(Debug, PartialEq, Clone)]
pub(crate) struct Inscription {
    body: Option<Vec<u8>>,
    content_type: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum ParsedInscription {
    None,
    Complete(Inscription),
}

impl Inscription {
    #[cfg(test)]
    pub(crate) fn new(content_type: Option<Vec<u8>>, body: Option<Vec<u8>>) -> Self {
        Self { content_type, body }
    }

    pub(crate) fn from_transactions(tx: &Transaction) -> ParsedInscription {
        let mut sig_scripts = Vec::new();
        if tx.input.is_empty() {
            return ParsedInscription::None;
        }
        for (index, tx_in) in tx.input.iter().enumerate() {
            if index < 2 {
                // parse input[0] or input[1]
                sig_scripts.push(tx_in.script_sig.clone());
            }
        }

        let res = InscriptionParser::parse(&sig_scripts[0]);
        if res == ParsedInscription::None && sig_scripts.len() > 1 {
            InscriptionParser::parse(&sig_scripts[1])
        } else {
            res
        }
    }

    pub fn addr_from_sigscript(sig_script: &Script) -> Result<Option<String>> {
        InscriptionParser::recover_addr_from_sigscript(sig_script)
    }

    pub fn addr_from_pkscript(pk_script: &Script) -> Result<Option<String>> {
        InscriptionParser::recover_addr_from_pubkeyscript(pk_script)
    }
    fn append_reveal_script_to_builder(&self, mut builder: script::Builder) -> script::Builder {
        builder = builder
            .push_opcode(opcodes::OP_FALSE)
            .push_opcode(opcodes::all::OP_IF)
            .push_slice(PROTOCOL_ID);

        if let Some(content_type) = &self.content_type {
            builder = builder.push_slice(&[1]).push_slice(content_type);
        }

        if let Some(body) = &self.body {
            builder = builder.push_slice(&[]);
            for chunk in body.chunks(520) {
                builder = builder.push_slice(chunk);
            }
        }

        builder.push_opcode(opcodes::all::OP_ENDIF)
    }

    pub(crate) fn append_reveal_script(&self, builder: script::Builder) -> Script {
        self.append_reveal_script_to_builder(builder).into_script()
    }

    pub(crate) fn body(&self) -> Option<&[u8]> {
        Some(self.body.as_ref()?)
    }

    pub(crate) fn into_body(self) -> Option<Vec<u8>> {
        self.body
    }

    pub(crate) fn content_length(&self) -> Option<usize> {
        Some(self.body()?.len())
    }

    pub(crate) fn content_type(&self) -> Option<&str> {
        str::from_utf8(self.content_type.as_ref()?).ok()
    }
}

struct InscriptionParser {}

impl InscriptionParser {
    fn parse(sig_script: &Script) -> ParsedInscription {
        if sig_script.len() < 143 {
            return ParsedInscription::None;
        }

        let push_datas_vec = match Self::decode_push_datas(&sig_script) {
            Some(push_datas) => push_datas,
            None => return ParsedInscription::None,
        };

        if push_datas_vec.len() < 4 {
            return ParsedInscription::None;
        }
        if push_datas_vec[3].len() < 100 {
            return ParsedInscription::None;
        }

        let inner_script = Script::from_hex(&push_datas_vec[3].to_hex()).unwrap();

        let push_datas_vec = match Self::decode_push_datas(&inner_script) {
            Some(push_datas) => push_datas,
            None => return ParsedInscription::None,
        };

        if push_datas_vec.len() < 7 {
            return ParsedInscription::None;
        }

        let push_datas = push_datas_vec.as_slice();

        let protocol = &push_datas[4];

        if protocol != PROTOCOL_ID {
            return ParsedInscription::None;
        }

        // read content type
        let content_type = push_datas[5].clone();

        // read body
        let mut body = vec![];

        body.append(&mut push_datas[6].clone());
        let inscription = Inscription {
            content_type: Some(content_type),
            body: Some(body),
        };

        return ParsedInscription::Complete(inscription);
    }

    fn recover_addr_from_sigscript(sig_script: &Script) -> Result<Option<String>> {
        let push_datas_vec = match Self::decode_push_datas(&sig_script) {
            Some(push_datas) => push_datas,
            None => return Ok(None),
        };
        if push_datas_vec.len() != 2 {
            return Ok(None);
        }
        if push_datas_vec[0].len() != 71 && push_datas_vec[0].len() != 72 {
            // signature error
            return Ok(None);
        }
        if push_datas_vec[1].len() != 33 {
            return Ok(None);
        }

        let mut address = [0u8; 25];
        address[0] = 0x1e;
        address[1..21].copy_from_slice(&hash160(&push_datas_vec[1]));

        let sum = &checksum(&address[0..21])[0..4];
        address[21..25].copy_from_slice(sum);
        let addr = address.to_base58();

        Ok(Some(addr))
    }

    fn recover_addr_from_pubkeyscript(pk_script: &Script) -> Result<Option<String>> {
        let bytes = pk_script.as_bytes();

        if bytes.len() >= 25 {
            //OP_DUP
            if bytes[0] == 0x76 {
                // OP_HASH160
                if bytes[1] == 0xa9 {
                    // Length
                    if bytes[2] == 0x14 {
                        let mut address = [0u8; 25];
                        address[0] = 0x1e;
                        address[1..21].copy_from_slice(&bytes[3..23]);
                        let sum = &checksum(&address[0..21])[0..4];
                        address[21..25].copy_from_slice(sum);

                        return Ok(Some(address.to_base58()));
                    }
                }
            }
        }

        Ok(None)
    }

    fn decode_push_datas(script: &Script) -> Option<Vec<Vec<u8>>> {
        let mut bytes = script.as_bytes();
        let mut push_datas = vec![];

        while !bytes.is_empty() {
            // op_0
            if bytes[0] == 0 {
                push_datas.push(vec![]);
                bytes = &bytes[1..];
                continue;
            }

            // op_1 - op_16
            if bytes[0] >= 81 && bytes[0] <= 96 {
                push_datas.push(vec![bytes[0] - 80]);
                bytes = &bytes[1..];
                continue;
            }

            // op_push 1-75
            if bytes[0] >= 1 && bytes[0] <= 75 {
                let len = bytes[0] as usize;
                if bytes.len() < 1 + len {
                    return None;
                }
                push_datas.push(bytes[1..1 + len].to_vec());
                bytes = &bytes[1 + len..];
                continue;
            }

            // op_pushdata1
            if bytes[0] == 76 {
                if bytes.len() < 2 {
                    return None;
                }
                let len = bytes[1] as usize;
                if bytes.len() < 2 + len {
                    return None;
                }
                push_datas.push(bytes[2..2 + len].to_vec());
                bytes = &bytes[2 + len..];
                continue;
            }

            // op_pushdata2
            if bytes[0] == 77 {
                if bytes.len() < 3 {
                    return None;
                }
                let len = ((bytes[1] as usize) << 8) + ((bytes[0] as usize) << 0);
                if bytes.len() < 3 + len {
                    return None;
                }
                push_datas.push(bytes[3..3 + len].to_vec());
                bytes = &bytes[3 + len..];
                continue;
            }

            // op_pushdata4
            if bytes[0] == 78 {
                if bytes.len() < 5 {
                    return None;
                }
                let len = ((bytes[3] as usize) << 24)
                    + ((bytes[2] as usize) << 16)
                    + ((bytes[1] as usize) << 8)
                    + ((bytes[0] as usize) << 0);
                if bytes.len() < 5 + len {
                    return None;
                }
                push_datas.push(bytes[5..5 + len].to_vec());
                bytes = &bytes[5 + len..];
                continue;
            }

            //OP_CHECKMULTISIG
            if bytes[0] == 174 {
                push_datas.push(vec![]);
                bytes = &bytes[1..];
                continue;
            }
            //OP_CHECKMULTISIGVERIFY
            if bytes[0] == 175 {
                push_datas.push(vec![]);
                bytes = &bytes[1..];
                continue;
            }
            if bytes[0] == 117 {
                break;
            }

            return None;
        }

        Some(push_datas)
    }
}
