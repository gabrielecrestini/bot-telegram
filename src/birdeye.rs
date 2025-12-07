// ═══════════════════════════════════════════════════════════════════════════════
// BIRDEYE API - Dati di Mercato Professionali per Trading Serio
// ═══════════════════════════════════════════════════════════════════════════════
//
// Birdeye fornisce:
// - Prezzi in tempo reale (WebSocket)
// - Dati OHLCV storici
// - Top traders e whale watching
// - Portfolio tracking
// - Token security score
// - Multi-chain support

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::time::Duration;

// Birdeye API Base
const BIRDEYE_API: &str = "https://public-api.birdeye.so";
const BIRDEYE_WS: &str = "wss://public-api.birdeye.so/socket";

// API Key (opzionale per rate limit più alti)
// Ottieni gratis su: https://birdeye.so/
fn get_api_key() -> Option<String> {
    std::env::var("BIRDEYE_API_KEY").ok()
}

// ═══════════════════════════════════════════════════════════════════════════════
// STRUTTURE DATI
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPrice {
    pub address: String,
    pub symbol: String,
    pub name: String,
    pub price: f64,
    pub price_change_24h: f64,
    pub volume_24h: f64,
    pub liquidity: f64,
    pub market_cap: f64,
    pub holder_count: u64,
    pub last_trade_unix_time: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenOverview {
    pub address: String,
    pub decimals: u8,
    pub symbol: String,
    pub name: String,
    pub logo_uri: String,
    pub price: f64,
    pub history_24h_price: f64,
    pub price_change_24h_percent: f64,
    pub liquidity: f64,
    pub market_cap: f64,
    pub real_mc: f64,
    pub supply: f64,
    pub holder: u64,
    pub volume_24h: f64,
    pub volume_24h_usd: f64,
    pub volume_24h_change_percent: f64,
    pub trade_24h: u64,
    pub trade_24h_change_percent: f64,
    pub unique_wallet_24h: u64,
    pub unique_wallet_24h_change_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSecurity {
    pub owner_address: Option<String>,
    pub creation_tx: Option<String>,
    pub creation_time: Option<i64>,
    pub creation_slot: Option<u64>,
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    pub is_token_2022: bool,
    pub mutable_metadata: bool,
    // Security Scores
    pub top10_holder_percent: f64,
    pub is_freeze_disabled: bool,
    pub is_mint_disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OHLCVData {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub unix_time: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopTrader {
    pub address: String,
    pub volume_24h: f64,
    pub trades_24h: u64,
    pub pnl_24h: f64,
    pub win_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletPortfolio {
    pub wallet: String,
    pub total_usd: f64,
    pub items: Vec<PortfolioItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioItem {
    pub address: String,
    pub symbol: String,
    pub name: String,
    pub logo_uri: String,
    pub balance: f64,
    pub ui_amount: f64,
    pub price_usd: f64,
    pub value_usd: f64,
    pub price_change_24h: f64,
}

// ═══════════════════════════════════════════════════════════════════════════════
// API CALLS
// ═══════════════════════════════════════════════════════════════════════════════

fn build_client() -> Result<reqwest::Client, Box<dyn Error + Send + Sync>> {
    let builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(10));
    
    Ok(builder.build()?)
}

fn build_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("accept", "application/json".parse().unwrap());
    headers.insert("x-chain", "solana".parse().unwrap());
    
    if let Some(api_key) = get_api_key() {
        headers.insert("X-API-KEY", api_key.parse().unwrap());
    }
    
    headers
}

/// Ottiene il prezzo di un token
pub async fn get_token_price(token_address: &str) -> Result<TokenPrice, Box<dyn Error + Send + Sync>> {
    let url = format!("{}/defi/price?address={}", BIRDEYE_API, token_address);
    
    let client = build_client()?;
    let response = client
        .get(&url)
        .headers(build_headers())
        .send()
        .await?;

    #[derive(Deserialize)]
    struct ApiResponse {
        success: bool,
        data: Option<PriceData>,
    }

    #[derive(Deserialize)]
    struct PriceData {
        value: f64,
        #[serde(rename = "updateUnixTime")]
        update_unix_time: i64,
        #[serde(rename = "updateHumanTime")]
        update_human_time: Option<String>,
        #[serde(rename = "priceChange24h")]
        price_change_24h: Option<f64>,
    }

    let api_response: ApiResponse = response.json().await?;
    
    if !api_response.success {
        return Err("Birdeye API error".into());
    }

    let data = api_response.data.ok_or("No price data")?;
    
    Ok(TokenPrice {
        address: token_address.to_string(),
        symbol: "".to_string(),
        name: "".to_string(),
        price: data.value,
        price_change_24h: data.price_change_24h.unwrap_or(0.0),
        volume_24h: 0.0,
        liquidity: 0.0,
        market_cap: 0.0,
        holder_count: 0,
        last_trade_unix_time: data.update_unix_time,
    })
}

/// Ottiene overview completo di un token
pub async fn get_token_overview(token_address: &str) -> Result<TokenOverview, Box<dyn Error + Send + Sync>> {
    let url = format!("{}/defi/token_overview?address={}", BIRDEYE_API, token_address);
    
    let client = build_client()?;
    let response = client
        .get(&url)
        .headers(build_headers())
        .send()
        .await?;

    #[derive(Deserialize)]
    struct ApiResponse {
        success: bool,
        data: Option<TokenOverview>,
    }

    let api_response: ApiResponse = response.json().await?;
    
    if !api_response.success {
        return Err("Birdeye token overview error".into());
    }

    api_response.data.ok_or("No token data".into())
}

/// Ottiene dati di sicurezza di un token
pub async fn get_token_security(token_address: &str) -> Result<TokenSecurity, Box<dyn Error + Send + Sync>> {
    let url = format!("{}/defi/token_security?address={}", BIRDEYE_API, token_address);
    
    let client = build_client()?;
    let response = client
        .get(&url)
        .headers(build_headers())
        .send()
        .await?;

    #[derive(Deserialize)]
    struct ApiResponse {
        success: bool,
        data: Option<TokenSecurity>,
    }

    let api_response: ApiResponse = response.json().await?;
    
    if !api_response.success {
        return Err("Birdeye security error".into());
    }

    api_response.data.ok_or("No security data".into())
}

/// Ottiene dati OHLCV (candele)
pub async fn get_ohlcv(
    token_address: &str,
    interval: &str, // 1m, 5m, 15m, 1H, 4H, 1D
    time_from: i64,
    time_to: i64,
) -> Result<Vec<OHLCVData>, Box<dyn Error + Send + Sync>> {
    let url = format!(
        "{}/defi/ohlcv?address={}&type={}&time_from={}&time_to={}",
        BIRDEYE_API, token_address, interval, time_from, time_to
    );
    
    let client = build_client()?;
    let response = client
        .get(&url)
        .headers(build_headers())
        .send()
        .await?;

    #[derive(Deserialize)]
    struct ApiResponse {
        success: bool,
        data: Option<OHLCVResponse>,
    }

    #[derive(Deserialize)]
    struct OHLCVResponse {
        items: Vec<OHLCVItem>,
    }

    #[derive(Deserialize)]
    struct OHLCVItem {
        o: f64, // open
        h: f64, // high
        l: f64, // low
        c: f64, // close
        v: f64, // volume
        #[serde(rename = "unixTime")]
        unix_time: i64,
    }

    let api_response: ApiResponse = response.json().await?;
    
    if !api_response.success {
        return Err("Birdeye OHLCV error".into());
    }

    let data = api_response.data.ok_or("No OHLCV data")?;
    
    Ok(data.items.into_iter().map(|item| OHLCVData {
        open: item.o,
        high: item.h,
        low: item.l,
        close: item.c,
        volume: item.v,
        unix_time: item.unix_time,
    }).collect())
}

/// Ottiene il portfolio di un wallet
pub async fn get_wallet_portfolio(wallet_address: &str) -> Result<WalletPortfolio, Box<dyn Error + Send + Sync>> {
    let url = format!("{}/v1/wallet/token_list?wallet={}", BIRDEYE_API, wallet_address);
    
    let client = build_client()?;
    let response = client
        .get(&url)
        .headers(build_headers())
        .send()
        .await?;

    #[derive(Deserialize)]
    struct ApiResponse {
        success: bool,
        data: Option<WalletData>,
    }

    #[derive(Deserialize)]
    struct WalletData {
        items: Vec<WalletItem>,
        #[serde(rename = "totalUsd")]
        total_usd: f64,
    }

    #[derive(Deserialize)]
    struct WalletItem {
        address: String,
        symbol: Option<String>,
        name: Option<String>,
        #[serde(rename = "logoURI")]
        logo_uri: Option<String>,
        balance: f64,
        #[serde(rename = "uiAmount")]
        ui_amount: f64,
        #[serde(rename = "priceUsd")]
        price_usd: Option<f64>,
        #[serde(rename = "valueUsd")]
        value_usd: Option<f64>,
        #[serde(rename = "priceChange24hPercent")]
        price_change_24h: Option<f64>,
    }

    let api_response: ApiResponse = response.json().await?;
    
    if !api_response.success {
        return Err("Birdeye wallet error".into());
    }

    let data = api_response.data.ok_or("No wallet data")?;
    
    Ok(WalletPortfolio {
        wallet: wallet_address.to_string(),
        total_usd: data.total_usd,
        items: data.items.into_iter().map(|item| PortfolioItem {
            address: item.address,
            symbol: item.symbol.unwrap_or_default(),
            name: item.name.unwrap_or_default(),
            logo_uri: item.logo_uri.unwrap_or_default(),
            balance: item.balance,
            ui_amount: item.ui_amount,
            price_usd: item.price_usd.unwrap_or(0.0),
            value_usd: item.value_usd.unwrap_or(0.0),
            price_change_24h: item.price_change_24h.unwrap_or(0.0),
        }).collect(),
    })
}

/// Ottiene i top gainers (token con più guadagno nelle ultime 24h)
pub async fn get_top_gainers(limit: u32) -> Result<Vec<TokenOverview>, Box<dyn Error + Send + Sync>> {
    let url = format!(
        "{}/defi/token_trending?sort_by=rank&sort_type=asc&offset=0&limit={}",
        BIRDEYE_API, limit
    );
    
    let client = build_client()?;
    let response = client
        .get(&url)
        .headers(build_headers())
        .send()
        .await?;

    #[derive(Deserialize)]
    struct ApiResponse {
        success: bool,
        data: Option<TrendingData>,
    }

    #[derive(Deserialize)]
    struct TrendingData {
        items: Vec<TokenOverview>,
    }

    let api_response: ApiResponse = response.json().await?;
    
    if !api_response.success {
        return Err("Birdeye trending error".into());
    }

    let data = api_response.data.ok_or("No trending data")?;
    Ok(data.items)
}

/// Ottiene prezzi multipli in un'unica chiamata
pub async fn get_multi_price(addresses: &[&str]) -> Result<Vec<TokenPrice>, Box<dyn Error + Send + Sync>> {
    let addresses_str = addresses.join(",");
    let url = format!("{}/defi/multi_price?list_address={}", BIRDEYE_API, addresses_str);
    
    let client = build_client()?;
    let response = client
        .get(&url)
        .headers(build_headers())
        .send()
        .await?;

    #[derive(Deserialize)]
    struct ApiResponse {
        success: bool,
        data: Option<std::collections::HashMap<String, PriceData>>,
    }

    #[derive(Deserialize)]
    struct PriceData {
        value: f64,
        #[serde(rename = "priceChange24h")]
        price_change_24h: Option<f64>,
        #[serde(rename = "updateUnixTime")]
        update_unix_time: i64,
    }

    let api_response: ApiResponse = response.json().await?;
    
    if !api_response.success {
        return Err("Birdeye multi-price error".into());
    }

    let data = api_response.data.ok_or("No multi-price data")?;
    
    Ok(data.into_iter().map(|(addr, price)| TokenPrice {
        address: addr,
        symbol: "".to_string(),
        name: "".to_string(),
        price: price.value,
        price_change_24h: price.price_change_24h.unwrap_or(0.0),
        volume_24h: 0.0,
        liquidity: 0.0,
        market_cap: 0.0,
        holder_count: 0,
        last_trade_unix_time: price.update_unix_time,
    }).collect())
}

// ═══════════════════════════════════════════════════════════════════════════════
// SAFETY SCORE CALCULATOR
// ═══════════════════════════════════════════════════════════════════════════════

/// Calcola un punteggio di sicurezza basato sui dati Birdeye
pub async fn calculate_safety_score(token_address: &str) -> Result<u8, Box<dyn Error + Send + Sync>> {
    let security = get_token_security(token_address).await?;
    let overview = get_token_overview(token_address).await.ok();
    
    let mut score = 50u8; // Base score
    
    // Mint authority disabilitato = +20
    if security.is_mint_disabled {
        score = score.saturating_add(20);
    }
    
    // Freeze authority disabilitato = +15
    if security.is_freeze_disabled {
        score = score.saturating_add(15);
    }
    
    // Metadata non mutabile = +10
    if !security.mutable_metadata {
        score = score.saturating_add(10);
    }
    
    // Top 10 holders < 50% = +10
    if security.top10_holder_percent < 50.0 {
        score = score.saturating_add(10);
    } else if security.top10_holder_percent > 80.0 {
        score = score.saturating_sub(20);
    }
    
    // Token 2022 = -5 (nuovi, meno testati)
    if security.is_token_2022 {
        score = score.saturating_sub(5);
    }
    
    // Holders count dal overview
    if let Some(ov) = overview {
        if ov.holder > 1000 {
            score = score.saturating_add(10);
        }
        if ov.volume_24h_usd > 100000.0 {
            score = score.saturating_add(10);
        }
    }
    
    Ok(score.min(100))
}

// ═══════════════════════════════════════════════════════════════════════════════
// TESTS
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_sol_price() {
        let sol_address = "So11111111111111111111111111111111111111112";
        let result = get_token_price(sol_address).await;
        assert!(result.is_ok());
        let price = result.unwrap();
        assert!(price.price > 0.0);
    }
}
