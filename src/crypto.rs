use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use rand::RngCore;

// AES-256-GCM requires a 32-byte key.
pub type AesKey = aes_gcm::Key<Aes256Gcm>;

/// Generates a new, random 32-byte key for AES-256-GCM encryption.
#[allow(dead_code)]
pub fn generate_key() -> AesKey {
    Aes256Gcm::generate_key(OsRng)
}

/// Decodes a hex-encoded key string into an AesKey.
/// Returns None if the hex is invalid or not exactly 32 bytes.
pub fn key_from_hex(hex_key: &str) -> Option<AesKey> {
    let bytes = hex::decode(hex_key).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    Some(*AesKey::from_slice(&bytes))
}

/// Encrypts the given plaintext using AES-256-GCM.
///
/// The process is:
/// 1. Generate a random 12-byte nonce. A nonce must be unique for every encryption with the same key.
/// 2. Encrypt the plaintext.
/// 3. Prepend the nonce to the resulting ciphertext. This is crucial for decryption.
/// 4. Hex-encode the combined (nonce + ciphertext) for easy transport.
///
/// Returns the hex-encoded string or an error.
pub fn encrypt(key: &AesKey, plaintext: &[u8]) -> Result<String, aes_gcm::Error> {
    let cipher = Aes256Gcm::new(key);
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher.encrypt(nonce, plaintext)?;

    let mut combined = Vec::with_capacity(nonce.len() + ciphertext.len());
    combined.extend_from_slice(nonce.as_slice());
    combined.extend_from_slice(&ciphertext);

    Ok(hex::encode(combined))
}

/// Decrypts a hex-encoded ciphertext that was encrypted with `encrypt`.
///
/// The process is:
/// 1. Hex-decode the input string.
/// 2. Split the 12-byte nonce from the front of the data.
/// 3. Decrypt the remaining ciphertext using the key and nonce.
///
/// Returns the decrypted plaintext as a String or an error if decryption fails.
pub fn decrypt(key: &AesKey, hex_ciphertext: &str) -> Result<String, String> {
    let combined = hex::decode(hex_ciphertext).map_err(|e| format!("Hex decode error: {}", e))?;

    if combined.len() < 12 {
        return Err("Ciphertext is too short to contain a nonce".to_string());
    }

    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    let cipher = Aes256Gcm::new(key);
    let plaintext_bytes = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("Decryption error: {}", e))?;

    String::from_utf8(plaintext_bytes).map_err(|e| format!("UTF-8 conversion error: {}", e))
}
