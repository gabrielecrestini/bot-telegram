// ═══════════════════════════════════════════════════════════════════════════════
// RAYDIUM - Client leggero per quote e swap USDT-first
// ═══════════════════════════════════════════════════════════════════════════════
//
// Raydium offre molta liquidità su pool USDT ed è utile come percorso primario
// prima di Jupiter. Questa integrazione espone API analoghe a quelle di Orca per
// ottenere quote e transazioni di swap.

use base64::{engine::general_purpose, Engine as _};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use solana_sdk::{
    hash::Hash, message::VersionedMessage, signature::Keypair, transaction::VersionedTransaction,
};
use std::{error::Error, time::Duration};

const RAYDIUM_QUOTE_API: &str = "https://api.raydium.io/v2/main/route/quote";
const RAYDIUM_SWAP_API: &str = "https://api.raydium.io/v2/main/route/swap";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaydiumRoute {
    pub id: String,
    pub input_mint: String,
    pub output_mint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaydiumQuote {
    pub in_amount: u64,
    pub out_amount: u64,
    pub min_out_amount: u64,
    pub price_impact_pct: f64,
    pub route: Option<RaydiumRoute>,
}

#[derive(Debug, Serialize)]
struct QuoteRequest {
    input_mint: String,
    output_mint: String,
    amount: u64,
    #[serde(rename = "slippageBps")]
    slippage_bps: u16,
}

#[derive(Debug, Serialize)]
struct SwapRequest {
    #[serde(rename = "userPublicKey")]
    user_public_key: String,
    #[serde(rename = "inputMint")]
    input_mint: String,
    #[serde(rename = "outputMint")]
    output_mint: String,
    amount: u64,
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

pub async fn get_quote(
    input_mint: &str,
    output_mint: &str,
    amount: u64,
    slippage_bps: u16,
) -> Result<RaydiumQuote, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let request = QuoteRequest {
        input_mint: input_mint.to_string(),
        output_mint: output_mint.to_string(),
        amount,
        slippage_bps,
    };

    let response = client.post(RAYDIUM_QUOTE_API).json(&request).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        warn!("Raydium quote fallita: {} - {}", status, body);
        return Err(format!("Raydium quote failed: {}", status).into());
    }

    let json: serde_json::Value = response.json().await?;
    let out_amount = json
        .get("outAmount")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    let min_out_amount = json
        .get("minOutAmount")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(out_amount);

    let price_impact_pct = json
        .get("priceImpactPct")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let route = json
        .get("route")
        .and_then(|r| serde_json::from_value::<RaydiumRoute>(r.clone()).ok());

    debug!(
        "Raydium quote {} -> {} | in {} | out {} | impact {:.3}%",
        input_mint,
        output_mint,
        amount,
        out_amount,
        price_impact_pct * 100.0
    );

    Ok(RaydiumQuote {
        in_amount: amount,
        out_amount,
        min_out_amount,
        price_impact_pct,
        route,
    })
}

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
        amount,
        slippage_bps,
        wrap_and_unwrap_sol: true,
    };

    let response = client.post(RAYDIUM_SWAP_API).json(&request).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        warn!("Raydium swap fallita: {} - {}", status, text);
        return Err(format!("Raydium swap failed: {}", status).into());
    }

    let swap_response: SwapResponse = response.json().await?;
    let tx_bytes = general_purpose::STANDARD.decode(&swap_response.swap_transaction)?;
    let tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;

    info!(
        "✅ Raydium swap TX ottenuta per {} -> {}",
        input_mint, output_mint
    );
    Ok(tx)
}

pub fn sign_transaction(
    tx: &VersionedTransaction,
    payer: &Keypair,
    blockhash: Hash,
) -> Result<VersionedTransaction, Box<dyn Error + Send + Sync>> {
    let mut tx = tx.clone();
    let message = match tx.message() {
        VersionedMessage::Legacy(_) => return Err("Legacy message non supportato".into()),
        VersionedMessage::V0(msg) => msg.clone(),
    };

    let mut msg_with_bh = message.clone();
    msg_with_bh.recent_blockhash = blockhash;

    let mut signed = VersionedTransaction::try_new(VersionedMessage::V0(msg_with_bh), &[payer])?;
    signed.signatures.extend_from_slice(&tx.signatures);
    Ok(signed)
}
