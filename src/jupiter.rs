use serde::{Deserialize, Serialize};
use std::error::Error;
use std::time::Duration;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use solana_sdk::transaction::VersionedTransaction;
use solana_sdk::message::VersionedMessage;
use solana_sdk::signature::Keypair;
use base64::{Engine as _, engine::general_purpose};
use reqwest;
use log::{info, warn, error};
use hickory_resolver::{TokioAsyncResolver, config::{ResolverConfig, ResolverOpts, NameServerConfig, Protocol}};

// API URLs
const JUP_TOKEN_LIST_API: &str = "https://token.jup.ag/strict"; 
const DEX_API: &str = "https://api.dexscreener.com/latest/dex/tokens/";
const DEX_SEARCH_API: &str = "https://api.dexscreener.com/latest/dex/search?q=";
const DEX_TRENDING_API: &str = "https://api.dexscreener.com/token-boosts/top/v1";
const BIRDEYE_API: &str = "https://public-api.birdeye.so/defi/tokenlist?sort_by=v24hChangePercent&sort_type=desc&offset=0&limit=20";
// IMPORTANTE: Usiamo lite-api.jup.ag invece di quote-api.jup.ag
// quote-api.jup.ag NON ha record DNS A (IPv4), solo IPv6 che AWS non supporta!
const JUP_QUOTE_API: &str = "https://lite-api.jup.ag/swap/v1/quote";
const JUP_SWAP_API: &str = "https://lite-api.jup.ag/swap/v1/swap";
const JUP_PRICE_API: &str = "https://lite-api.jup.ag/price/v2/price";

// ═══════════════════════════════════════════════════════════════════════════════
// DNS RESOLVER PERSONALIZZATO - Usa Cloudflare (1.1.1.1) e Google (8.8.8.8)
// Fix per problemi DNS su AWS
// ═══════════════════════════════════════════════════════════════════════════════

/// Crea un resolver DNS personalizzato con Cloudflare e Google DNS
fn create_custom_dns_config() -> ResolverConfig {
    let mut config = ResolverConfig::new();
    
    // Cloudflare DNS (1.1.1.1 e 1.0.0.1)
    config.add_name_server(NameServerConfig {
        socket_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53),
        protocol: Protocol::Udp,
        tls_dns_name: None,
        trust_negative_responses: true,
        bind_addr: None,
    });
    config.add_name_server(NameServerConfig {
        socket_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 0, 0, 1)), 53),
        protocol: Protocol::Udp,
        tls_dns_name: None,
        trust_negative_responses: true,
        bind_addr: None,
    });
    
    // Google DNS (8.8.8.8 e 8.8.4.4)
    config.add_name_server(NameServerConfig {
        socket_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 53),
        protocol: Protocol::Udp,
        tls_dns_name: None,
        trust_negative_responses: true,
        bind_addr: None,
    });
    config.add_name_server(NameServerConfig {
        socket_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4)), 53),
        protocol: Protocol::Udp,
        tls_dns_name: None,
        trust_negative_responses: true,
        bind_addr: None,
    });
    
    config
}

/// Crea un client HTTP robusto con DNS personalizzato (Cloudflare + Google)
/// FORZA IPv4 per evitare errori AAAA su AWS (Jupiter non supporta IPv6)
fn create_http_client() -> reqwest::Client {
    // FORZA IPv4 - Risolve errore "no record found for Query AAAA"
    // Jupiter quote-api.jup.ag non ha record IPv6 (AAAA), solo IPv4 (A)
    let ipv4_addr: std::net::IpAddr = "0.0.0.0".parse().unwrap();
    
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(10)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_nodelay(true)
        // FORZA IPv4 - Questo è il fix per AWS!
        .local_address(ipv4_addr)
        // NON usare trust-dns perché cerca anche AAAA
        // Il resolver di sistema con local_address IPv4 funziona meglio
        .build()
        .unwrap_or_else(|_| {
            warn!("⚠️ Fallback a client HTTP di base");
            reqwest::Client::new()
        })
}

/// Risolve manualmente un hostname usando DNS personalizzati (1.1.1.1, 8.8.8.8)
/// Utile come fallback se trust-dns non funziona
pub async fn resolve_hostname(hostname: &str) -> Result<Vec<IpAddr>, Box<dyn Error + Send + Sync>> {
    let config = create_custom_dns_config();
    let mut opts = ResolverOpts::default();
    opts.timeout = Duration::from_secs(5);
    opts.attempts = 3;
    
    let resolver = TokioAsyncResolver::tokio(config, opts);
    
    let response = resolver.lookup_ip(hostname).await?;
    let ips: Vec<IpAddr> = response.iter().collect();
    
    if ips.is_empty() {
        Err(format!("Nessun IP trovato per {}", hostname).into())
    } else {
        Ok(ips)
    }
}

