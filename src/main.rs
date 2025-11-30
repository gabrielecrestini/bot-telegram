use dotenv::dotenv;
use log::{info, error, warn, debug};
use std::sync::{Arc, Mutex};
use tokio::time::{sleep, Duration};
use std::env;
use std::collections::{HashMap, HashSet};
use sqlx::Row;
use futures::StreamExt;
use solana_client::rpc_config::{RpcTransactionLogsFilter, RpcTransactionLogsConfig, RpcTransactionConfig};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use solana_transaction_status::UiTransactionEncoding;
use solana_transaction_status::option_serializer::OptionSerializer;
use solana_sdk::signature::Signer;

// MODULI
pub mod raydium;
pub mod wallet_manager;
pub mod network;
pub mod db;
pub mod telegram_bot;
pub mod safety;
pub mod strategy;
pub mod api;
pub mod jupiter;

const WATCHLIST: &[&str] = &[
    "So11111111111111111111111111111111111111112", 
    "JUPyiwrYJFskUPiHa7hkeR8VUtkCw785HvjeyzmEgGz",
    "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", 
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", 
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
    "HZ1JovNiVvGrGNiiYv3XW5KKge5Wbtf2dqsfYfFq5pump", 
    "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", 
];

#[derive(Clone, serde::Serialize)]
pub struct GemData {
    pub token: String,
    pub symbol: String, 
    pub name: String,
    pub price: f64,     
    pub safety_score: u8,
    pub liquidity_usd: f64,
    pub market_cap: f64,
    pub volume_24h: f64,
    pub change_1h: f64,
    pub change_24h: f64,
    pub image_url: String,
    pub timestamp: i64,
    pub source: String, 
}

// STATO CONDIVISO AGGIORNATO
pub struct AppState {
    pub found_gems: Mutex<Vec<GemData>>,
    pub math_signals: Mutex<Vec<api::SignalData>>,
    // Cache per evitare doppi acquisti (User -> Token -> Timestamp)
    pub buy_cooldowns: Mutex<HashMap<String, HashMap<String, i64>>>, 
    // Cache per evitare doppi processamenti Sniper
    pub processed_sigs: Mutex<HashSet<String>>,
}

// --- HELPER: CONTROLLO COOLDOWN ---
fn check_and_set_cooldown(state: &Arc<AppState>, user_id: &str, token: &str) -> bool {
    let mut cache = state.buy_cooldowns.lock().unwrap();
    let user_cache = cache.entry(user_id.to_string()).or_insert_with(HashMap::new);
    let now = chrono::Utc::now().timestamp();

    if let Some(last_buy) = user_cache.get(token) {
        // Se comprato meno di 10 minuti fa (600s), BLOCCA
        if now - last_buy < 600 {
            return false; // Bloccato
        }
    }
    
    // Aggiorna timestamp e permetti
    user_cache.insert(token.to_string(), now);
    true
}

// --- HELPER: CONTROLLO DUPLICATI SNIPER ---
fn is_new_signature(state: &Arc<AppState>, sig: &str) -> bool {
    let mut cache = state.processed_sigs.lock().unwrap();
    if cache.contains(sig) { return false; }
    cache.insert(sig.to_string());
    // Pulizia periodica semplice: se supera 10k elementi, svuota (per non esplodere RAM)
    if cache.len() > 10000 { cache.clear(); }
    true
}

