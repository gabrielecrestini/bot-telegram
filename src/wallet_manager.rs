use solana_sdk::signature::{Keypair, Signer};
use sqlx::{SqlitePool, Row}; // Importante: Row
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce
};
use rand::{rngs::OsRng, RngCore};
use std::env;
use log::{info};

// Helper per gestire gli errori
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 1. CREA WALLET UTENTE
pub async fn create_user_wallet(pool: &SqlitePool, tg_id: &str) -> Result<String> {
    // FIX: Usa sqlx::query() invece di query!() per evitare errori di compilazione
    let exists = sqlx::query("SELECT pubkey FROM users WHERE tg_id = ?")
        .bind(tg_id)
        .fetch_optional(pool)
        .await?;

    if let Some(record) = exists {
        let pubkey: String = record.get("pubkey");
        return Ok(pubkey); 
    }

    // Genera Keypair Solana Random
    let kp = Keypair::new();
    let pubkey = kp.pubkey().to_string();
    let secret_bytes = kp.to_bytes();

    // Criptazione AES-256
    let master_key = env::var("MASTER_KEY").expect("❌ Manca MASTER_KEY nel .env");
    let cipher = Aes256Gcm::new_from_slice(master_key.as_bytes())
        .map_err(|_| "Lunghezza MASTER_KEY invalida (serve 32 chars)")?;
    
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let encrypted_sk = cipher.encrypt(nonce, secret_bytes.as_ref())
        .map_err(|_| "Errore Criptazione")?;

    // Salvataggio
    let stored_value = format!("{}:{}", hex::encode(nonce_bytes), hex::encode(encrypted_sk));
    let now_str = chrono::Utc::now().to_rfc3339();

    // FIX: Query standard per INSERT
    sqlx::query("INSERT INTO users (tg_id, private_key_enc, pubkey, created_at) VALUES (?, ?, ?, ?)")
        .bind(tg_id)
        .bind(stored_value)
        .bind(&pubkey)
        .bind(now_str)
        .execute(pool)
        .await?;

    info!("👤 Creato wallet per TG {}: {}", tg_id, pubkey);
    Ok(pubkey)
}

/// 2. RECUPERA WALLET DECRIPTATO
pub async fn get_decrypted_wallet(pool: &SqlitePool, tg_id: &str) -> Result<Keypair> {
    // FIX: Query standard per SELECT
    let record = sqlx::query("SELECT private_key_enc FROM users WHERE tg_id = ?")
        .bind(tg_id)
        .fetch_optional(pool)
        .await?;

    let row = match record {
        Some(r) => r,
        None => return Err("Utente non trovato".into()),
    };

    let private_key_enc: String = row.get("private_key_enc");

    // Parsing e Decriptazione
    let parts: Vec<&str> = private_key_enc.split(':').collect();
    if parts.len() != 2 { return Err("Formato chiave non valido nel DB".into()); }
    
    let nonce_bytes = hex::decode(parts[0])?;
    let ciphertext = hex::decode(parts[1])?;

    let master_key = env::var("MASTER_KEY").expect("Manca MASTER_KEY");
    let cipher = Aes256Gcm::new_from_slice(master_key.as_bytes())?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let decrypted_bytes = cipher.decrypt(nonce, ciphertext.as_ref())
        .map_err(|_| "Decriptazione Fallita! Master Key errata?")?;

    let kp = Keypair::from_bytes(&decrypted_bytes).map_err(|_| "Keypair invalida")?;
    Ok(kp)
}