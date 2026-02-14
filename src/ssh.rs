// SSH key management and signing module
// Supports both ssh-agent and direct file-based signing

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use ssh_key::PublicKey;

/// Represents a loaded SSH key (either from agent or file)
#[derive(Debug, Clone)]
pub struct SshKey {
    /// Display name (e.g., "id_ed25519" or key comment from agent)
    pub name: String,
    /// Full OpenSSH format public key
    pub public_key: String,
    /// Key type: "ed25519" or "rsa"
    pub key_type: String,
    /// Source of the key
    pub source: KeySource,
}

#[derive(Debug, Clone)]
pub enum KeySource {
    /// Key loaded from ssh-agent
    Agent,
    /// Key loaded from file (path to private key)
    File(PathBuf),
}

/// Result of attempting to connect to ssh-agent
pub enum AgentConnection {
    Connected(ssh_agent_client_rs::Client),
    NotAvailable(String),
}

/// Try to connect to ssh-agent
pub fn connect_to_agent() -> AgentConnection {
    let socket_path = match env::var("SSH_AUTH_SOCK") {
        Ok(path) => path,
        Err(_) => return AgentConnection::NotAvailable("SSH_AUTH_SOCK not set".to_string()),
    };

    match ssh_agent_client_rs::Client::connect(Path::new(&socket_path)) {
        Ok(client) => AgentConnection::Connected(client),
        Err(e) => AgentConnection::NotAvailable(format!("Failed to connect to ssh-agent: {}", e)),
    }
}

/// List keys from ssh-agent
pub fn list_agent_keys() -> Result<Vec<SshKey>, String> {
    let mut client = match connect_to_agent() {
        AgentConnection::Connected(c) => c,
        AgentConnection::NotAvailable(e) => return Err(e),
    };

    let identities = client
        .list_all_identities()
        .map_err(|e| format!("Failed to list identities: {}", e))?;

    let mut keys = Vec::new();
    for identity in identities {
        match identity {
            ssh_agent_client_rs::Identity::PublicKey(pk) => {
                // Get the key type from algorithm
                let key_type = match pk.algorithm() {
                    ssh_key::Algorithm::Ed25519 => "ed25519",
                    ssh_key::Algorithm::Rsa { .. } => "rsa",
                    _ => continue, // Skip unsupported key types
                };

                // Convert to OpenSSH format string
                let public_key = pk
                    .to_openssh()
                    .map_err(|e| format!("Failed to encode key: {}", e))?;

                // Get comment if available
                let name = match pk.comment() {
                    c if !c.is_empty() => c.to_string(),
                    _ => key_type.to_string(),
                };

                keys.push(SshKey {
                    name,
                    public_key,
                    key_type: key_type.to_string(),
                    source: KeySource::Agent,
                });
            }
            ssh_agent_client_rs::Identity::Certificate(_) => {
                // Skip certificates for now
                continue;
            }
        }
    }

    Ok(keys)
}

/// Sign data using ssh-agent
pub fn sign_with_agent(public_key: &str, data: &[u8]) -> Result<Vec<u8>, String> {
    let mut client = match connect_to_agent() {
        AgentConnection::Connected(c) => c,
        AgentConnection::NotAvailable(e) => return Err(e),
    };

    // Parse the public key
    let key = PublicKey::from_openssh(public_key)
        .map_err(|e| format!("Failed to parse public key: {}", e))?;

    // Sign using the agent
    let signature = client
        .sign(&key, data)
        .map_err(|e| format!("Failed to sign: {}", e))?;

    // Extract raw signature bytes
    Ok(signature.as_bytes().to_vec())
}

/// Scan ~/.ssh/ directory for key files
pub fn scan_ssh_key_files() -> Vec<SshKey> {
    let ssh_dir = match dirs::home_dir() {
        Some(home) => home.join(".ssh"),
        None => return Vec::new(),
    };

    let mut keys = Vec::new();

    if let Ok(entries) = fs::read_dir(&ssh_dir) {
        for entry in entries.flatten() {
            let path = entry.path();

            // Look for .pub files
            if let Some(ext) = path.extension() {
                if ext == "pub" {
                    if let Ok(content) = fs::read_to_string(&path) {
                        let content = content.trim();

                        // Determine key type
                        let key_type = if content.starts_with("ssh-ed25519") {
                            "ed25519"
                        } else if content.starts_with("ssh-rsa") {
                            "rsa"
                        } else {
                            continue; // Skip unsupported key types
                        };

                        // Get the private key path (remove .pub extension)
                        let private_key_path = path.with_extension("");

                        // Only add if private key exists
                        if private_key_path.exists() {
                            let name = path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("unknown")
                                .to_string();

                            keys.push(SshKey {
                                name,
                                public_key: content.to_string(),
                                key_type: key_type.to_string(),
                                source: KeySource::File(private_key_path),
                            });
                        }
                    }
                }
            }
        }
    }

    // Sort: ed25519 keys first, then by name
    keys.sort_by(|a, b| match (&a.key_type[..], &b.key_type[..]) {
        ("ed25519", "rsa") => std::cmp::Ordering::Less,
        ("rsa", "ed25519") => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });

    keys
}

