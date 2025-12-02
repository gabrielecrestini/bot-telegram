// ═══════════════════════════════════════════════════════════════════════════════
// JITO - Bundle Transactions + MEV Protection per Trading Professionale
// ═══════════════════════════════════════════════════════════════════════════════
//
// Jito fornisce:
// - Bundle Transactions (più TX in un singolo slot)
// - MEV Protection (protezione da frontrunning)
// - Priority Fee ottimizzate
// - Latenza minima (~400ms)
// - Block Engine per esecuzione garantita

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::time::Duration;
use solana_sdk::transaction::VersionedTransaction;
use solana_sdk::signature::Signature;
use base64::{Engine as _, engine::general_purpose};
use log::{info, warn, error};

// Jito Block Engine Endpoints
const JITO_MAINNET_BLOCK_ENGINE: &str = "https://mainnet.block-engine.jito.wtf";
const JITO_BUNDLE_API: &str = "https://mainnet.block-engine.jito.wtf/api/v1/bundles";
const JITO_TIP_ACCOUNTS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4bVmkzzTpq2gwWJunPxEQmP",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

// Tip minimo per bundle (in lamports) - ~0.00001 SOL
const MIN_TIP_LAMPORTS: u64 = 10_000;
// Tip raccomandato per priorità alta
const HIGH_PRIORITY_TIP: u64 = 100_000; // 0.0001 SOL

#[derive(Debug, Serialize)]
struct BundleRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct BundleResponse {
    jsonrpc: String,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    error: Option<JitoError>,
}

#[derive(Debug, Deserialize)]
struct JitoError {
    code: i64,
    message: String,
}

#[derive(Debug, Serialize)]
struct TipInstruction {
    program_id: String,
    accounts: Vec<TipAccount>,
    data: String,
}

#[derive(Debug, Serialize)]
struct TipAccount {
    pubkey: String,
    is_signer: bool,
    is_writable: bool,
}

/// Ottiene un account tip casuale per Jito
pub fn get_random_tip_account() -> &'static str {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as usize;
    JITO_TIP_ACCOUNTS[seed % JITO_TIP_ACCOUNTS.len()]
}

