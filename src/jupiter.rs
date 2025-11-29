use serde::{Deserialize, Serialize};
use std::error::Error;
use solana_sdk::transaction::Transaction;
use base64::{Engine as _, engine::general_purpose};
use reqwest;

const JUP_TOKEN_LIST_API: &str = "https://token.jup.ag/strict"; 
const DEX_API: &str = "https://api.dexscreener.com/latest/dex/tokens/";
const JUP_QUOTE_API: &str = "https://quote-api.jup.ag/v6/quote";
const JUP_SWAP_API: &str = "https://quote-api.jup.ag/v6/swap";

#[derive(Deserialize, Debug, Clone)]
pub struct JupiterToken { pub address: String, pub symbol: String, pub name: String }

#[derive(Deserialize, Debug)]
struct DexResponse { pairs: Option<Vec<PairData>> }
#[derive(Deserialize, Debug)]
struct PairData { priceUsd: Option<String>, baseToken: TokenInfo, liquidity: Option<LiquidityInfo>, fdv: Option<f64>, volume: Option<VolumeInfo>, priceChange: Option<PriceChangeInfo> }
#[derive(Deserialize, Debug)]
struct TokenInfo { symbol: String }
#[derive(Deserialize, Debug)]
struct LiquidityInfo { usd: Option<f64> }
#[derive(Deserialize, Debug)]
struct VolumeInfo { h24: Option<f64> }
#[derive(Deserialize, Debug)]
struct PriceChangeInfo { m5: Option<f64>, h1: Option<f64> }

pub struct TokenMarketData {
    pub price: f64, pub symbol: String, pub liquidity_usd: f64, pub market_cap: f64, pub volume_24h: f64, pub change_5m: f64, pub change_1h: f64
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SwapRequest { quote_response: serde_json::Value, user_public_key: String, wrap_and_unwrap_sol: bool }
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SwapResponse { swap_transaction: String }

pub async fn fetch_all_verified_tokens() -> Result<Vec<JupiterToken>, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let tokens = client.get(JUP_TOKEN_LIST_API).send().await?.json::<Vec<JupiterToken>>().await?;
    Ok(tokens)
}

pub async fn get_token_market_data(mint: &str) -> Result<TokenMarketData, Box<dyn Error + Send + Sync>> {
    let url = format!("{}{}", DEX_API, mint);
    let resp = reqwest::get(&url).await?.json::<DexResponse>().await?;

    if let Some(pairs) = resp.pairs {
        if let Some(pair) = pairs.first() {
            let price = pair.priceUsd.as_ref().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            let symbol = pair.baseToken.symbol.clone();
            let liq = pair.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);
            let mcap = pair.fdv.unwrap_or(0.0);
            let vol = pair.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
            let ch_5m = pair.priceChange.as_ref().and_then(|c| c.m5).unwrap_or(0.0);
            let ch_1h = pair.priceChange.as_ref().and_then(|c| c.h1).unwrap_or(0.0);
            return Ok(TokenMarketData { price, symbol, liquidity_usd: liq, market_cap: mcap, volume_24h: vol, change_5m: ch_5m, change_1h: ch_1h });
        }
    }
    Ok(TokenMarketData { price: 0.0, symbol: "UNK".into(), liquidity_usd: 0.0, market_cap: 0.0, volume_24h: 0.0, change_5m: 0.0, change_1h: 0.0 })
}

pub async fn get_token_info(mint: &str) -> Result<(f64, String), Box<dyn Error + Send + Sync>> {
    let data = get_token_market_data(mint).await?;
    Ok((data.price, data.symbol))
}

pub async fn get_jupiter_swap_tx(user_pubkey: &str, input_mint: &str, output_mint: &str, amount_lamports: u64, slippage_bps: u16) -> Result<Transaction, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let quote_url = format!("{}?inputMint={}&outputMint={}&amount={}&slippageBps={}", JUP_QUOTE_API, input_mint, output_mint, amount_lamports, slippage_bps);
    let quote_resp: serde_json::Value = client.get(&quote_url).send().await?.json().await?;
    if quote_resp.get("error").is_some() { return Err(format!("Errore Quote: {}", quote_resp).into()); }
    
    let swap_req = SwapRequest { quote_response: quote_resp, user_public_key: user_pubkey.to_string(), wrap_and_unwrap_sol: true };
    let swap_resp: SwapResponse = client.post(JUP_SWAP_API).json(&swap_req).send().await?.json().await?;
    
    let tx_bytes = general_purpose::STANDARD.decode(&swap_resp.swap_transaction)?;
    let transaction: Transaction = bincode::deserialize(&tx_bytes)?;
    Ok(transaction)
}