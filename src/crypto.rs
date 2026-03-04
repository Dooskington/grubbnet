extern crate bcrypt;
extern crate openssl;

use crate::error::{Error, Result};
use openssl::encrypt::Decrypter;
use openssl::hash::MessageDigest;
use openssl::pkey::PKey;
use openssl::rsa::Padding;

pub use openssl::{
    pkey::Private,
    rsa::{Rsa, RsaRef},
};

/// Decrypt some bytes, using the private key from the given Rsa key-pair.
/// Returns a tuple with the decrypted bytes and the message length, or an `Error`.
pub fn decrypt(rsa: &Rsa<Private>, bytes: &[u8]) -> Result<(Vec<u8>, usize)> {
    let pkey = PKey::from_rsa(rsa.clone()).map_err(Error::OpenSsl)?;
    let mut decrypter = Decrypter::new(&pkey).map_err(Error::OpenSsl)?;
    decrypter.set_rsa_padding(Padding::PKCS1_OAEP).map_err(Error::OpenSsl)?;
    decrypter.set_rsa_oaep_md(MessageDigest::sha256()).map_err(Error::OpenSsl)?;
    decrypter.set_rsa_mgf1_md(MessageDigest::sha256()).map_err(Error::OpenSsl)?;

    let buffer_len = decrypter.decrypt_len(bytes).map_err(Error::OpenSsl)?;
    let mut decrypted_bytes = vec![0u8; buffer_len];
    let decrypted_len = decrypter.decrypt(bytes, &mut decrypted_bytes).map_err(Error::OpenSsl)?;
    Ok((decrypted_bytes, decrypted_len))
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