/// Invia una singola transazione via Jito Bundle
/// Più veloce e protetta da MEV
pub async fn send_transaction_jito(
    tx: &VersionedTransaction,
    tip_lamports: Option<u64>,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let tip = tip_lamports.unwrap_or(MIN_TIP_LAMPORTS);
    
    // Serializza la transazione
    let tx_bytes = bincode::serialize(tx)?;
    let tx_base64 = general_purpose::STANDARD.encode(&tx_bytes);
    
    // Crea bundle con singola TX
    let bundle_request = BundleRequest {
        jsonrpc: "2.0".to_string(),
        id: 1,
        method: "sendBundle".to_string(),
        params: vec![vec![tx_base64]],
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let response = client
        .post(JITO_BUNDLE_API)
        .json(&bundle_request)
        .send()
        .await?;

    let bundle_response: BundleResponse = response.json().await?;

    if let Some(error) = bundle_response.error {
        warn!("⚠️ Jito error: {} - {}", error.code, error.message);
        return Err(format!("Jito error: {}", error.message).into());
    }

    match bundle_response.result {
        Some(bundle_id) => {
            info!("🚀 Jito Bundle inviato: {} (tip: {} lamports)", bundle_id, tip);
            Ok(bundle_id)
        }
        None => Err("Jito: nessun bundle ID ricevuto".into()),
    }
}

/// Invia multiple transazioni come bundle atomico
/// Tutte le TX vengono eseguite nello stesso slot o nessuna
pub async fn send_bundle(
    transactions: Vec<VersionedTransaction>,
    tip_lamports: Option<u64>,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    if transactions.is_empty() {
        return Err("Bundle vuoto".into());
    }

    let tip = tip_lamports.unwrap_or(MIN_TIP_LAMPORTS);
    
    // Serializza tutte le transazioni
    let tx_base64_list: Vec<String> = transactions
        .iter()
        .filter_map(|tx| {
            bincode::serialize(tx).ok().map(|bytes| {
                general_purpose::STANDARD.encode(&bytes)
            })
        })
        .collect();

    if tx_base64_list.is_empty() {
        return Err("Nessuna transazione valida nel bundle".into());
    }

    let bundle_request = BundleRequest {
        jsonrpc: "2.0".to_string(),
        id: 1,
        method: "sendBundle".to_string(),
        params: vec![tx_base64_list],
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let response = client
        .post(JITO_BUNDLE_API)
        .json(&bundle_request)
        .send()
        .await?;

    let bundle_response: BundleResponse = response.json().await?;

    if let Some(error) = bundle_response.error {
        warn!("⚠️ Jito bundle error: {} - {}", error.code, error.message);
        return Err(format!("Jito error: {}", error.message).into());
    }

    match bundle_response.result {
        Some(bundle_id) => {
            info!("🚀 Jito Bundle ({} TX) inviato: {} (tip: {} lamports)", 
                transactions.len(), bundle_id, tip);
            Ok(bundle_id)
        }
        None => Err("Jito: nessun bundle ID ricevuto".into()),
    }
}

/// Ottiene lo stato di un bundle
pub async fn get_bundle_status(bundle_id: &str) -> Result<BundleStatus, Box<dyn Error + Send + Sync>> {
    #[derive(Serialize)]
    struct StatusRequest {
        jsonrpc: String,
        id: u64,
        method: String,
        params: Vec<String>,
    }

    #[derive(Deserialize)]
    struct StatusResponse {
        #[serde(default)]
        result: Option<BundleStatusResult>,
        #[serde(default)]
        error: Option<JitoError>,
    }

    #[derive(Deserialize)]
    struct BundleStatusResult {
        context: StatusContext,
        value: Vec<BundleStatusValue>,
    }

    #[derive(Deserialize)]
    struct StatusContext {
        slot: u64,
    }

    #[derive(Deserialize)]
    struct BundleStatusValue {
        bundle_id: String,
        transactions: Vec<String>,
        slot: u64,
        confirmation_status: String,
        err: Option<serde_json::Value>,
    }

    let request = StatusRequest {
        jsonrpc: "2.0".to_string(),
        id: 1,
        method: "getBundleStatuses".to_string(),
        params: vec![bundle_id.to_string()],
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let response = client
        .post(JITO_BUNDLE_API)
        .json(&request)
        .send()
        .await?;

    let status_response: StatusResponse = response.json().await?;

    if let Some(error) = status_response.error {
        return Err(format!("Jito status error: {}", error.message).into());
    }

    if let Some(result) = status_response.result {
        if let Some(value) = result.value.first() {
            return Ok(BundleStatus {
                bundle_id: value.bundle_id.clone(),
                slot: value.slot,
                status: value.confirmation_status.clone(),
                is_confirmed: value.confirmation_status == "confirmed" || 
                              value.confirmation_status == "finalized",
                error: value.err.as_ref().map(|e| e.to_string()),
            });
        }
    }

    Ok(BundleStatus {
        bundle_id: bundle_id.to_string(),
        slot: 0,
        status: "pending".to_string(),
        is_confirmed: false,
        error: None,
    })
}

#[derive(Debug, Clone)]
pub struct BundleStatus {
    pub bundle_id: String,
    pub slot: u64,
    pub status: String,
    pub is_confirmed: bool,
    pub error: Option<String>,
}

/// Attende la conferma di un bundle con retry
pub async fn wait_for_bundle_confirmation(
    bundle_id: &str,
    timeout_ms: u64,
) -> Result<BundleStatus, Box<dyn Error + Send + Sync>> {
    let start = std::time::Instant::now();
    let timeout = Duration::from_millis(timeout_ms);

    loop {
        if start.elapsed() > timeout {
            return Err("Timeout attesa conferma bundle".into());
        }

        match get_bundle_status(bundle_id).await {
            Ok(status) => {
                if status.is_confirmed {
                    info!("✅ Bundle {} confermato in slot {}", bundle_id, status.slot);
                    return Ok(status);
                }
                if let Some(ref err) = status.error {
                    warn!("❌ Bundle {} fallito: {}", bundle_id, err);
                    return Err(format!("Bundle fallito: {}", err).into());
                }
            }
            Err(e) => {
                warn!("⚠️ Errore status bundle: {}", e);
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PRIORITY FEE CALCULATOR
// ═══════════════════════════════════════════════════════════════════════════════

/// Calcola la priority fee ottimale basata sulla congestione della rete
pub async fn get_optimal_priority_fee() -> u64 {
    // In futuro: query a Jito per fee dinamiche
    // Per ora usiamo un valore fisso ottimizzato
    HIGH_PRIORITY_TIP
}

/// Crea istruzione tip per Jito
pub fn create_tip_instruction(
    payer: &solana_sdk::pubkey::Pubkey,
    tip_lamports: u64,
) -> solana_sdk::instruction::Instruction {
    let tip_account = solana_sdk::pubkey::Pubkey::from_str(get_random_tip_account()).unwrap();
    
    solana_sdk::system_instruction::transfer(payer, &tip_account, tip_lamports)
}

// ═══════════════════════════════════════════════════════════════════════════════
// SMART EXECUTOR - Sceglie il metodo migliore per inviare TX
// ═══════════════════════════════════════════════════════════════════════════════

/// Modalità di esecuzione transazione
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExecutionMode {
    /// Velocità massima via Jito (MEV protected)
    JitoBundle,
    /// Standard RPC con priority fee
    StandardRpc,
    /// Combo: prova Jito, fallback RPC
    Smart,
}

/// Risultato esecuzione smart
#[derive(Debug)]
pub struct ExecutionResult {
    pub signature: String,
    pub mode_used: ExecutionMode,
    pub latency_ms: u64,
    pub bundle_id: Option<String>,
}

impl ExecutionResult {
    pub fn new_jito(bundle_id: String, latency_ms: u64) -> Self {
        Self {
            signature: bundle_id.clone(),
            mode_used: ExecutionMode::JitoBundle,
            latency_ms,
            bundle_id: Some(bundle_id),
        }
    }

    pub fn new_rpc(signature: String, latency_ms: u64) -> Self {
        Self {
            signature,
            mode_used: ExecutionMode::StandardRpc,
            latency_ms,
            bundle_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_random_tip_account() {
        let account = get_random_tip_account();
        assert!(!account.is_empty());
        assert!(JITO_TIP_ACCOUNTS.contains(&account));
    }
}
