extern crate bcrypt;
extern crate openssl;

use crate::error::{Error, Result};
use openssl::rsa::Padding;

pub use openssl::{
    pkey::Private,
    rsa::{Rsa, RsaRef},
};

/// Decrypt some bytes, using the private key from the given Rsa key-pair.
/// Returns a tuple with the decrypted bytes and the message length, or an `Error`.
pub fn decrypt(rsa: &Rsa<Private>, bytes: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut decrypted_bytes: Vec<u8> = vec![0; rsa.size() as usize];
    match rsa.private_decrypt(&bytes, &mut decrypted_bytes, Padding::PKCS1) {
        Ok(decrypted_len) => Ok((decrypted_bytes, decrypted_len)),
        Err(e) => Err(Error::OpenSsl(e)),
    }
}

/// Hash a plaintext string.
/// `cost` must be an integer between 4 and 31.
pub fn hash(plaintext: &str, cost: u32) -> Result<String> {
    match bcrypt::hash(plaintext, cost) {
        Ok(hashed) => Ok(hashed.to_owned()),
        Err(e) => Err(Error::Bcrypt(e)),
    }
}

/// Verify a plaintext string against a hashed string.
pub fn verify(plaintext: &str, hashed: &str) -> Result<bool> {
    match bcrypt::verify(plaintext, hashed) {
        Ok(is_match) => Ok(is_match),
        Err(e) => Err(Error::Bcrypt(e)),
    }
}
