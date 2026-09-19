//! Diffie-Hellman key exchange for Telegram voice calls

use num_bigint::BigUint;
use num_traits::One;
use rand::RngCore;
use sha1::Sha1;
use sha2::{Digest, Sha256};

/// Cached DH configuration from Telegram
#[derive(Clone, Debug)]
pub struct DhConfig {
    pub g: u32,
    pub p: Vec<u8>,
    pub version: i32,
}

/// DH key exchange state for a call
#[derive(Clone, Debug)]
pub struct DhExchange {
    /// Our random secret value (a for caller, b for callee)
    secret: Vec<u8>,
    /// g^a mod p (caller) or g^b mod p (callee)
    pub g_x: Vec<u8>,
    /// SHA256(g_a) for phone.requestCall
    pub g_a_hash: Vec<u8>,
    /// The DH config used
    config: DhConfig,
}

impl DhExchange {
    /// Create a new DH exchange (generates random a, computes g^a mod p)
    pub fn new(config: &DhConfig, server_random: &[u8]) -> Self {
        // Generate 256-byte random value, XOR with server random
        let mut secret = vec![0u8; 256];
        rand::thread_rng().fill_bytes(&mut secret);

        // XOR with server random for additional entropy
        for (i, byte) in server_random.iter().enumerate() {
            if i < secret.len() {
                secret[i] ^= byte;
            }
        }

        let p = BigUint::from_bytes_be(&config.p);
        let g = BigUint::from(config.g);
        let a = BigUint::from_bytes_be(&secret);

        // g^a mod p
        let g_a = g.modpow(&a, &p);
        let g_a_bytes = g_a.to_bytes_be();

        // SHA256(g_a) for requestCall
        let g_a_hash = {
            let mut hasher = Sha256::new();
            hasher.update(&g_a_bytes);
            hasher.finalize().to_vec()
        };

        Self {
            secret,
            g_x: g_a_bytes,
            g_a_hash,
            config: config.clone(),
        }
    }

    /// Validate that a g_a or g_b value is safe (1 < g_x < p-1)
    pub fn validate_g_x(g_x: &[u8], p: &[u8]) -> bool {
        let g_x = BigUint::from_bytes_be(g_x);
        let p = BigUint::from_bytes_be(p);
        let one = BigUint::one();
        let p_minus_one = &p - &one;

        g_x > one && g_x < p_minus_one
    }

    /// Compute the shared key from the other party's g_x value
    /// Returns (shared_key, key_fingerprint)
    pub fn compute_shared_key(&self, other_g_x: &[u8]) -> Result<(Vec<u8>, i64), String> {
        let p = BigUint::from_bytes_be(&self.config.p);

        // Validate the other party's value
        if !Self::validate_g_x(other_g_x, &self.config.p) {
            return Err("Invalid g_x value: outside safe range".to_string());
        }

        let other = BigUint::from_bytes_be(other_g_x);
        let secret = BigUint::from_bytes_be(&self.secret);

        // shared_key = other_g_x ^ secret mod p
        let shared_key = other.modpow(&secret, &p);
        let mut key_bytes = shared_key.to_bytes_be();

        // Pad to 256 bytes if needed
        while key_bytes.len() < 256 {
            key_bytes.insert(0, 0);
        }

        // key_fingerprint = last 8 bytes of SHA1(key) as i64
        let fingerprint = {
            let mut hasher = Sha1::new();
            hasher.update(&key_bytes);
            let sha1_result = hasher.finalize();
            let bytes = &sha1_result[12..20];
            i64::from_le_bytes(bytes.try_into().unwrap())
        };

        Ok((key_bytes, fingerprint))
    }
}
