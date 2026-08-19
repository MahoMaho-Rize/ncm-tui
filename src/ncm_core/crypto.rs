use aes::Aes128;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cbc::Encryptor as CbcEncryptor;
use cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit, KeyIvInit, block_padding::Pkcs7};
use ecb::{Decryptor as EcbDecryptor, Encryptor as EcbEncryptor};
use md5::{Digest, Md5};
use num_bigint::BigUint;
use thiserror::Error;

const WEAPI_AES_KEY: &[u8; 16] = b"0CoJUm6Qyw8W8jud";
const WEAPI_AES_IV: &[u8; 16] = b"0102030405060708";
const EAPI_AES_KEY: &[u8; 16] = b"e82ckenh8dichen8";
const EAPI_DELIMITER: &str = "-36cd479b6b5-";

const WEAPI_RSA_MODULUS: &str = concat!(
    "00e0b509f6259df8642dbc35662901477df22677ec152b5ff68ace615bb7b725152",
    "b3ab17a876aea8a5aa76d2e417629ec4ee341f56135fccf695280104e0312ecbda",
    "92557c93870114af6c9d05c4f7f0c3685b7a46bee255932575cce10b424d813cfe",
    "4875d3e82047b97ddef52741d546b8e289dc6935b3ece0462db0a22b8e7"
);
const WEAPI_RSA_EXPONENT: &str = "10001";

type Aes128CbcEnc = CbcEncryptor<Aes128>;
type Aes128EcbEnc = EcbEncryptor<Aes128>;
type Aes128EcbDec = EcbDecryptor<Aes128>;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("AES key must be exactly 16 bytes")]
    InvalidKeyLength,

    #[error("AES padding or block length is invalid")]
    InvalidPadding,

    #[error("invalid built-in RSA parameter")]
    InvalidRsaParameter,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WeapiForm {
    pub params: String,
    pub enc_sec_key: String,
}

fn aes_cbc_encrypt(plaintext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, CryptoError> {
    let mut buffer = vec![0_u8; plaintext.len() + 16];
    buffer[..plaintext.len()].copy_from_slice(plaintext);
    let encrypted = Aes128CbcEnc::new(key.into(), WEAPI_AES_IV.into())
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
        .map_err(|_| CryptoError::InvalidPadding)?;
    Ok(encrypted.to_vec())
}

/// Match Python's `base64.encodebytes`: wrap at 76 columns and append a newline.
fn python_base64_encodebytes(input: &[u8]) -> String {
    let encoded = STANDARD.encode(input);
    let mut output = String::with_capacity(encoded.len() + encoded.len() / 76 + 1);
    for chunk in encoded.as_bytes().chunks(76) {
        // Base64 output is always ASCII.
        output.push_str(std::str::from_utf8(chunk).expect("base64 must be ASCII"));
        output.push('\n');
    }
    output
}

fn weapi_rsa_encrypt(secret_key: &str) -> Result<String, CryptoError> {
    let reversed: String = secret_key.chars().rev().collect();
    let message = BigUint::from_bytes_be(reversed.as_bytes());
    let exponent = BigUint::parse_bytes(WEAPI_RSA_EXPONENT.as_bytes(), 16)
        .ok_or(CryptoError::InvalidRsaParameter)?;
    let modulus = BigUint::parse_bytes(WEAPI_RSA_MODULUS.as_bytes(), 16)
        .ok_or(CryptoError::InvalidRsaParameter)?;
    let encrypted = message.modpow(&exponent, &modulus).to_str_radix(16);
    Ok(format!("{encrypted:0>256}"))
}

pub fn weapi_encrypt(plaintext_json: &str, secret_key: &str) -> Result<WeapiForm, CryptoError> {
    let secret_bytes = secret_key.as_bytes();
    if secret_bytes.len() != 16 {
        return Err(CryptoError::InvalidKeyLength);
    }
    let mut second_key = [0_u8; 16];
    second_key.copy_from_slice(secret_bytes);

    let first_cipher = aes_cbc_encrypt(plaintext_json.as_bytes(), WEAPI_AES_KEY)?;
    let first_base64 = python_base64_encodebytes(&first_cipher);
    let second_cipher = aes_cbc_encrypt(first_base64.as_bytes(), &second_key)?;

    Ok(WeapiForm {
        params: python_base64_encodebytes(&second_cipher),
        enc_sec_key: weapi_rsa_encrypt(secret_key)?,
    })
}

pub fn eapi_encrypt(api_path: &str, payload_json: &str) -> Result<String, CryptoError> {
    let digest_input = format!("nobody{api_path}use{payload_json}md5forencrypt");
    let digest = hex::encode(Md5::digest(digest_input.as_bytes()));
    let plaintext = format!("{api_path}{EAPI_DELIMITER}{payload_json}{EAPI_DELIMITER}{digest}");

    let mut buffer = vec![0_u8; plaintext.len() + 16];
    buffer[..plaintext.len()].copy_from_slice(plaintext.as_bytes());
    let encrypted = Aes128EcbEnc::new(EAPI_AES_KEY.into())
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
        .map_err(|_| CryptoError::InvalidPadding)?;
    Ok(hex::encode(encrypted))
}

pub fn eapi_decrypt(ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut buffer = ciphertext.to_vec();
    let plaintext = Aes128EcbDec::new(EAPI_AES_KEY.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map_err(|_| CryptoError::InvalidPadding)?;
    Ok(plaintext.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_base64_wraps_and_terminates_lines() {
        let encoded = python_base64_encodebytes(&[0_u8; 80]);
        let lines: Vec<_> = encoded.split_inclusive('\n').collect();
        assert_eq!(lines[0].trim_end().len(), 76);
        assert!(encoded.ends_with('\n'));
    }

    #[test]
    fn eapi_cipher_round_trip_recovers_signed_plaintext() {
        let path = "/api/test";
        let payload = r#"{"id":"123"}"#;
        let encrypted = eapi_encrypt(path, payload).unwrap();
        let decrypted = eapi_decrypt(&hex::decode(encrypted).unwrap()).unwrap();
        let decrypted = String::from_utf8(decrypted).unwrap();
        assert!(decrypted.starts_with(&format!("{path}{EAPI_DELIMITER}{payload}")));
    }
}
