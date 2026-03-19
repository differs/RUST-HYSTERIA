use blake2::{Blake2b, Digest, digest::consts::U32};
use rand::RngCore;
use thiserror::Error;

pub const SALAMANDER_PSK_MIN_LEN: usize = 4;
pub const SALAMANDER_SALT_LEN: usize = 8;
const SALAMANDER_KEY_LEN: usize = 32;

type Blake2b256 = Blake2b<U32>;

pub trait Obfuscator {
    fn obfuscate(&self, input: &[u8], output: &mut [u8]) -> usize;
    fn deobfuscate(&self, input: &[u8], output: &mut [u8]) -> usize;
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SalamanderError {
    #[error("PSK must be at least {SALAMANDER_PSK_MIN_LEN} bytes")]
    PskTooShort,
}

#[derive(Debug, Clone)]
pub struct SalamanderObfuscator {
    psk: Vec<u8>,
}

impl SalamanderObfuscator {
    pub fn new(psk: impl Into<Vec<u8>>) -> Result<Self, SalamanderError> {
        let psk = psk.into();
        if psk.len() < SALAMANDER_PSK_MIN_LEN {
            return Err(SalamanderError::PskTooShort);
        }
        Ok(Self { psk })
    }

    fn key(&self, salt: &[u8]) -> [u8; SALAMANDER_KEY_LEN] {
        let mut input = Vec::with_capacity(self.psk.len() + salt.len());
        input.extend_from_slice(&self.psk);
        input.extend_from_slice(salt);
        Blake2b256::digest(input).into()
    }
}

impl Obfuscator for SalamanderObfuscator {
    fn obfuscate(&self, input: &[u8], output: &mut [u8]) -> usize {
        let out_len = input.len() + SALAMANDER_SALT_LEN;
        if output.len() < out_len {
            return 0;
        }

        let mut rng = rand::rng();
        rng.fill_bytes(&mut output[..SALAMANDER_SALT_LEN]);
        let key = self.key(&output[..SALAMANDER_SALT_LEN]);
        for (index, byte) in input.iter().enumerate() {
            output[index + SALAMANDER_SALT_LEN] = *byte ^ key[index % SALAMANDER_KEY_LEN];
        }
        out_len
    }

    fn deobfuscate(&self, input: &[u8], output: &mut [u8]) -> usize {
        let out_len = input.len().saturating_sub(SALAMANDER_SALT_LEN);
        if out_len == 0 || output.len() < out_len {
            return 0;
        }

        let key = self.key(&input[..SALAMANDER_SALT_LEN]);
        for (index, byte) in input[SALAMANDER_SALT_LEN..].iter().enumerate() {
            output[index] = *byte ^ key[index % SALAMANDER_KEY_LEN];
        }
        out_len
    }
}