/// Esegue una richiesta GET con retry automatico e gestione DNS robusta
/// Ritenta fino a 5 volte con backoff esponenziale
async fn robust_get(url: &str) -> Result<reqwest::Response, Box<dyn Error + Send + Sync>> {
    let client = create_http_client();
    let mut last_error = String::new();
    
    for attempt in 1..=5 {
        match client.get(url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    return Ok(resp);
                } else {
                    last_error = format!("HTTP {}", resp.status());
                }
            }
            Err(e) => {
                last_error = e.to_string();
                
                // Log errore DNS specifico
                if last_error.contains("dns") || last_error.contains("resolve") || last_error.contains("connect") {
                    warn!("⚠️ DNS/Connect error (attempt {}): {}", attempt, last_error);
                }
            }
        }
        
        // Backoff esponenziale: 200ms, 400ms, 800ms, 1600ms, 3200ms
        if attempt < 5 {
            tokio::time::sleep(Duration::from_millis(200 * (1 << (attempt - 1)))).await;
        }
    }
    
    Err(format!("Request failed after 5 attempts: {}", last_error).into())
}

/// Esegue una richiesta POST con retry automatico e gestione DNS robusta
async fn robust_post<T: serde::Serialize>(url: &str, body: &T) -> Result<reqwest::Response, Box<dyn Error + Send + Sync>> {
    let client = create_http_client();
    let mut last_error = String::new();
    
    for attempt in 1..=5 {
        match client.post(url).json(body).send().await {
            Ok(resp) => {
                if resp.status().is_success() || resp.status().as_u16() < 500 {
                    return Ok(resp);
                } else {
                    last_error = format!("HTTP {}", resp.status());
                }
            }
            Err(e) => {
                last_error = e.to_string();
                
                if last_error.contains("dns") || last_error.contains("resolve") || last_error.contains("connect") {
                    warn!("⚠️ DNS/Connect error POST (attempt {}): {}", attempt, last_error);
                }
            }
        }
        
        if attempt < 5 {
            tokio::time::sleep(Duration::from_millis(200 * (1 << (attempt - 1)))).await;
        }
    }
    
    Err(format!("POST request failed after 5 attempts: {}", last_error).into())
}

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

// SwapRequest per Jupiter V6 API con priority fees ottimizzate
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SwapRequest { 
    quote_response: serde_json::Value, 
    user_public_key: String, 
    wrap_and_unwrap_sol: bool,
    // Priority fee dinamica basata su congestione rete
    #[serde(skip_serializing_if = "Option::is_none")]
    prioritization_fee_lamports: Option<u64>,
    // Compute unit price (micro-lamports per CU)
    #[serde(skip_serializing_if = "Option::is_none")]
    compute_unit_price_micro_lamports: Option<u64>,
}

/// Risposta swap da Jupiter lite-api.jup.ag/swap/v1/swap
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SwapResponse { 
    swap_transaction: String,
    // Altri campi opzionali che potremmo usare in futuro
    #[serde(default)]
    last_valid_block_height: Option<u64>,
    #[serde(default)]
    prioritization_fee_lamports: Option<u64>,
}

pub async fn fetch_all_verified_tokens() -> Result<Vec<JupiterToken>, Box<dyn Error + Send + Sync>> {
    let resp = robust_get(JUP_TOKEN_LIST_API).await?;
    let tokens = resp.json::<Vec<JupiterToken>>().await?;
    Ok(tokens)
}

// ═══════════════════════════════════════════════════════════════════════════════
// SISTEMA DI SCORING SCIENTIFICO AVANZATO
// Basato su: Liquidità, Volume, Market Cap, Momentum, Volatilità, Rapporti
// ═══════════════════════════════════════════════════════════════════════════════

/// Analisi dettagliata del potenziale di un token
#[derive(Debug, Clone)]
pub struct TokenAnalysis {
    pub liquidity_score: f64,      // 0-25 punti
    pub volume_score: f64,         // 0-20 punti
    pub mcap_potential_score: f64, // 0-20 punti
    pub momentum_score: f64,       // 0-20 punti
    pub safety_score: f64,         // 0-15 punti
    pub total_score: u8,
    pub risk_level: String,
    pub recommendation: String,
}

/// Calcola score SCIENTIFICO 0-100 basato su metriche avanzate
fn calculate_token_score(liq: f64, vol: f64, mcap: f64, change_5m: f64, change_1h: f64, change_24h: f64) -> u8 {
    let analysis = analyze_token_potential(liq, vol, mcap, change_5m, change_1h, change_24h);
    analysis.total_score
}