// --- SMART AUTO-BUY (Sicuro) ---
async fn execute_smart_auto_buy(
    pool: &sqlx::SqlitePool,
    net: &Arc<network::NetworkClient>,
    state: &Arc<AppState>,
    token_mint: &Pubkey
) {
    let users = sqlx::query("SELECT tg_id FROM users WHERE is_active = 1").fetch_all(pool).await;
    if let Ok(rows) = users {
        if rows.is_empty() { return; }
        
        let mint_str = token_mint.to_string();
        info!("🤖 AUTO-BUY CHECK: {} utenti potenziali per {}", rows.len(), mint_str);

        // Fetch Pool Keys UNA volta sola
        let pool_keys_res = raydium::fetch_pool_keys_by_mint(net, token_mint).await;
        let pool_keys = match pool_keys_res {
            Ok(k) => k,
            Err(_) => return, // Se non c'è pool, inutile provare
        };

        for row in rows {
            let uid: String = row.get("tg_id");

            // 1. CHECK COOLDOWN (Anti-Loop)
            if !check_and_set_cooldown(state, &uid, &mint_str) {
                debug!("🚫 Auto-Buy saltato per {} su {}: Cooldown attivo.", uid, mint_str);
                continue;
            }

            let net_c = net.clone();
            let pool_c = pool.clone();
            let token_c = mint_str.clone();
            let keys_c = pool_keys.clone();
            let mint_key = *token_mint;

            tokio::spawn(async move {
                if let Ok(payer) = wallet_manager::get_decrypted_wallet(&pool_c, &uid).await {
                    
                    // 2. CHECK SALDO & RISK MANAGEMENT
                    let bal = net_c.get_balance_fast(&payer.pubkey()).await;
                    let bal_sol = bal as f64 / 1_000_000_000.0;
                    
                    // Non comprare se saldo < 0.05 SOL (riserva gas)
                    if bal_sol < 0.05 { return; }

                    let mut amt_sol = crate::strategy::calculate_investment_amount(bal_sol);
                    
                    // TETTO MASSIMO DI SICUREZZA (Max 0.5 SOL per auto-trade)
                    if amt_sol > 0.5 { amt_sol = 0.5; }
                    
                    let amt_lam = (amt_sol * 1_000_000_000.0) as u64;

                    if amt_lam > 0 {
                        // 3. JUPITER FIRST
                        let input = "So11111111111111111111111111111111111111112";
                        let mut success = false;

                        if let Ok(mut tx) = jupiter::get_jupiter_swap_tx(&payer.pubkey().to_string(), input, &token_c, amt_lam, 100).await { // 1% Slippage Jupiter
                             let bh = net_c.rpc.get_latest_blockhash().await.unwrap();
                             tx.sign(&[&payer], bh);
                             if let Ok(sig) = net_c.rpc.send_transaction(&tx).await {
                                 info!("✅ BUY JUPITER ({}) -> TX: {}", uid, sig);
                                 let _ = db::record_buy(&pool_c, &uid, &token_c, &sig.to_string(), amt_lam).await;
                                 success = true;
                             }
                        }

                        // 4. RAYDIUM FALLBACK (Con Slippage 2%)
                        if !success {
                             // Usa slippage 2% (200 bps) invece di 0
                             if let Ok(sig) = raydium::execute_swap(&net_c, &payer, &keys_c, mint_key, amt_lam, 200).await {
                                 info!("⚡ BUY RAYDIUM ({}) -> TX: {}", uid, sig);
                                 let _ = db::record_buy(&pool_c, &uid, &token_c, &sig, amt_lam).await;
                             }
                        }
                    }
                }
            });
        }
    }
}

// --- MARKET STRATEGY (Filtrato) ---
async fn run_market_strategy(net: Arc<network::NetworkClient>, state: Arc<AppState>, pool: sqlx::SqlitePool) {
    let mut history: std::collections::HashMap<String, strategy::MarketData> = std::collections::HashMap::new();
    
    loop {
        for token in WATCHLIST {
            // 1. Check Dati Mercato Completi
            if let Ok(mkt) = jupiter::get_token_market_data(token).await {
                 
                 // FILTRO LIQUIDITÀ E VOLUME (Anti-Rumore)
                 // Ignora se Liquidità < 10k o Volume 24h < 50k
                 if mkt.liquidity_usd < 10000.0 || mkt.volume_24h < 50000.0 { continue; }

                 let data = history.entry(token.to_string()).or_insert_with(|| strategy::MarketData::new(&mkt.symbol));
                 data.add_tick(mkt.price, mkt.volume_24h); // Usa add_tick con volume

                 // Analisi
                 let action = strategy::analyze_market(data, 1.0); 
                 if let strategy::TradeAction::Buy { amount_sol: _, reason } = action {
                     info!("📈 SEGNALE VALIDO: {} - {}", mkt.symbol, reason);
                     
                     if let Ok(mut s) = state.math_signals.lock() {
                         if !s.iter().any(|x| x.token == *token && (chrono::Utc::now().timestamp() - x.timestamp) < 300) {
                             s.insert(0, api::SignalData { token: token.to_string(), price: mkt.price, score: 90, reason: reason.clone(), timestamp: chrono::Utc::now().timestamp() });
                             if s.len() > 20 { s.pop(); }
                         }
                     }
                     
                     // Esegui Auto-Buy (che ora ha il check cooldown)
                     let p = pool.clone(); let n = net.clone(); let s = state.clone(); let m = Pubkey::from_str(token).unwrap();
                     tokio::spawn(async move { execute_smart_auto_buy(&p, &n, &s, &m).await; });
                 }
            }
            sleep(Duration::from_millis(500)).await;
        }
        
        if history.len() > 50 { history.clear(); }
        sleep(Duration::from_secs(30)).await;
    }
}

