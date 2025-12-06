// ═══════════════════════════════════════════════════════════════════════════════
// METEORA DEX - Terza API per Trading (Dynamic AMM + DLMM)
// ═══════════════════════════════════════════════════════════════════════════════
//
// Meteora fornisce:
// - Dynamic AMM pools
// - DLMM (Dynamic Liquidity Market Maker)
// - Ottima liquidità per token popolari
// - Basse fee competitive

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::time::Duration;
use log::{debug, info, warn};

// Meteora API Endpoints  
const METEORA_API: &str = "https://dlmm-api.meteora.ag";
const METEORA_QUOTE_API: &str = "https://dlmm-api.meteora.ag/pair/all_with_pagination";

// ═══════════════════════════════════════════════════════════════════════════════
// STRUTTURE DATI
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeteoraPool {
    pub address: String,
    pub name: String,
    pub mint_x: String,
    pub mint_y: String,
    pub reserve_x: String,
    pub reserve_y: String,
    pub reserve_x_amount: u64,
    pub reserve_y_amount: u64,
    pub bin_step: u16,
    pub base_fee_percentage: String,
    pub max_fee_percentage: String,
    pub protocol_fee_percentage: String,
    pub liquidity: String,
    pub reward_mint_x: String,
    pub reward_mint_y: String,
    pub fees_24h: f64,
    pub today_fees: f64,
    pub trade_volume_24h: f64,
    pub cumulative_trade_volume: String,
    pub cumulative_fee_volume: String,
    pub current_price: f64,
    pub apr: f64,
    pub apy: f64,
    pub hide: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeteoraQuote {
    pub in_amount: u64,
    pub out_amount: u64,
    pub fee_amount: u64,
    pub price_impact: f64,
}

// ═══════════════════════════════════════════════════════════════════════════════
// API CALLS
// ═══════════════════════════════════════════════════════════════════════════════

/// Ottiene tutti i pool Meteora
pub async fn get_all_pools() -> Result<Vec<MeteoraPool>, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let response = client
        .get(METEORA_QUOTE_API)
        .header("accept", "application/json")
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("Meteora API error: {}", response.status()).into());
    }

    #[derive(Deserialize)]
    struct PoolsResponse {
        pairs: Vec<MeteoraPool>,
    }

    let data: PoolsResponse = response.json().await?;
    Ok(data.pairs)
}