/// Analisi scientifica completa del token
pub fn analyze_token_potential(liq: f64, vol: f64, mcap: f64, change_5m: f64, change_1h: f64, change_24h: f64) -> TokenAnalysis {
    
    // ═══════════════════════════════════════════════════════════════
    // 1. LIQUIDITY SCORE (0-25 punti)
    // Formula: Liquidità logaritmica normalizzata
    // Logica: Più liquidità = meno slippage = più sicuro
    // ═══════════════════════════════════════════════════════════════
    let liquidity_score: f64 = if liq <= 0.0 {
        0.0
    } else {
        let log_liq = (liq.ln() - 8.0_f64).max(0.0); // ln(3000) ≈ 8
        (log_liq * 2.5).min(25.0)
        // $10k = ~5 punti, $50k = ~12 punti, $200k = ~20 punti, $1M+ = 25 punti
    };

    // ═══════════════════════════════════════════════════════════════
    // 2. VOLUME SCORE (0-20 punti)
    // Formula: Volume/Liquidity Ratio + Volume assoluto
    // Logica: Alto volume rispetto alla liquidità = interesse forte
    // ═══════════════════════════════════════════════════════════════
    let vol_liq_ratio: f64 = if liq > 0.0 { vol / liq } else { 0.0 };
    let volume_score: f64 = {
        let mut vs: f64 = 0.0;
        
        // Ratio Volume/Liquidity (0-10 punti)
        // Ratio > 2 significa che il volume giornaliero è 2x la liquidità = MOLTO attivo
        if vol_liq_ratio > 3.0 { vs += 10.0; }
        else if vol_liq_ratio > 2.0 { vs += 8.0; }
        else if vol_liq_ratio > 1.0 { vs += 6.0; }
        else if vol_liq_ratio > 0.5 { vs += 4.0; }
        else if vol_liq_ratio > 0.2 { vs += 2.0; }
        
        // Volume assoluto (0-10 punti)
        if vol > 1_000_000.0 { vs += 10.0; }
        else if vol > 500_000.0 { vs += 8.0; }
        else if vol > 100_000.0 { vs += 6.0; }
        else if vol > 50_000.0 { vs += 4.0; }
        else if vol > 20_000.0 { vs += 2.0; }
        
        vs.min(20.0)
    };

    // ═══════════════════════════════════════════════════════════════
    // 3. MARKET CAP POTENTIAL SCORE (0-20 punti)
    // Formula: Low cap + Volume alto = Potenziale esplosivo
    // Logica: Token con mcap basso MA volume alto possono fare 10-100x
    // ═══════════════════════════════════════════════════════════════
    let mcap_potential_score: f64 = {
        let mut mps: f64 = 0.0;
        
        // Sweet Spot: Market cap tra $100k e $10M con volume alto
        if mcap > 0.0 && mcap < 500_000.0 && vol > 50_000.0 {
            // MICRO CAP con volume = MASSIMO POTENZIALE (ma alto rischio)
            mps += 18.0;
        } else if mcap >= 500_000.0 && mcap < 2_000_000.0 && vol > 100_000.0 {
            // SMALL CAP con buon volume = Ottimo potenziale
            mps += 16.0;
        } else if mcap >= 2_000_000.0 && mcap < 10_000_000.0 && vol > 200_000.0 {
            // MID CAP emergente
            mps += 12.0;
        } else if mcap >= 10_000_000.0 && mcap < 50_000_000.0 {
            // Già consolidato ma può crescere
            mps += 8.0;
        } else if mcap >= 50_000_000.0 {
            // Grande = stabile ma meno upside
            mps += 4.0;
        }
        
        // Bonus: Volume/MCap ratio alto (indica interesse rispetto alla dimensione)
        let vol_mcap_ratio: f64 = if mcap > 0.0 { vol / mcap } else { 0.0 };
        if vol_mcap_ratio > 0.3 { mps += 2.0; } // Volume > 30% del mcap in 24h = molto attivo
        
        mps.min(20.0)
    };

    // ═══════════════════════════════════════════════════════════════
    // 4. MOMENTUM SCORE (0-20 punti)
    // Formula: Analisi multi-timeframe con pesi
    // Logica: Trend consistente su più timeframe = segnale forte
    // ═══════════════════════════════════════════════════════════════
    let momentum_score: f64 = {
        let mut ms: f64 = 10.0; // Base neutra
        
        // Analisi 5 minuti (reazione immediata)
        if change_5m > 3.0 && change_5m < 15.0 { ms += 3.0; } // Pump sano
        else if change_5m > 15.0 { ms -= 2.0; } // Troppo veloce = sospetto
        else if change_5m < -5.0 { ms -= 3.0; } // Dump recente
        
        // Analisi 1 ora (trend a breve)
        if change_1h > 5.0 && change_1h < 30.0 { ms += 4.0; } // Crescita sana
        else if change_1h > 30.0 && change_1h < 50.0 { ms += 2.0; } // Pump ma attenzione
        else if change_1h > 50.0 { ms -= 3.0; } // Pump & dump probabile
        else if change_1h < -15.0 { ms -= 4.0; } // Crollo
        
        // Analisi 24h (trend giornaliero)
        if change_24h > 10.0 && change_24h < 50.0 { ms += 3.0; } // Trend positivo
        else if change_24h > 50.0 && change_24h < 100.0 { ms += 1.0; } // Forte ma rischioso
        else if change_24h > 100.0 { ms -= 2.0; } // Già pumpato troppo
        else if change_24h < -20.0 { ms -= 3.0; } // Downtrend
        
        // BONUS: Trend coerente (tutti positivi o accelerazione)
        if change_5m > 0.0 && change_1h > change_5m && change_1h > 0.0 {
            ms += 2.0; // Accelerazione positiva
        }
        
        ms.clamp(0.0, 20.0)
    };

    // ═══════════════════════════════════════════════════════════════
    // 5. SAFETY SCORE (0-15 punti)
    // Formula: Rapporti di sicurezza
    // Logica: Liquidità sufficiente per uscire senza slippage eccessivo
    // ═══════════════════════════════════════════════════════════════
    let safety_score: f64 = {
        let mut ss: f64 = 0.0;
        
        // Liquidità minima assoluta
        if liq >= 50_000.0 { ss += 5.0; }
        else if liq >= 20_000.0 { ss += 3.0; }
        else if liq >= 10_000.0 { ss += 1.0; }
        else { ss -= 5.0; } // PENALITÀ: Liquidità troppo bassa
        
        // Stabilità: Volatilità non eccessiva
        let volatility: f64 = (change_1h.abs() + change_24h.abs()) / 2.0;
        if volatility < 20.0 { ss += 5.0; } // Stabile
        else if volatility < 40.0 { ss += 3.0; } // Moderata
        else if volatility > 80.0 { ss -= 3.0; } // Troppo volatile
        
        // Volume consistente (non manipolato)
        if vol > liq * 0.3 && vol < liq * 5.0 { ss += 5.0; } // Volume "normale"
        else if vol > liq * 10.0 { ss -= 2.0; } // Volume anomalo = possibile wash trading
        
        ss.clamp(0.0, 15.0)
    };

    // ═══════════════════════════════════════════════════════════════
    // CALCOLO FINALE
    // ═══════════════════════════════════════════════════════════════
    let total_raw = liquidity_score + volume_score + mcap_potential_score + momentum_score + safety_score;
    let total_score = total_raw.clamp(0.0, 100.0) as u8;
    
    // Risk Level
    let risk_level = if safety_score >= 12.0 && liq >= 100_000.0 {
        "🟢 BASSO".to_string()
    } else if safety_score >= 8.0 && liq >= 30_000.0 {
        "🟡 MEDIO".to_string()
    } else {
        "🔴 ALTO".to_string()
    };
    
    // Recommendation
    let recommendation = if total_score >= 75 {
        "💎 GEMMA - Alto potenziale, considera entry".to_string()
    } else if total_score >= 60 {
        "✅ BUONO - Metriche solide, monitora".to_string()
    } else if total_score >= 45 {
        "⚠️ CAUTO - Rischio medio, attendi conferme".to_string()
    } else {
        "❌ EVITA - Metriche deboli".to_string()
    };

    TokenAnalysis {
        liquidity_score,
        volume_score,
        mcap_potential_score,
        momentum_score,
        safety_score,
        total_score,
        risk_level,
        recommendation,
    }
}

