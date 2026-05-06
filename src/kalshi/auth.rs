use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::pss::BlindedSigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::RsaPrivateKey;
use sha2::Sha256;

pub struct KalshiAuth {
    pub key_id: String,
    signing_key: BlindedSigningKey<Sha256>,
}

impl KalshiAuth {
    pub fn load(key_id: String, key_path: &str) -> Result<Self> {
        let pem = std::fs::read_to_string(key_path)
            .with_context(|| format!("reading key file '{key_path}'"))?;
        // Try PKCS#8 ("PRIVATE KEY") first, fall back to PKCS#1 ("RSA PRIVATE KEY").
        let private_key = RsaPrivateKey::from_pkcs8_pem(&pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(&pem))
            .context("parsing RSA private key — expected PKCS#8 or PKCS#1 PEM")?;
        let signing_key = BlindedSigningKey::<Sha256>::new(private_key);
        Ok(Self { key_id, signing_key })
    }

    /// Produce the three Kalshi auth headers for a request.
    /// Returns (key_id, timestamp_ms_string, base64_signature).
    pub fn sign(&self, method: &str, path: &str) -> (String, String, String) {
        let ts = chrono::Utc::now().timestamp_millis().to_string();
        // Strip query string — Kalshi signs the bare path only.
        let bare_path = path.split('?').next().unwrap_or(path);
        let message = format!("{}{}{}", ts, method.to_uppercase(), bare_path);

        let mut rng = rand::thread_rng();
        let sig = self.signing_key.sign_with_rng(&mut rng, message.as_bytes());
        let sig_b64 = STANDARD.encode(sig.to_bytes());

        (self.key_id.clone(), ts, sig_b64)
    }
}