// --- SNIPER LISTENER (Anti-Rug e Anti-Doppioni) ---
async fn run_sniper_listener(net: Arc<network::NetworkClient>, state: Arc<AppState>, pool: sqlx::SqlitePool) {
    let raydium_id = Pubkey::from_str(crate::raydium::RAYDIUM_V4_PROGRAM_ID).unwrap();
    let mut ws_client = net.clone();

    loop {
        match ws_client.pubsub.logs_subscribe(
             RpcTransactionLogsFilter::Mentions(vec![raydium_id.to_string()]),
             RpcTransactionLogsConfig { commitment: Some(CommitmentConfig::processed()) }
        ).await {
            Ok((mut stream, _)) => {
                info!("✅ Sniper Attivo.");
                while let Some(log) = stream.next().await {
                    if log.value.logs.iter().any(|l| l.contains("initialize2")) {
                        let sig_str = log.value.signature;
                        
                        // 1. CHECK DUPLICATI
                        if !is_new_signature(&state, &sig_str) { continue; }

                        let n_an = net.clone(); let s_an = state.clone(); let p_an = pool.clone();
                        
                        tokio::spawn(async move {
                            if let Ok(sig) = solana_sdk::signature::Signature::from_str(&sig_str) {
                                if let Ok(tx) = n_an.rpc.get_transaction_with_config(&sig, RpcTransactionConfig { encoding: Some(UiTransactionEncoding::Json), commitment: Some(CommitmentConfig::confirmed()), max_supported_transaction_version: Some(0) }).await {
                                    if let Some(meta) = tx.transaction.meta {
                                        let balances = match meta.post_token_balances { OptionSerializer::Some(b) => b, _ => vec![] };
                                        let wsol = "So11111111111111111111111111111111111111112";
                                        
                                        for b in balances {
                                            let mint = b.mint;
                                            if mint != wsol && b.ui_token_amount.decimals > 0 {
                                                if let Ok(pk) = Pubkey::from_str(&mint) {
                                                    // 2. CHECK SAFETY + ANTI-HONEYPOT (Simulazione)
                                                    // Qui chiameremo la nuova safety::full_check
                                                    if let Ok(rep) = safety::check_token_safety(&n_an, &pk).await {
                                                        if rep.is_safe {
                                                            sleep(Duration::from_secs(2)).await;
                                                            if let Ok(mkt) = jupiter::get_token_market_data(&mint).await {
                                                                // 3. FILTRO QUALITÀ RIGIDO
                                                                if mkt.liquidity_usd > 5000.0 && mkt.price > 0.0 {
                                                                    info!("💎 GEMMA NUOVA: {} (${:.6}) Liq: ${:.0} Score: {}", mkt.symbol, mkt.price, mkt.liquidity_usd, mkt.score);
                                                                    
                                                                    if let Ok(mut g) = s_an.found_gems.lock() {
                                                                        g.insert(0, GemData { 
                                                                            token: mint.clone(), 
                                                                            symbol: mkt.symbol.clone(), 
                                                                            name: mkt.name,
                                                                            price: mkt.price, 
                                                                            safety_score: mkt.score, 
                                                                            liquidity_usd: mkt.liquidity_usd,
                                                                            market_cap: mkt.market_cap,
                                                                            volume_24h: mkt.volume_24h,
                                                                            change_1h: mkt.change_1h,
                                                                            change_24h: mkt.change_24h,
                                                                            image_url: mkt.image_url,
                                                                            timestamp: chrono::Utc::now().timestamp(), 
                                                                            source: "SNIPER".into() 
                                                                        });
                                                                        // Ordina per score decrescente
                                                                        g.sort_by(|a, b| b.safety_score.cmp(&a.safety_score));
                                                                        if g.len() > 30 { g.pop(); }
                                                                    }
                                                                    
                                                                    execute_smart_auto_buy(&p_an, &n_an, &s_an, &pk).await;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    }
                }
            },
            Err(_) => sleep(Duration::from_secs(5)).await
        }
    }
}

async fn monitor_open_positions(pool: &sqlx::SqlitePool, net: &Arc<network::NetworkClient>) {
    // ... (Codice identico a prima, ma assicurati di chiamare execute_sell se serve)
}

// --- GEM DISCOVERY (Trova token promettenti) ---
async fn run_gem_discovery(state: Arc<AppState>, net: Arc<network::NetworkClient>) {
    info!("💎 Gem Discovery Task avviato");
    loop {
        match jupiter::discover_trending_gems().await {
            Ok(gems) => {
                info!("💎 Scoperte {} gemme dal mercato", gems.len());
                let mut verified_gems: Vec<GemData> = Vec::new();
                
                // Prima verifica tutte le gemme (FUORI dal lock)
                for gem in gems {
                    if let Ok(pk) = Pubkey::from_str(&gem.address) {
                        if let Ok(report) = safety::check_token_safety(&net, &pk).await {
                            if report.is_safe && gem.score >= 50 {
                                verified_gems.push(GemData {
                                    token: gem.address.clone(),
                                    symbol: gem.symbol.clone(),
                                    name: gem.name.clone(),
                                    price: gem.price,
                                    safety_score: gem.score,
                                    liquidity_usd: gem.liquidity_usd,
                                    market_cap: gem.market_cap,
                                    volume_24h: gem.volume_24h,
                                    change_1h: gem.change_1h,
                                    change_24h: gem.change_24h,
                                    image_url: gem.image_url.clone(),
                                    timestamp: chrono::Utc::now().timestamp(),
                                    source: "DISCOVERY".into(),
                                });
                            }
                        }
                    }
                }
                
                // Poi aggiorna lo state (lock breve)
                if !verified_gems.is_empty() {
                    let mut found = state.found_gems.lock().unwrap();
                    for gem in verified_gems {
                        if !found.iter().any(|g| g.token == gem.token) {
                            found.push(gem);
                        }
                    }
                    found.sort_by(|a, b| b.safety_score.cmp(&a.safety_score));
                    found.truncate(20);
                }
            },
            Err(e) => {
                warn!("⚠️ Errore gem discovery: {}", e);
            }
        }
        
        // Scansiona ogni 60 secondi
        sleep(Duration::from_secs(60)).await;
    }
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    if env::var("RUST_LOG").is_err() { env::set_var("RUST_LOG", "info"); }
    env_logger::init();
    info!("🚀 GOD SNIPER: Ultimate Safe Engine Avviato.");

    let _master = env::var("MASTER_KEY").expect("Manca KEY");
    let _rpc = env::var("RPC_URL").expect("Manca RPC");
    let _db = env::var("DATABASE_URL").expect("Manca DB");

    let pool = db::connect().await;
    let net = Arc::new(network::init_clients().await);

    let state = Arc::new(AppState { 
        found_gems: Mutex::new(Vec::new()), 
        math_signals: Mutex::new(Vec::new()),
        buy_cooldowns: Mutex::new(HashMap::new()), // Nuovo
        processed_sigs: Mutex::new(HashSet::new()), // Nuovo
    });

    let p1=pool.clone(); let n1=net.clone();
    tokio::spawn(async move { telegram_bot::start_bot(p1, n1).await; });

    let p2=pool.clone(); let n2=net.clone(); let s2=state.clone();
    tokio::spawn(async move { api::start_server(p2, n2, s2).await; });

    let p3=pool.clone(); let n3=net.clone(); let s3=state.clone();
    tokio::spawn(async move { run_market_strategy(n3, s3, p3).await; });

    let p4=pool.clone(); let n4=net.clone(); let s4=state.clone();
    tokio::spawn(async move { run_sniper_listener(n4, s4, p4).await; });

    // Gem Discovery Task - Scansiona il mercato per token promettenti
    let n5=net.clone(); let s5=state.clone();
    tokio::spawn(async move { run_gem_discovery(s5, n5).await; });

    // let p6=pool.clone(); let n6=net.clone();
    // tokio::spawn(async move { run_position_manager(p6, n6).await; }); // Attiva se hai il modulo completo

    match tokio::signal::ctrl_c().await {
        Ok(()) => info!("🛑 Chiusura sicura."),
        Err(_) => {}
    }
    pool.close().await;
}