/// Ottiene dati completi di un token da DexScreener
pub async fn get_token_market_data(mint: &str) -> Result<TokenMarketData, Box<dyn Error + Send + Sync>> {
    let url = format!("{}{}", DEX_API, mint);
    
    // Usa chiamata HTTP robusta con retry e DNS personalizzato
    let resp = robust_get(&url).await?;
    let data = resp.json::<DexResponse>().await?;

    if let Some(pairs) = data.pairs {
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
            
            // Immagine del token - DexScreener primario, Jupiter come fallback
            let image_url = pair.info.as_ref()
                .and_then(|i| i.imageUrl.clone())
                .filter(|url| !url.is_empty())
                .unwrap_or_else(|| format!("https://token-icons.s3.amazonaws.com/{}.png", mint));
            
            let score = calculate_token_score(liq, vol, mcap, ch_5m, ch_1h, ch_24h);
            
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
/// Usa multiple fonti e applica scoring scientifico avanzato
pub async fn discover_trending_gems() -> Result<Vec<TokenMarketData>, Box<dyn Error + Send + Sync>> {
    let mut gems: Vec<TokenMarketData> = Vec::new();
    
    // ═══════════════════════════════════════════════════════════════
    // FONTE 1: DexScreener Token Profiles (Nuovi/Trending)
    // Usa chiamate HTTP robuste con DNS personalizzato (1.1.1.1, 8.8.8.8)
    // ═══════════════════════════════════════════════════════════════
    let sources = vec![
        "https://api.dexscreener.com/latest/dex/search?q=solana",
        "https://api.dexscreener.com/token-profiles/latest/v1", // Nuovi token
    ];
    
    for search_url in sources {
        if let Ok(resp) = robust_get(search_url).await {
            if let Ok(data) = resp.json::<DexResponse>().await {
                if let Some(pairs) = data.pairs {
                    for pair in pairs.iter().take(50) {
                        if let Some(token_data) = process_pair(&pair) {
                            // Applica filtri avanzati
                            if passes_quality_filters(&token_data) {
                                gems.push(token_data);
                            }
                        }
                    }
                }
            }
        }
    }
    
    // ═══════════════════════════════════════════════════════════════
    // FONTE 2: Ricerca specifica per "meme", "ai", "defi" su Solana
    // ═══════════════════════════════════════════════════════════════
    let keywords = vec!["meme solana", "ai solana", "sol defi", "pump solana"];
    for keyword in keywords {
        let url = format!("https://api.dexscreener.com/latest/dex/search?q={}", keyword);
        if let Ok(resp) = robust_get(&url).await {
            if let Ok(data) = resp.json::<DexResponse>().await {
                if let Some(pairs) = data.pairs {
                    for pair in pairs.iter().take(20) {
                        if let Some(token_data) = process_pair(&pair) {
                            if passes_quality_filters(&token_data) && !gems.iter().any(|g| g.address == token_data.address) {
                                gems.push(token_data);
                            }
                        }
                    }
                }
            }
        }
    }
    
    // ═══════════════════════════════════════════════════════════════
    // RANKING FINALE
    // ═══════════════════════════════════════════════════════════════
    
    // Ordina per score decrescente
    gems.sort_by(|a, b| b.score.cmp(&a.score));
    
    // Rimuovi duplicati
    gems.dedup_by(|a, b| a.address == b.address);
    
    // Top 20 gemme
    gems.truncate(20);
    
    info!("💎 Scoperte {} gemme con scoring scientifico", gems.len());
    
    // Log delle top 5
    for (i, gem) in gems.iter().take(5).enumerate() {
        info!("  #{} {} - Score: {} | Liq: ${:.0}K | MCap: ${:.0}K | Vol: ${:.0}K | 24h: {:.1}%",
            i+1, gem.symbol, gem.score, 
            gem.liquidity_usd/1000.0, gem.market_cap/1000.0, 
            gem.volume_24h/1000.0, gem.change_24h);
    }
    
    Ok(gems)
}

/// Processa un pair da DexScreener e crea TokenMarketData
fn process_pair(pair: &PairData) -> Option<TokenMarketData> {
    let liq = pair.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);
    let vol = pair.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
    let price = pair.priceUsd.as_ref().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let mcap = pair.fdv.unwrap_or(0.0);
    let ch_5m = pair.priceChange.as_ref().and_then(|c| c.m5).unwrap_or(0.0);
    let ch_1h = pair.priceChange.as_ref().and_then(|c| c.h1).unwrap_or(0.0);
    let ch_24h = pair.priceChange.as_ref().and_then(|c| c.h24).unwrap_or(0.0);
    
    // Skip se mancano dati essenziali
    if price <= 0.0 || liq <= 0.0 { return None; }
    
    let symbol = pair.baseToken.symbol.clone();
    let name = pair.baseToken.name.clone().unwrap_or_else(|| symbol.clone());
    let address = pair.baseToken.address.clone().unwrap_or_default();
    
    if address.is_empty() || address.len() < 30 { return None; }
    
    let image_url = pair.info.as_ref()
        .and_then(|i| i.imageUrl.clone())
        .unwrap_or_else(|| format!("https://img.jup.ag/v6/{}/logo", address));
    
    let score = calculate_token_score(liq, vol, mcap, ch_5m, ch_1h, ch_24h);
    
    Some(TokenMarketData {
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
    })
}

/// Filtri di qualità avanzati per escludere token scam/dead
fn passes_quality_filters(token: &TokenMarketData) -> bool {
    // 1. Liquidità minima $5,000
    if token.liquidity_usd < 5_000.0 { return false; }
    
    // 2. Volume minimo $10,000 (deve essere attivamente tradato)
    if token.volume_24h < 10_000.0 { return false; }
    
    // 3. Score minimo 35 (il nostro scoring scientifico)
    if token.score < 35 { return false; }
    
    // 4. Non in dump estremo (> -50% in 24h = probabilmente rug)
    if token.change_24h < -50.0 { return false; }
    
    // 5. Non pump estremo recente (> +200% in 1h = pump & dump)
    if token.change_1h > 200.0 { return false; }
    
    // 6. Market cap ragionevole (non troppo alto per potenziale, non troppo basso per sicurezza)
    // Sweet spot: $50k - $100M
    if token.market_cap > 0.0 && (token.market_cap < 30_000.0 || token.market_cap > 500_000_000.0) {
        return false;
    }
    
    // 7. Volume/Liquidity ratio sano (0.1x - 10x)
    let vol_liq_ratio = token.volume_24h / token.liquidity_usd;
    if vol_liq_ratio < 0.05 || vol_liq_ratio > 20.0 { return false; }
    
    // 8. DEVE avere immagine valida (non vuota e non placeholder)
    if !is_valid_image_url(&token.image_url) {
        return false;
    }
    
    true
}

/// Verifica se un URL immagine è valido
fn is_valid_image_url(url: &str) -> bool {
    // Deve avere lunghezza minima
    if url.len() < 15 { return false; }
    
    // Deve iniziare con http
    if !url.starts_with("http") { return false; }
    
    // Non deve contenere placeholder
    let bad_patterns = ["undefined", "null", "placeholder", "default", "unknown", "missing"];
    for pattern in bad_patterns {
        if url.to_lowercase().contains(pattern) { return false; }
    }
    
    // Deve avere estensione immagine o essere da CDN noto
    let valid_sources = [
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg",
        "arweave.net", "ipfs.io", "cloudflare", "jup.ag", "dexscreener",
        "githubusercontent", "s3.amazonaws", "token-icons", "raw.githubusercontent"
    ];
    
    valid_sources.iter().any(|s| url.to_lowercase().contains(s))
}

pub async fn get_token_info(mint: &str) -> Result<(f64, String), Box<dyn Error + Send + Sync>> {
    let data = get_token_market_data(mint).await?;
    Ok((data.price, data.symbol))
}

// ═══════════════════════════════════════════════════════════════════════════════
// ALTCOIN AFFERMATE - Token con alta capitalizzazione e storico
// ═══════════════════════════════════════════════════════════════════════════════

/// Lista dei token più importanti su Solana con alta market cap
/// Questi sono token verificati, listati su exchange, con liquidità alta
const TOP_SOLANA_TOKENS: &[(&str, &str)] = &[
    // TIER 1 - Blue Chips (MCap > $1B)
    ("JUPyiwrYJFskUPiHa7hkeR1b1GdpBFq64bwMZQvvVAGMv", "JUP"),      // Jupiter
    ("EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", "WIF"),       // dogwifhat
    ("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", "BONK"),      // Bonk
    ("HZ1JovNiVvGrGNiiYv3XW5KKge5Wbtf2dqsfYfFq5pump", "PYTH"),     // Pyth
    ("jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL", "JTO"),        // Jito
    ("rndrizKT3MK1iimdxRdWabcF7Zg7AR5T4nud4EkHBof", "RENDER"),     // Render
    
    // TIER 2 - DeFi Leaders (MCap $100M-$1B)
    ("4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R", "RAY"),       // Raydium
    ("orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE", "ORCA"),       // Orca
    ("mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So", "mSOL"),       // Marinade
    ("7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs", "ETH"),       // Wormhole ETH
    ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC"),      // USDC
    
    // TIER 3 - Meme con volume (MCap $50M-$500M)
    ("A3eME5CetyZPBoWbRUwY3tSe25S6tb18ba9ZPbWk9eFJ", "PENG"),      // Peng
    ("7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr", "POPCAT"),    // Popcat  
    ("ukHH6c7mMyiWCf1b9pnWe25TSpkDDt3H5pQZgZ74J82", "BOME"),       // Book of Meme
    
    // TIER 4 - AI & Gaming
    ("nosXBVoaCTtYdLvKY6Csb4AC8JCdQKKAaWYtx2ZMoo7", "NOS"),        // Nosana
    ("SHDWyBxihqiCj6YekG2GUr7wqKLeLAMK1gHZck9pL6y", "SHDW"),       // Shadow
];

/// Recupera i dati delle altcoin più importanti ordinate per market cap
/// Ottiene dati delle top altcoin Solana
/// Usa chiamate HTTP robuste con DNS personalizzato (1.1.1.1, 8.8.8.8)
pub async fn get_top_altcoins() -> Result<Vec<TokenMarketData>, Box<dyn Error + Send + Sync>> {
    let mut tokens: Vec<TokenMarketData> = Vec::new();
    
    info!("📊 Recupero dati top altcoin Solana...");
    
    for (address, symbol) in TOP_SOLANA_TOKENS {
        match get_token_market_data(address).await {
            Ok(mut data) => {
                // Verifica che abbia dati validi
                if data.price > 0.0 && data.liquidity_usd > 10_000.0 {
                    // Usa il simbolo corretto dalla nostra lista
                    if data.symbol == "UNK" {
                        data.symbol = symbol.to_string();
                        data.name = symbol.to_string();
                    }
                    tokens.push(data);
                }
            },
            Err(e) => {
                warn!("⚠️ Errore recupero {}: {}", symbol, e);
            }
        }
        
        // Piccola pausa per non sovraccaricare l'API
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    
    // Ordina per market cap decrescente
    tokens.sort_by(|a, b| {
        b.market_cap.partial_cmp(&a.market_cap).unwrap_or(std::cmp::Ordering::Equal)
    });
    
    info!("📊 Recuperate {} altcoin con dati validi", tokens.len());
    
    Ok(tokens)
}

/// Trova altcoin con momentum positivo (potenziale profitto)
pub async fn find_profitable_altcoins() -> Result<Vec<TokenMarketData>, Box<dyn Error + Send + Sync>> {
    let mut profitable: Vec<TokenMarketData> = Vec::new();
    
    // 1. Recupera top altcoin dalla nostra lista
    let top_coins = get_top_altcoins().await?;
    
    for coin in top_coins {
        // Filtra solo quelle con momentum positivo
        if coin.change_1h > 0.0 || coin.change_24h > 2.0 {
            profitable.push(coin);
        }
    }
    
    // 2. Cerca anche su DexScreener i token Solana con più volume
    // Usa chiamate HTTP robuste con DNS personalizzato (1.1.1.1, 8.8.8.8)
    let volume_url = "https://api.dexscreener.com/latest/dex/tokens/So11111111111111111111111111111111111111112";
    if let Ok(resp) = robust_get(volume_url).await {
        if let Ok(data) = resp.json::<DexResponse>().await {
            if let Some(pairs) = data.pairs {
                for pair in pairs.iter().take(30) {
                    if let Some(token_data) = process_pair(&pair) {
                        // Solo token con alta capitalizzazione e momentum positivo
                        if token_data.market_cap > 10_000_000.0 
                           && token_data.liquidity_usd > 100_000.0
                           && (token_data.change_1h > 1.0 || token_data.change_24h > 5.0)
                           && !profitable.iter().any(|t| t.address == token_data.address)
                        {
                            profitable.push(token_data);
                        }
                    }
                }
            }
        }
    }
    
    // 3. Ordina per combinazione di market cap e momentum
    profitable.sort_by(|a, b| {
        // Score = market_cap * (1 + change_24h/100)
        let score_a = a.market_cap * (1.0 + a.change_24h.max(0.0) / 100.0);
        let score_b = b.market_cap * (1.0 + b.change_24h.max(0.0) / 100.0);
        score_b.partial_cmp(&score_a).unwrap_or(std::cmp::Ordering::Equal)
    });
    
    // Limita a 20
    profitable.truncate(20);
    
    info!("💰 Trovate {} altcoin con potenziale profitto", profitable.len());
    
    for (i, coin) in profitable.iter().take(5).enumerate() {
        info!("  #{} {} | MCap: ${:.1}M | 1h: {:+.1}% | 24h: {:+.1}%",
            i+1, coin.symbol, coin.market_cap/1_000_000.0, coin.change_1h, coin.change_24h);
    }
    
    Ok(profitable)
}

// ═══════════════════════════════════════════════════════════════════════════════
// JUPITER QUOTE - Per comparazione con altri DEX
// ═══════════════════════════════════════════════════════════════════════════════

/// Struttura Quote Jupiter per comparazione
#[derive(Debug, Clone)]
pub struct JupiterQuote {
    pub in_amount: u64,
    pub out_amount: u64,
    pub price_impact_pct: f64,
    pub slippage_bps: u16,
}

/// Ottiene solo la quote da Jupiter (senza transazione)
pub async fn get_jupiter_quote(
    input_mint: &str, 
    output_mint: &str, 
    amount_lamports: u64, 
    slippage_bps: u16
) -> Result<JupiterQuote, Box<dyn Error + Send + Sync>> {
    let quote_url = format!("{}?inputMint={}&outputMint={}&amount={}&slippageBps={}", 
        JUP_QUOTE_API, input_mint, output_mint, amount_lamports, slippage_bps);
    
    let quote_resp = match robust_get(&quote_url).await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(json) => json,
            Err(e) => return Err(format!("Quote JSON parse error: {}", e).into()),
        },
        Err(e) => return Err(format!("Quote request failed: {}", e).into()),
    };
    
    if quote_resp.get("error").is_some() { 
        return Err(format!("Jupiter Quote Error: {}", quote_resp).into()); 
    }
    
    let in_amount = quote_resp["inAmount"].as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(amount_lamports);
    
    let out_amount = quote_resp["outAmount"].as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    
    let price_impact_pct = quote_resp["priceImpactPct"].as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    
    Ok(JupiterQuote {
        in_amount,
        out_amount,
        price_impact_pct,
        slippage_bps,
    })
}

