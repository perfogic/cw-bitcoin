use bitcoin::consensus::{Decodable, Encodable};
use cosmwasm_schema::cw_serde;
use ed::{Decode, Encode, Result as EncodingResult};
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};

/// A wrapper that adds core `orga` traits to types from the `bitcoin` crate.
#[cw_serde]
pub struct Adapter<T> {
    inner: T,
}

impl<T> Adapter<T> {
    /// Creates a new `Adapter` from a value.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Consumes the `Adapter` and returns the inner value.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T> From<T> for Adapter<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Default> Default for Adapter<T> {
    fn default() -> Self {
        Self {
            inner: Default::default(),
        }
    }
}

impl<T> Deref for Adapter<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> DerefMut for Adapter<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<T: Encodable> Encode for Adapter<T> {
    fn encode(&self) -> EncodingResult<Vec<u8>> {
        let mut dest: Vec<u8> = Vec::new();
        self.encode_into(&mut dest)?;
        Ok(dest)
    }

    fn encode_into<W: Write>(&self, dest: &mut W) -> EncodingResult<()> {
        self.inner.consensus_encode(dest)
    }

    fn encoding_length(&self) -> EncodingResult<usize> {
        let mut _dest: Vec<u8> = Vec::new();
        self.inner.consensus_encode(&mut _dest)
    }
}

impl<T: Decodable> Decode for Adapter<T> {
    fn decode<R: Read>(mut input: R) -> EncodingResult<Self> {
        Decodable::consensus_decode(&mut input)
    }
}

impl<T: Copy> Copy for Adapter<T> {}
