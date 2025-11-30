use serde::{Deserialize, Serialize};
use std::error::Error;
use solana_sdk::transaction::Transaction;
use base64::{Engine as _, engine::general_purpose};
use reqwest;
use log::{info, warn};

const JUP_TOKEN_LIST_API: &str = "https://token.jup.ag/strict"; 
const DEX_API: &str = "https://api.dexscreener.com/latest/dex/tokens/";
const DEX_SEARCH_API: &str = "https://api.dexscreener.com/latest/dex/search?q=";
const DEX_TRENDING_API: &str = "https://api.dexscreener.com/token-boosts/top/v1";
const BIRDEYE_API: &str = "https://public-api.birdeye.so/defi/tokenlist?sort_by=v24hChangePercent&sort_type=desc&offset=0&limit=20";
const JUP_QUOTE_API: &str = "https://quote-api.jup.ag/v6/quote";
const JUP_SWAP_API: &str = "https://quote-api.jup.ag/v6/swap";
const JUP_PRICE_API: &str = "https://price.jup.ag/v6/price";

#[derive(Deserialize, Debug, Clone)]
pub struct JupiterToken { pub address: String, pub symbol: String, pub name: String }

#[derive(Deserialize, Debug)]
struct DexResponse { pairs: Option<Vec<PairData>> }

#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct PairData { 
    priceUsd: Option<String>, 
    baseToken: TokenInfo, 
    liquidity: Option<LiquidityInfo>, 
    fdv: Option<f64>, 
    volume: Option<VolumeInfo>, 
    priceChange: Option<PriceChangeInfo>,
    info: Option<TokenExtraInfo>,
}

#[derive(Deserialize, Debug)]
struct TokenInfo { 
    symbol: String,
    name: Option<String>,
    address: Option<String>,
}

#[derive(Deserialize, Debug)]
struct LiquidityInfo { usd: Option<f64> }

#[derive(Deserialize, Debug)]
struct VolumeInfo { h24: Option<f64> }

#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct PriceChangeInfo { m5: Option<f64>, h1: Option<f64>, h24: Option<f64> }

#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct TokenExtraInfo {
    imageUrl: Option<String>,
    websites: Option<Vec<WebsiteInfo>>,
    socials: Option<Vec<SocialInfo>>,
}

#[derive(Deserialize, Debug)]
struct WebsiteInfo { url: Option<String> }

#[derive(Deserialize, Debug)]
struct SocialInfo { url: Option<String>, platform: Option<String> }

/// Dati completi di un token per il frontend
#[derive(Clone, Debug, Serialize)]
pub struct TokenMarketData {
    pub address: String,
    pub price: f64, 
    pub symbol: String, 
    pub name: String,
    pub liquidity_usd: f64, 
    pub market_cap: f64, 
    pub volume_24h: f64, 
    pub change_5m: f64, 
    pub change_1h: f64,
    pub change_24h: f64,
    pub image_url: String,
    pub score: u8, // Punteggio 0-100 basato su analisi
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

/// Calcola uno score 0-100 basato su metriche del token
fn calculate_token_score(liq: f64, vol: f64, mcap: f64, change_1h: f64, change_24h: f64) -> u8 {
    let mut score: f64 = 50.0; // Base score
    
    // Liquidità (più alta = più sicuro)
    if liq > 100000.0 { score += 15.0; }
    else if liq > 50000.0 { score += 10.0; }
    else if liq > 10000.0 { score += 5.0; }
    else if liq < 5000.0 { score -= 20.0; } // Troppo bassa = rischio
    
    // Volume 24h (attività di trading)
    if vol > 500000.0 { score += 10.0; }
    else if vol > 100000.0 { score += 5.0; }
    else if vol < 10000.0 { score -= 10.0; }
    
    // Market Cap (stabilità)
    if mcap > 10_000_000.0 { score += 10.0; }
    else if mcap > 1_000_000.0 { score += 5.0; }
    
    // Momentum positivo
    if change_1h > 5.0 && change_1h < 50.0 { score += 10.0; } // Salita sana
    else if change_1h > 50.0 { score -= 5.0; } // Troppo pump = rischio dump
    else if change_1h < -20.0 { score -= 15.0; } // Crollo
    
    // Trend 24h
    if change_24h > 10.0 && change_24h < 100.0 { score += 5.0; }
    else if change_24h < -30.0 { score -= 10.0; }
    
    score.clamp(0.0, 100.0) as u8
}

/// Ottiene dati completi di un token da DexScreener
pub async fn get_token_market_data(mint: &str) -> Result<TokenMarketData, Box<dyn Error + Send + Sync>> {
    let url = format!("{}{}", DEX_API, mint);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    
    let resp = client.get(&url).send().await?.json::<DexResponse>().await?;

    if let Some(pairs) = resp.pairs {
        // Prendi la coppia con più liquidità (solitamente SOL pair)
        if let Some(pair) = pairs.iter()
            .filter(|p| p.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0) > 0.0)
            .max_by(|a, b| {
                let la = a.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);
                let lb = b.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);
                la.partial_cmp(&lb).unwrap_or(std::cmp::Ordering::Equal)
            }) 
        {
            let price = pair.priceUsd.as_ref().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            let symbol = pair.baseToken.symbol.clone();
            let name = pair.baseToken.name.clone().unwrap_or_else(|| symbol.clone());
            let address = pair.baseToken.address.clone().unwrap_or_else(|| mint.to_string());
            let liq = pair.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);
            let mcap = pair.fdv.unwrap_or(0.0);
            let vol = pair.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
            let ch_5m = pair.priceChange.as_ref().and_then(|c| c.m5).unwrap_or(0.0);
            let ch_1h = pair.priceChange.as_ref().and_then(|c| c.h1).unwrap_or(0.0);
            let ch_24h = pair.priceChange.as_ref().and_then(|c| c.h24).unwrap_or(0.0);
            
            // Immagine del token
            let image_url = pair.info.as_ref()
                .and_then(|i| i.imageUrl.clone())
                .unwrap_or_else(|| format!("https://img.jup.ag/v6/{}/logo", mint));
            
            let score = calculate_token_score(liq, vol, mcap, ch_1h, ch_24h);
            
            return Ok(TokenMarketData { 
                address,
                price, 
                symbol, 
                name,
                liquidity_usd: liq, 
                market_cap: mcap, 
                volume_24h: vol, 
                change_5m: ch_5m, 
                change_1h: ch_1h,
                change_24h: ch_24h,
                image_url,
                score,
            });
        }
    }
    
    Ok(TokenMarketData { 
        address: mint.to_string(),
        price: 0.0, 
        symbol: "UNK".into(), 
        name: "Unknown".into(),
        liquidity_usd: 0.0, 
        market_cap: 0.0, 
        volume_24h: 0.0, 
        change_5m: 0.0, 
        change_1h: 0.0,
        change_24h: 0.0,
        image_url: "".into(),
        score: 0,
    })
}