/// Ottiene transazione swap da Jupiter con priority fees ottimizzate
/// slippage_bps: 100 = 1%, 200 = 2%, ecc.
/// Include retry automatico per errori di rete
/// IMPORTANTE: Jupiter V6 restituisce VersionedTransaction
/// Usa DNS personalizzato (Cloudflare 1.1.1.1 + Google 8.8.8.8) per risolvere problemi AWS
pub async fn get_jupiter_swap_tx(user_pubkey: &str, input_mint: &str, output_mint: &str, amount_lamports: u64, slippage_bps: u16) -> Result<VersionedTransaction, Box<dyn Error + Send + Sync>> {
    let quote_url = format!("{}?inputMint={}&outputMint={}&amount={}&slippageBps={}", 
        JUP_QUOTE_API, input_mint, output_mint, amount_lamports, slippage_bps);
    
    info!("🔍 Jupiter swap: {}...→{}... | {} lamports | slippage {}bps", 
        &input_mint[..8], &output_mint[..8], amount_lamports, slippage_bps);
    
    // 1. GET Quote - Usa chiamata HTTP robusta con retry e DNS personalizzato
    let quote_resp = match robust_get(&quote_url).await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(json) => json,
            Err(e) => return Err(format!("Quote JSON parse error: {}", e).into()),
        },
        Err(e) => return Err(format!("Quote request failed: {}", e).into()),
    };
    
    if quote_resp.get("error").is_some() { 
        return Err(format!("Jupiter Quote Error: {}", quote_resp).into()); 
    }
    
    // Priority Fee Ottimizzata:
    // NOTA: lite-api NON accetta entrambi! Usa solo prioritizationFeeLamports
    // 50,000 lamports = 0.00005 SOL ≈ $0.01 - sufficiente per priorità
    let swap_req = SwapRequest { 
        quote_response: quote_resp, 
        user_public_key: user_pubkey.to_string(), 
        wrap_and_unwrap_sol: true,
        prioritization_fee_lamports: Some(50_000),  // Solo questo!
        compute_unit_price_micro_lamports: None,     // NON usare insieme!
    };
    
    // 2. POST Swap - Usa chiamata HTTP robusta con retry e DNS personalizzato
    let swap_resp = robust_post(JUP_SWAP_API, &swap_req).await?;
    
    // Prima ottieni la risposta come testo per debug
    let resp_text = swap_resp.text().await?;
    
    // Verifica se è un errore JSON
    if let Ok(error_json) = serde_json::from_str::<serde_json::Value>(&resp_text) {
        if let Some(error_msg) = error_json.get("error") {
            error!("❌ Jupiter API error: {}", error_msg);
            return Err(format!("Jupiter API error: {}", error_msg).into());
        }
        if let Some(msg) = error_json.get("message") {
            error!("❌ Jupiter API message: {}", msg);
            return Err(format!("Jupiter API: {}", msg).into());
        }
    }
    
    // Parse come SwapResponse
    match serde_json::from_str::<SwapResponse>(&resp_text) {
        Ok(data) => {
            let tx_bytes = general_purpose::STANDARD.decode(&data.swap_transaction)?;
            let transaction: VersionedTransaction = bincode::deserialize(&tx_bytes)?;
            info!("✅ Jupiter swap TX ottenuta");
            Ok(transaction)
        }
        Err(e) => {
            // Log della risposta raw per debug
            error!("❌ Jupiter swap parse error: {} | Response: {}", e, &resp_text[..resp_text.len().min(200)]);
            Err(format!("Swap parse error: {}", e).into())
        }
    }
}

/// Firma una VersionedTransaction con un Keypair
pub fn sign_versioned_transaction(
    tx: &VersionedTransaction,
    payer: &Keypair,
    recent_blockhash: solana_sdk::hash::Hash,
) -> Result<VersionedTransaction, Box<dyn Error + Send + Sync>> {
    // Crea una copia del messaggio con il nuovo blockhash
    let mut message = tx.message.clone();
    
    match &mut message {
        VersionedMessage::Legacy(m) => {
            m.recent_blockhash = recent_blockhash;
        }
        VersionedMessage::V0(m) => {
            m.recent_blockhash = recent_blockhash;
        }
    }
    
    // Crea la transazione firmata
    let signed_tx = VersionedTransaction::try_new(message, &[payer])?;
    
    Ok(signed_tx)
}