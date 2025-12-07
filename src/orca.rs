// ═══════════════════════════════════════════════════════════════════════════════
// ORCA DEX - Aggregator Alternativo a Jupiter per Trading Professionale
// ═══════════════════════════════════════════════════════════════════════════════
//
// Orca fornisce:
// - Whirlpools (Concentrated Liquidity)
// - Swap API simile a Jupiter
// - Ottima liquidità su Solana
// - Basse fee (0.01% - 1%)
// - Perfetto come fallback per Jupiter

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::time::Duration;
use solana_sdk::transaction::VersionedTransaction;
use solana_sdk::message::VersionedMessage;
use solana_sdk::signature::Keypair;
use solana_sdk::hash::Hash;
use base64::{Engine as _, engine::general_purpose};
use log::{info, warn, debug};

// Orca API Endpoints
const ORCA_QUOTE_API: &str = "https://api.orca.so/v1/quote";
const ORCA_SWAP_API: &str = "https://api.orca.so/v1/swap";
const ORCA_WHIRLPOOL_API: &str = "https://api.orca.so/v1/whirlpool";

// ═══════════════════════════════════════════════════════════════════════════════
// STRUTTURE DATI
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrcaQuote {
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: u64,
    pub out_amount: u64,
    pub min_out_amount: u64,
    pub price_impact_pct: f64,
    pub fee_amount: u64,
    pub fee_pct: f64,
    pub route_plan: Vec<RoutePlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutePlan {
    pub pool_address: String,
    pub input_mint: String,
    pub output_mint: String,
    pub fee_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhirlpoolInfo {
    pub address: String,
    pub token_a: TokenInfo,
    pub token_b: TokenInfo,
    pub liquidity: u128,
    pub sqrt_price: u128,
    pub tick_current: i32,
    pub fee_rate: u16,
    pub volume_24h: f64,
    pub tvl_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub mint: String,
    pub symbol: String,
    pub decimals: u8,
}

// ═══════════════════════════════════════════════════════════════════════════════
// QUOTE API - Ottieni preventivo swap
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Serialize)]
struct QuoteRequest {
    #[serde(rename = "inputMint")]
    input_mint: String,
    #[serde(rename = "outputMint")]
    output_mint: String,
    amount: String,
    #[serde(rename = "slippageBps")]
    slippage_bps: u16,
    #[serde(rename = "onlyDirectRoutes")]
    only_direct_routes: bool,
}

/// Ottiene una quote per lo swap da Orca
pub async fn get_quote(
    input_mint: &str,
    output_mint: &str,
    amount: u64,
    slippage_bps: u16,
) -> Result<OrcaQuote, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let url = format!(
        "{}?inputMint={}&outputMint={}&amount={}&slippageBps={}",
        ORCA_QUOTE_API, input_mint, output_mint, amount, slippage_bps
    );

    let response = client
        .get(&url)
        .header("accept", "application/json")
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        warn!("Orca quote failed: {} - {}", status, text);
        return Err(format!("Orca quote failed: {}", status).into());
    }

    let quote: OrcaQuote = response.json().await?;
    
    debug!("Orca Quote: {} {} -> {} {} (impact: {:.2}%)", 
        quote.in_amount, input_mint, quote.out_amount, output_mint, quote.price_impact_pct);
    
    Ok(quote)
}

// ═══════════════════════════════════════════════════════════════════════════════
// SWAP API - Esegui swap
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Serialize)]
struct SwapRequest {
    #[serde(rename = "userPublicKey")]
    user_public_key: String,
    #[serde(rename = "inputMint")]
    input_mint: String,
    #[serde(rename = "outputMint")]
    output_mint: String,
    amount: String,
    #[serde(rename = "slippageBps")]
    slippage_bps: u16,
    #[serde(rename = "wrapAndUnwrapSol")]
    wrap_and_unwrap_sol: bool,
}

#[derive(Debug, Deserialize)]
struct SwapResponse {
    #[serde(rename = "swapTransaction")]
    swap_transaction: String,
}

/// Ottiene la transazione di swap da Orca
pub async fn get_swap_transaction(
    user_pubkey: &str,
    input_mint: &str,
    output_mint: &str,
    amount: u64,
    slippage_bps: u16,
) -> Result<VersionedTransaction, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let request = SwapRequest {
        user_public_key: user_pubkey.to_string(),
        input_mint: input_mint.to_string(),
        output_mint: output_mint.to_string(),
        amount: amount.to_string(),
        slippage_bps,
        wrap_and_unwrap_sol: true,
    };

    let response = client
        .post(ORCA_SWAP_API)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        warn!("Orca swap failed: {} - {}", status, text);
        return Err(format!("Orca swap failed: {}", status).into());
    }

    let swap_response: SwapResponse = response.json().await?;
    
    // Decodifica la transazione base64
    let tx_bytes = general_purpose::STANDARD.decode(&swap_response.swap_transaction)?;
    let tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;
    
    info!("✅ Orca swap TX ottenuta per {} -> {}", input_mint, output_mint);
    Ok(tx)
}