/// Cerca gemme promettenti su Solana - Tokens con potenziale di crescita
pub async fn discover_trending_gems() -> Result<Vec<TokenMarketData>, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    
    let mut gems: Vec<TokenMarketData> = Vec::new();
    
    // 1. Cerca token Solana su DexScreener con volume alto
    let search_url = "https://api.dexscreener.com/latest/dex/search?q=solana";
    if let Ok(resp) = client.get(search_url).send().await {
        if let Ok(data) = resp.json::<DexResponse>().await {
            if let Some(pairs) = data.pairs {
                for pair in pairs.iter().take(30) {
                    let liq = pair.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);
                    let vol = pair.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
                    let ch_1h = pair.priceChange.as_ref().and_then(|c| c.h1).unwrap_or(0.0);
                    let ch_24h = pair.priceChange.as_ref().and_then(|c| c.h24).unwrap_or(0.0);
                    let price = pair.priceUsd.as_ref().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                    let mcap = pair.fdv.unwrap_or(0.0);
                    
                    // Filtro qualità: liquidità minima, volume minimo, non pump & dump
                    if liq >= 10000.0 && vol >= 50000.0 && ch_1h > -10.0 && ch_1h < 100.0 && price > 0.0 {
                        let symbol = pair.baseToken.symbol.clone();
                        let name = pair.baseToken.name.clone().unwrap_or_else(|| symbol.clone());
                        let address = pair.baseToken.address.clone().unwrap_or_default();
                        
                        if address.is_empty() { continue; }
                        
                        let image_url = pair.info.as_ref()
                            .and_then(|i| i.imageUrl.clone())
                            .unwrap_or_else(|| format!("https://img.jup.ag/v6/{}/logo", address));
                        
                        let score = calculate_token_score(liq, vol, mcap, ch_1h, ch_24h);
                        
                        // Solo token con score decente
                        if score >= 40 {
                            gems.push(TokenMarketData {
                                address,
                                price,
                                symbol,
                                name,
                                liquidity_usd: liq,
                                market_cap: mcap,
                                volume_24h: vol,
                                change_5m: pair.priceChange.as_ref().and_then(|c| c.m5).unwrap_or(0.0),
                                change_1h: ch_1h,
                                change_24h: ch_24h,
                                image_url,
                                score,
                            });
                        }
                    }
                }
            }
        }
    }
    
    // Ordina per score decrescente (migliori in cima)
    gems.sort_by(|a, b| b.score.cmp(&a.score));
    
    // Rimuovi duplicati per address
    gems.dedup_by(|a, b| a.address == b.address);
    
    // Limita a top 15
    gems.truncate(15);
    
    info!("💎 Trovate {} gemme potenziali", gems.len());
    Ok(gems)
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