/// Sign data using a private key file
/// If the key is encrypted, will need passphrase
pub fn sign_with_file(
    private_key_path: &Path,
    data: &[u8],
    passphrase: Option<&str>,
) -> Result<Vec<u8>, SignError> {
    use ssh_key::private::PrivateKey;

    // Read private key file
    let key_data =
        fs::read_to_string(private_key_path).map_err(|e| SignError::FileRead(e.to_string()))?;

    // Try to parse the private key
    let private_key = if let Some(pass) = passphrase {
        // Key is encrypted, decrypt with passphrase
        PrivateKey::from_openssh(&key_data)
            .map_err(|e| SignError::Parse(e.to_string()))
            .and_then(|key| {
                if key.is_encrypted() {
                    key.decrypt(pass.as_bytes())
                        .map_err(|e| SignError::Decrypt(e.to_string()))
                } else {
                    Ok(key)
                }
            })?
    } else {
        match PrivateKey::from_openssh(&key_data) {
            Ok(key) => {
                if key.is_encrypted() {
                    return Err(SignError::NeedsPassphrase);
                }
                key
            }
            Err(e) => return Err(SignError::Parse(e.to_string())),
        }
    };

    // Sign the data based on key type
    let signature_bytes = match private_key.key_data() {
        ssh_key::private::KeypairData::Ed25519(keypair) => {
            use ed25519_dalek::{Signer, SigningKey};

            // Get the secret key bytes (first 32 bytes of the private scalar)
            let secret_bytes: [u8; 32] = keypair.private.to_bytes();
            let signing_key = SigningKey::from_bytes(&secret_bytes);
            let signature = signing_key.sign(data);
            signature.to_bytes().to_vec()
        }
        ssh_key::private::KeypairData::Rsa(keypair) => {
            use rsa::pkcs1v15::SigningKey;
            use rsa::signature::{SignatureEncoding, Signer};
            use rsa::RsaPrivateKey;
            use sha2::Sha256;

            // Reconstruct RSA private key from components
            let n = rsa::BigUint::from_bytes_be(&keypair.public.n.as_bytes());
            let e = rsa::BigUint::from_bytes_be(&keypair.public.e.as_bytes());
            let d = rsa::BigUint::from_bytes_be(&keypair.private.d.as_bytes());
            let p = rsa::BigUint::from_bytes_be(&keypair.private.p.as_bytes());
            let q = rsa::BigUint::from_bytes_be(&keypair.private.q.as_bytes());

            let primes = vec![p, q];
            let rsa_key = RsaPrivateKey::from_components(n, e, d, primes)
                .map_err(|e| SignError::Sign(format!("Failed to construct RSA key: {}", e)))?;

            let signing_key = SigningKey::<Sha256>::new(rsa_key);
            let signature = signing_key.sign(data);
            signature.to_vec()
        }
        _ => return Err(SignError::Sign("Unsupported key type".to_string())),
    };

    Ok(signature_bytes)
}

#[derive(Debug)]
pub enum SignError {
    FileRead(String),
    Parse(String),
    NeedsPassphrase,
    Decrypt(String),
    Sign(String),
}

impl std::fmt::Display for SignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignError::FileRead(e) => write!(f, "Failed to read key file: {}", e),
            SignError::Parse(e) => write!(f, "Failed to parse key: {}", e),
            SignError::NeedsPassphrase => write!(f, "Key is encrypted, passphrase required"),
            SignError::Decrypt(e) => write!(f, "Failed to decrypt key: {}", e),
            SignError::Sign(e) => write!(f, "Failed to sign: {}", e),
        }
    }
}

/// Get all available SSH keys (from agent first, then files as fallback)
pub fn get_available_keys() -> (Vec<SshKey>, bool) {
    // Try agent first
    match list_agent_keys() {
        Ok(keys) if !keys.is_empty() => (keys, true),
        _ => {
            // Fall back to file-based keys
            let keys = scan_ssh_key_files();
            (keys, false)
        }
    }
}

/// Sign a challenge using the appropriate method based on key source
pub fn sign_challenge(
    key: &SshKey,
    challenge: &[u8],
    passphrase: Option<&str>,
) -> Result<Vec<u8>, SignError> {
    match &key.source {
        KeySource::Agent => sign_with_agent(&key.public_key, challenge).map_err(SignError::Sign),
        KeySource::File(path) => sign_with_file(path, challenge, passphrase),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_ssh_keys() {
        let keys = scan_ssh_key_files();
        // Just verify it doesn't panic
        println!("Found {} SSH keys", keys.len());
        for key in &keys {
            println!("  - {} ({})", key.name, key.key_type);
        }
    }
}