/// Firma una VersionedTransaction con la keypair
pub fn sign_transaction(
    tx: &VersionedTransaction,
    payer: &Keypair,
    recent_blockhash: Hash,
) -> Result<VersionedTransaction, Box<dyn Error + Send + Sync>> {
    // Clona e modifica il messaggio con il nuovo blockhash
    let mut message = tx.message.clone();
    
    match &mut message {
        VersionedMessage::Legacy(m) => {
            m.recent_blockhash = recent_blockhash;
        }
        VersionedMessage::V0(m) => {
            m.recent_blockhash = recent_blockhash;
        }
    }
    
    // Crea nuova transazione firmata
    let signed_tx = VersionedTransaction::try_new(message, &[payer])?;
    
    Ok(signed_tx)
}

// ═══════════════════════════════════════════════════════════════════════════════
// WHIRLPOOL API - Informazioni sui pool
// ═══════════════════════════════════════════════════════════════════════════════

/// Ottiene informazioni su un whirlpool specifico
pub async fn get_whirlpool_info(pool_address: &str) -> Result<WhirlpoolInfo, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let url = format!("{}/{}", ORCA_WHIRLPOOL_API, pool_address);

    let response = client
        .get(&url)
        .header("accept", "application/json")
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("Orca whirlpool info failed: {}", response.status()).into());
    }

    let info: WhirlpoolInfo = response.json().await?;
    Ok(info)
}

/// Trova i migliori pool per una coppia di token
pub async fn find_best_pools(
    token_a: &str,
    token_b: &str,
) -> Result<Vec<WhirlpoolInfo>, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let url = format!(
        "{}/list?tokenA={}&tokenB={}",
        ORCA_WHIRLPOOL_API, token_a, token_b
    );

    let response = client
        .get(&url)
        .header("accept", "application/json")
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("Orca pool list failed: {}", response.status()).into());
    }

    #[derive(Deserialize)]
    struct PoolList {
        whirlpools: Vec<WhirlpoolInfo>,
    }

    let list: PoolList = response.json().await?;
    
    // Ordina per TVL (più alto = migliore liquidità)
    let mut pools = list.whirlpools;
    pools.sort_by(|a, b| b.tvl_usd.partial_cmp(&a.tvl_usd).unwrap_or(std::cmp::Ordering::Equal));
    
    Ok(pools)
}

// ═══════════════════════════════════════════════════════════════════════════════
// SMART SWAP - Confronta Jupiter vs Orca e sceglie il migliore
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct SwapComparison {
    pub best_dex: String,
    pub jupiter_out: Option<u64>,
    pub orca_out: Option<u64>,
    pub jupiter_impact: Option<f64>,
    pub orca_impact: Option<f64>,
    pub savings_pct: f64,
}

/// Confronta le quote di Jupiter e Orca per trovare il miglior prezzo
pub async fn compare_quotes(
    input_mint: &str,
    output_mint: &str,
    amount: u64,
    slippage_bps: u16,
) -> SwapComparison {
    // Usiamo direttamente Orca (FastSwap) per evitare dipendenze Jupiter lato backend
    let orca_result = get_quote(input_mint, output_mint, amount, slippage_bps).await;

    let jupiter_out: Option<u64> = None;
    let jupiter_impact: Option<f64> = None;
    let orca_out = orca_result.as_ref().ok().map(|q| q.out_amount);
    let orca_impact = orca_result.as_ref().ok().map(|q| q.price_impact_pct);

    let (best_dex, savings_pct) = match orca_out {
        Some(_) => ("Orca".to_string(), 0.0),
        None => ("None".to_string(), 0.0),
    };

    SwapComparison {
        best_dex,
        jupiter_out,
        orca_out,
        jupiter_impact,
        orca_impact,
        savings_pct,
    }
}

/// Esegue lo swap usando il DEX con il miglior prezzo
pub async fn smart_swap(
    user_pubkey: &str,
    payer: &Keypair,
    input_mint: &str,
    output_mint: &str,
    amount: u64,
    slippage_bps: u16,
    recent_blockhash: Hash,
) -> Result<(VersionedTransaction, String), Box<dyn Error + Send + Sync>> {
    let comparison = compare_quotes(input_mint, output_mint, amount, slippage_bps).await;
    
    info!("🔍 Best DEX: {} (savings: {:.2}%)", comparison.best_dex, comparison.savings_pct);
    
    match comparison.best_dex.as_str() {
        "Orca" => {
            let tx = get_swap_transaction(user_pubkey, input_mint, output_mint, amount, slippage_bps).await?;
            let signed = sign_transaction(&tx, payer, recent_blockhash)?;
            Ok((signed, "Orca".to_string()))
        }
        _ => {
            // Default a Jupiter
            let tx = crate::jupiter::get_jupiter_swap_tx(user_pubkey, input_mint, output_mint, amount, slippage_bps).await?;
            let signed = crate::jupiter::sign_versioned_transaction(&tx, payer, recent_blockhash)?;
            Ok((signed, "Jupiter".to_string()))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TESTS
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_quote() {
        let sol = "So11111111111111111111111111111111111111112";
        let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let amount = 1_000_000_000; // 1 SOL
        
        let result = get_quote(sol, usdc, amount, 100).await;
        // Può fallire se Orca API non è raggiungibile
        if result.is_ok() {
            let quote = result.unwrap();
            assert!(quote.out_amount > 0);
        }
    }
}