/// Trova il miglior pool per una coppia di token
pub async fn find_best_pool(
    token_a: &str,
    token_b: &str,
) -> Result<Option<MeteoraPool>, Box<dyn Error + Send + Sync>> {
    let pools = get_all_pools().await?;
    
    let best = pools.into_iter()
        .filter(|p| !p.hide)
        .filter(|p| {
            (p.mint_x == token_a && p.mint_y == token_b) ||
            (p.mint_x == token_b && p.mint_y == token_a)
        })
        .max_by(|a, b| {
            a.trade_volume_24h.partial_cmp(&b.trade_volume_24h)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    
    Ok(best)
}

/// Calcola una quote approssimativa
pub async fn get_quote(
    input_mint: &str,
    output_mint: &str,
    amount: u64,
) -> Result<MeteoraQuote, Box<dyn Error + Send + Sync>> {
    let pool = find_best_pool(input_mint, output_mint).await?
        .ok_or("Nessun pool Meteora trovato")?;
    
    // Calcolo semplificato basato sul prezzo corrente
    let is_x_to_y = pool.mint_x == input_mint;
    let price = pool.current_price;
    
    let out_amount = if is_x_to_y {
        (amount as f64 * price) as u64
    } else {
        (amount as f64 / price) as u64
    };
    
    // Fee stimata (base fee)
    let fee_pct: f64 = pool.base_fee_percentage.parse().unwrap_or(0.25) / 100.0;
    let fee_amount = (out_amount as f64 * fee_pct) as u64;
    let out_after_fee = out_amount.saturating_sub(fee_amount);
    
    Ok(MeteoraQuote {
        in_amount: amount,
        out_amount: out_after_fee,
        fee_amount,
        price_impact: 0.1, // Stima conservativa
    })
}

/// Ottiene i top pool per volume
pub async fn get_top_pools(limit: usize) -> Result<Vec<MeteoraPool>, Box<dyn Error + Send + Sync>> {
    let mut pools = get_all_pools().await?;
    
    // Filtra e ordina per volume
    pools.retain(|p| !p.hide && p.trade_volume_24h > 1000.0);
    pools.sort_by(|a, b| b.trade_volume_24h.partial_cmp(&a.trade_volume_24h).unwrap_or(std::cmp::Ordering::Equal));
    pools.truncate(limit);
    
    Ok(pools)
}

/// Ottiene token profittevoli da Meteora (basato su APY)
pub async fn get_profitable_tokens() -> Result<Vec<ProfitableToken>, Box<dyn Error + Send + Sync>> {
    let pools = get_all_pools().await?;
    
    let mut tokens: Vec<ProfitableToken> = pools.iter()
        .filter(|p| !p.hide && p.apy > 10.0 && p.trade_volume_24h > 10000.0)
        .flat_map(|p| {
            vec![
                ProfitableToken {
                    address: p.mint_x.clone(),
                    volume_24h: p.trade_volume_24h,
                    apy: p.apy,
                    liquidity: p.liquidity.parse().unwrap_or(0.0),
                },
                ProfitableToken {
                    address: p.mint_y.clone(),
                    volume_24h: p.trade_volume_24h,
                    apy: p.apy,
                    liquidity: p.liquidity.parse().unwrap_or(0.0),
                },
            ]
        })
        .collect();
    
    // Deduplica e ordina per APY
    tokens.sort_by(|a, b| b.apy.partial_cmp(&a.apy).unwrap_or(std::cmp::Ordering::Equal));
    tokens.dedup_by(|a, b| a.address == b.address);
    tokens.truncate(20);
    
    Ok(tokens)
}

#[derive(Debug, Clone)]
pub struct ProfitableToken {
    pub address: String,
    pub volume_24h: f64,
    pub apy: f64,
    pub liquidity: f64,
}

// ═══════════════════════════════════════════════════════════════════════════════
// CONFRONTO 3 DEX
// ═══════════════════════════════════════════════════════════════════════════════

/// Confronta quote da Jupiter, Orca e Meteora
pub async fn compare_all_dex(
    input_mint: &str,
    output_mint: &str,
    amount: u64,
    slippage_bps: u16,
) -> DexComparison {
    // Esegui tutte le quote in parallelo
    let (jupiter, orca, meteora) = tokio::join!(
        crate::jupiter::get_jupiter_quote(input_mint, output_mint, amount, slippage_bps),
        crate::orca::get_quote(input_mint, output_mint, amount, slippage_bps),
        get_quote(input_mint, output_mint, amount)
    );

    let jupiter_out = jupiter.as_ref().ok().map(|q| q.out_amount);
    let orca_out = orca.as_ref().ok().map(|q| q.out_amount);
    let meteora_out = meteora.as_ref().ok().map(|q| q.out_amount);

    // Trova il migliore
    let mut best_dex = "Jupiter";
    let mut best_out = jupiter_out.unwrap_or(0);

    if let Some(o) = orca_out {
        if o > best_out {
            best_out = o;
            best_dex = "Orca";
        }
    }

    if let Some(m) = meteora_out {
        if m > best_out {
            best_out = m;
            best_dex = "Meteora";
        }
    }

    DexComparison {
        best_dex: best_dex.to_string(),
        best_out,
        jupiter_out,
        orca_out,
        meteora_out,
    }
}

#[derive(Debug, Clone)]
pub struct DexComparison {
    pub best_dex: String,
    pub best_out: u64,
    pub jupiter_out: Option<u64>,
    pub orca_out: Option<u64>,
    pub meteora_out: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_pools() {
        let result = get_all_pools().await;
        // Può fallire se API non raggiungibile
        if let Ok(pools) = result {
            println!("Found {} Meteora pools", pools.len());
        }
    }
}
