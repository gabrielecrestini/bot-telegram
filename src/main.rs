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
pub mod engine;

// ═══════════════════════════════════════════════════════════════════════════════
// WATCHLIST - Token monitorati per segnali
// ═══════════════════════════════════════════════════════════════════════════════
const WATCHLIST: &[&str] = &[
    "So11111111111111111111111111111111111111112", 
    "JUPyiwrYJFskUPiHa7hkeR8VUtKCw785HvjeyzmEgGz",
    "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", 
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", 
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
    "HZ1JovNiVvGrGNiiYvv3XW5KKge5Wbtf2dqsfYfFq5pump", 
    "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", 
];

// ═══════════════════════════════════════════════════════════════════════════════
// DATA STRUCTURES
// ═══════════════════════════════════════════════════════════════════════════════

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

// ═══════════════════════════════════════════════════════════════════════════════
// APP STATE - Stato condiviso dell'applicazione
// ═══════════════════════════════════════════════════════════════════════════════

pub struct AppState {
    pub found_gems: Mutex<Vec<GemData>>,
    pub math_signals: Mutex<Vec<api::SignalData>>,
    pub buy_cooldowns: Mutex<HashMap<String, HashMap<String, i64>>>, 
    pub processed_sigs: Mutex<HashSet<String>>,
    // AMMS Engine
    pub market_data: Mutex<HashMap<String, strategy::MarketData>>,
    pub open_positions: Mutex<HashMap<String, Vec<engine::OpenPosition>>>, // user_id -> positions
    pub portfolio_stats: Mutex<HashMap<String, engine::PortfolioStats>>,   // user_id -> stats
    // Bot Users: user_id -> (amount, strategy)
    pub bot_active_users: Mutex<HashMap<String, (f64, String)>>,
}

// ═══════════════════════════════════════════════════════════════════════════════
// HELPER FUNCTIONS
// ═══════════════════════════════════════════════════════════════════════════════

fn check_and_set_cooldown(state: &Arc<AppState>, user_id: &str, token: &str) -> bool {
    let mut cache = state.buy_cooldowns.lock().unwrap();
    let user_cache = cache.entry(user_id.to_string()).or_insert_with(HashMap::new);
    let now = chrono::Utc::now().timestamp();

    if let Some(last_buy) = user_cache.get(token) {
        if now - last_buy < 600 { // 10 min cooldown
            return false;
        }
    }
    
    user_cache.insert(token.to_string(), now);
    true
}

fn is_new_signature(state: &Arc<AppState>, sig: &str) -> bool {
    let mut cache = state.processed_sigs.lock().unwrap();
    if cache.contains(sig) { return false; }
    cache.insert(sig.to_string());
    if cache.len() > 10000 { cache.clear(); }
    true
}

// ═══════════════════════════════════════════════════════════════════════════════
// AMMS AUTO-BUY - Buy con analisi AMMS completa
// ═══════════════════════════════════════════════════════════════════════════════

async fn execute_amms_auto_buy(
    pool: &sqlx::SqlitePool,
    net: &Arc<network::NetworkClient>,
    state: &Arc<AppState>,
    token_mint: &Pubkey,
    external_data: Option<&strategy::ExternalData>,
    trading_mode: strategy::TradingMode,
) {
    let users = sqlx::query("SELECT tg_id FROM users WHERE is_active = 1").fetch_all(pool).await;
    if let Ok(rows) = users {
        if rows.is_empty() { return; }
        
        let mint_str = token_mint.to_string();
        let mode_str = match trading_mode {
            strategy::TradingMode::Dip => "DIP",
            strategy::TradingMode::Breakout => "BREAKOUT",
            strategy::TradingMode::None => "AUTO",
        };
        info!("🤖 AMMS {} BUY: {} utenti per {}", mode_str, rows.len(), &mint_str[..8]);

        // Fetch Pool Keys UNA volta
        let pool_keys = match raydium::fetch_pool_keys_by_mint(net, token_mint).await {
            Ok(k) => Some(k),
            Err(_) => None,
        };

        for row in rows {
            let uid: String = row.get("tg_id");

            // 1. CHECK COOLDOWN
            if !check_and_set_cooldown(state, &uid, &mint_str) {
                continue;
            }

            let net_c = net.clone();
            let pool_c = pool.clone();
            let token_c = mint_str.clone();
            let keys_c = pool_keys.clone();
            let mint_key = *token_mint;
            let state_c = state.clone();
            let ext_data = external_data.cloned();
            let mode = trading_mode;

            tokio::spawn(async move {
                if let Ok(payer) = wallet_manager::get_decrypted_wallet(&pool_c, &uid).await {
                    let bal = net_c.get_balance_fast(&payer.pubkey()).await;
                    let bal_sol = bal as f64 / 1_000_000_000.0;
                    
                    if bal_sol < 0.02 { return; } // Riserva minima

                    // Calcola ATR se abbiamo market data
                    let atr_pct = if let Ok(mkt_data) = state_c.market_data.lock() {
                        mkt_data.get(&token_c)
                            .and_then(|d| strategy::analyze_market_full(d))
                            .map(|a| (a.atr / ext_data.as_ref().map(|e| e.price).unwrap_or(1.0)) * 100.0)
                    } else { None };

                    // BREAKOUT usa importo più cauto
                    let mut amt_sol = match mode {
                        strategy::TradingMode::Breakout => strategy::calculate_breakout_investment(bal_sol, atr_pct),
                        _ => strategy::calculate_investment_amount(bal_sol, atr_pct),
                    };
                    
                    // Tetto sicurezza auto-trade
                    amt_sol = amt_sol.min(0.5);
                    
                    let amt_lam = (amt_sol * 1_000_000_000.0) as u64;

                    if amt_lam > 0 {
                        let input = "So11111111111111111111111111111111111111112";
                        let mut success = false;
                        let mut entry_price = 0.0;
                        let mut tx_sig = String::new();

                        // JUPITER FIRST
                        if let Ok(mut tx) = jupiter::get_jupiter_swap_tx(
                            &payer.pubkey().to_string(), 
                            input, 
                            &token_c, 
                            amt_lam, 
                            100
                        ).await {
                            let bh = net_c.rpc.get_latest_blockhash().await.unwrap();
                            tx.sign(&[&payer], bh);
                            if let Ok(sig) = net_c.rpc.send_transaction(&tx).await {
                                let mode_str = match mode {
                                    strategy::TradingMode::Dip => "DIP",
                                    strategy::TradingMode::Breakout => "BREAKOUT",
                                    _ => "AUTO",
                                };
                                info!("✅ 🔵{} JUPITER ({}) -> TX: {}", mode_str, uid, sig);
                                let _ = db::record_buy_with_mode(&pool_c, &uid, &token_c, &sig.to_string(), amt_lam, mode_str).await;
                                success = true;
                                tx_sig = sig.to_string();
                                entry_price = ext_data.as_ref().map(|e| e.price).unwrap_or(0.0);
                            }
                        }

                        // RAYDIUM FALLBACK
                        if !success {
                            if let Some(keys) = keys_c {
                                if let Ok(sig) = raydium::execute_swap(&net_c, &payer, &keys, mint_key, amt_lam, 200).await {
                                    let mode_str = match mode {
                                        strategy::TradingMode::Dip => "DIP",
                                        strategy::TradingMode::Breakout => "BREAKOUT",
                                        _ => "AUTO",
                                    };
                                    info!("⚡ 🚀{} RAYDIUM ({}) -> TX: {}", mode_str, uid, sig);
                                    let _ = db::record_buy_with_mode(&pool_c, &uid, &token_c, &sig, amt_lam, mode_str).await;
                                    success = true;
                                    tx_sig = sig.clone();
                                    entry_price = ext_data.as_ref().map(|e| e.price).unwrap_or(0.0);
                                }
                            }
                        }

                        // Registra posizione per tracking AMMS
                        if success && entry_price > 0.0 {
                            let atr = if let Ok(mkt_data) = state_c.market_data.lock() {
                                mkt_data.get(&token_c)
                                    .and_then(|d| strategy::analyze_market_full(d))
                                    .map(|a| a.atr)
                                    .unwrap_or(entry_price * 0.03)
                            } else { entry_price * 0.03 };
                            
                            // Multipliers diversi per DIP e BREAKOUT
                            let (sl_mult, tp_mult) = match mode {
                                strategy::TradingMode::Breakout => (2.0, 3.0),
                                _ => (1.5, 2.0),
                            };
                            
                            if let Ok(mut positions) = state_c.open_positions.lock() {
                                let user_positions = positions.entry(uid.clone()).or_insert_with(Vec::new);
                                let pos = engine::OpenPosition::new(
                                    chrono::Utc::now().timestamp(),
                                    &token_c,
                                    entry_price,
                                    atr,
                                    amt_sol,
                                    amt_lam,
                                    mode,
                                );
                                user_positions.push(pos);
                                
                                let mode_str = match mode {
                                    strategy::TradingMode::Dip => "DIP",
                                    strategy::TradingMode::Breakout => "BREAKOUT",
                                    _ => "AUTO",
                                };
                                info!("📊 {} Posizione [{}] | SL: ${:.8} | TP: ${:.8}", 
                                    mode_str, uid, entry_price - (sl_mult * atr), entry_price + (tp_mult * atr));
                            }
                        }
                    }
                }
            });
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// POSITION MANAGER - Gestisce posizioni con trailing stop ATR
// ═══════════════════════════════════════════════════════════════════════════════

async fn run_position_manager(
    pool: sqlx::SqlitePool, 
    net: Arc<network::NetworkClient>,
    state: Arc<AppState>
) {
    info!("📊 AMMS Position Manager avviato");
    
    loop {
        // Raccogli posizioni da processare (senza tenere lock)
        let positions_snapshot: Vec<(String, Vec<engine::OpenPosition>)> = {
            if let Ok(positions) = state.open_positions.lock() {
                positions.iter()
                    .map(|(uid, pos)| (uid.clone(), pos.clone()))
                    .collect()
            } else { Vec::new() }
        };
        
        for (user_id, user_positions) in positions_snapshot {
            for pos in &user_positions {
                // Ottieni prezzo corrente
                let current_price = match jupiter::get_token_market_data(&pos.token_address).await {
                    Ok(mkt) => mkt.price,
                    Err(_) => continue,
                };
                
                // Calcola ATR corrente
                let current_atr = if let Ok(mkt_data) = state.market_data.lock() {
                    mkt_data.get(&pos.token_address)
                        .and_then(|d| strategy::analyze_market_full(d))
                        .map(|a| a.atr)
                        .unwrap_or(pos.entry_atr)
                } else { pos.entry_atr };
                
                // Clona posizione per update
                let mut pos_updated = pos.clone();
                let action = pos_updated.update(current_price, current_atr);
                
                match action {
                    engine::PositionAction::Sell { reason, pnl_pct } => {
                        info!("🔔 SELL SIGNAL [{}]: {} | {}", user_id, &pos.token_address[..8], reason);
                        
                        // Esegui vendita
                        if let Ok(payer) = wallet_manager::get_decrypted_wallet(&pool, &user_id).await {
                            if let Ok(mint) = Pubkey::from_str(&pos.token_address) {
                                // Tenta Jupiter sell
                                let output = "So11111111111111111111111111111111111111112";
                                let sell_amount = pos.amount_lamports;
                                
                                match jupiter::get_jupiter_swap_tx(
                                    &payer.pubkey().to_string(),
                                    &pos.token_address,
                                    output,
                                    sell_amount,
                                    200 // 2% slippage per sell
                                ).await {
                                    Ok(mut tx) => {
                                        let bh = net.rpc.get_latest_blockhash().await.unwrap();
                                        tx.sign(&[&payer], bh);
                                        if let Ok(sig) = net.rpc.send_transaction(&tx).await {
                                            info!("✅ AMMS SELL [{}] {} | PnL: {:+.1}% | TX: {}", 
                                                user_id, &pos.token_address[..8], pnl_pct, sig);
                                            
                                            // Registra nel DB
                                            let _ = db::record_sell(&pool, &user_id, &pos.token_address, &sig.to_string(), pnl_pct).await;
                                            
                                            // Update stats con modalità
                                            let hold_time = (chrono::Utc::now().timestamp() - pos.entry_time) as f64 / 60.0;
                                            let pnl_sol = pos.amount_sol * (pnl_pct / 100.0);
                                            
                                            if let Ok(mut stats) = state.portfolio_stats.lock() {
                                                let user_stats = stats.entry(user_id.clone()).or_default();
                                                user_stats.record_trade(pnl_pct, pnl_sol, hold_time, pos.mode);
                                            }
                                            
                                            // Rimuovi posizione
                                            if let Ok(mut positions) = state.open_positions.lock() {
                                                if let Some(user_pos) = positions.get_mut(&user_id) {
                                                    user_pos.retain(|p| p.id != pos.id);
                                                }
                                            }
                                            
                                            // REINVESTIMENTO AUTOMATICO
                                            let new_balance = net.get_balance_fast(&payer.pubkey()).await as f64 / 1_000_000_000.0;
                                            if new_balance > 0.02 {
                                                let stats_clone = if let Ok(stats) = state.portfolio_stats.lock() {
                                                    stats.get(&user_id).cloned().unwrap_or_default()
                                                } else { engine::PortfolioStats::default() };
                                                
                                                let (reinvest_amt, strategy_desc) = engine::calculate_reinvestment(
                                                    new_balance, pnl_pct, &stats_clone
                                                );
                                                
                                                info!("♻️ REINVEST [{}]: {} - {:.4} SOL", user_id, strategy_desc, reinvest_amt);
                                                // Il prossimo ciclo market_strategy gestirà il reinvestimento
                                            }
                                        }
                                    },
                                    Err(e) => warn!("❌ Sell fallito per {}: {}", user_id, e),
                                }
                            }
                        }
                    },
                    engine::PositionAction::Hold => {
                        // Aggiorna posizione in state
                        if let Ok(mut positions) = state.open_positions.lock() {
                            if let Some(user_pos) = positions.get_mut(&user_id) {
                                if let Some(p) = user_pos.iter_mut().find(|p| p.id == pos.id) {
                                    *p = pos_updated;
                                }
                            }
                        }
                    }
                }
            }
        }
        
        sleep(Duration::from_secs(5)).await;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MARKET STRATEGY - Analisi AMMS per segnali di entrata
// ═══════════════════════════════════════════════════════════════════════════════

async fn run_market_strategy(net: Arc<network::NetworkClient>, state: Arc<AppState>, pool: sqlx::SqlitePool) {
    info!("📊 AMMS Market Strategy avviato");
    
    loop {
        // Include anche gemme trovate nella watchlist
        let tokens_to_check: Vec<String> = {
            let mut tokens: Vec<String> = WATCHLIST.iter().map(|s| s.to_string()).collect();
            
            if let Ok(gems) = state.found_gems.lock() {
                for gem in gems.iter().take(10) { // Top 10 gems
                    if !tokens.contains(&gem.token) {
                        tokens.push(gem.token.clone());
                    }
                }
            }
            tokens
        };
        
        for token in &tokens_to_check {
            // Fetch dati mercato
            if let Ok(mkt) = jupiter::get_token_market_data(token).await {
                // FILTRO QUALITÀ
                if mkt.liquidity_usd < 10000.0 || mkt.volume_24h < 50000.0 { continue; }

                // Aggiorna market data
                {
                    let mut data_map = state.market_data.lock().unwrap();
                    let data = data_map.entry(token.clone())
                        .or_insert_with(|| strategy::MarketData::new(&mkt.symbol));
                    data.add_tick(mkt.price, mkt.volume_24h);
                }
                
                // Analisi AMMS
                let analysis = {
                    let data_map = state.market_data.lock().unwrap();
                    data_map.get(token).and_then(|d| strategy::analyze_market_full(d))
                };
                
                if let Some(analysis) = analysis {
                    let external = strategy::ExternalData {
                        price: mkt.price,
                        change_5m: mkt.change_1h / 12.0, // Stima 5m da 1h
                        change_1h: mkt.change_1h,
                        change_24h: mkt.change_24h,
                        volume_24h: mkt.volume_24h,
                        liquidity_usd: mkt.liquidity_usd,
                        market_cap: mkt.market_cap,
                    };
                    
                    let (should_buy, reason) = strategy::check_entry_conditions(&analysis, &external);
                    
                    // Aggiungi sempre alla lista gems se passa i filtri di base
                    if mkt.score >= 50 {
                        let gem = GemData {
                            token: token.clone(),
                            symbol: mkt.symbol.clone(),
                            name: mkt.name.clone(),
                            price: mkt.price,
                            safety_score: if should_buy { 90 } else { mkt.score },
                            liquidity_usd: mkt.liquidity_usd,
                            market_cap: mkt.market_cap,
                            volume_24h: mkt.volume_24h,
                            change_1h: mkt.change_1h,
                            change_24h: mkt.change_24h,
                            image_url: mkt.image_url.clone(),
                            timestamp: chrono::Utc::now().timestamp(),
                            source: if should_buy { "SIGNAL".into() } else { "WATCH".into() },
                        };
                        
                        if let Ok(mut gems) = state.found_gems.lock() {
                            // Aggiorna o aggiungi
                            if let Some(existing) = gems.iter_mut().find(|g| g.token == *token) {
                                existing.price = gem.price;
                                existing.change_1h = gem.change_1h;
                                existing.change_24h = gem.change_24h;
                                existing.safety_score = gem.safety_score;
                                existing.timestamp = gem.timestamp;
                            } else if gems.len() < 30 {
                                gems.push(gem);
                            }
                        }
                    }
                    
                    if should_buy {
                        let mode_str = match analysis.mode {
                            strategy::TradingMode::Dip => "🔵 DIP",
                            strategy::TradingMode::Breakout => "🚀 BREAKOUT",
                            strategy::TradingMode::None => "SIGNAL",
                        };
                        info!("📈 AMMS {} SEGNALE: {} - {}", mode_str, mkt.symbol, reason);
                        
                        // Salva segnale
                        if let Ok(mut signals) = state.math_signals.lock() {
                            let already_exists = signals.iter()
                                .any(|x| x.token == *token && (chrono::Utc::now().timestamp() - x.timestamp) < 300);
                            
                            if !already_exists {
                                signals.insert(0, api::SignalData { 
                                    token: token.clone(), 
                                    price: mkt.price, 
                                    score: 90, 
                                    reason: reason.clone(), 
                                    timestamp: chrono::Utc::now().timestamp() 
                                });
                                if signals.len() > 20 { signals.pop(); }
                            }
                        }
                        
                        // Auto-Buy con modalità corretta
                        if let Ok(pk) = Pubkey::from_str(token) {
                            let p = pool.clone();
                            let n = net.clone();
                            let s = state.clone();
                            let ext = external.clone();
                            let mode = analysis.mode;
                            
                            tokio::spawn(async move {
                                execute_amms_auto_buy(&p, &n, &s, &pk, Some(&ext), mode).await;
                            });
                        }
                    }
                }
            }
            
            sleep(Duration::from_millis(300)).await;
        }
        
        // Pulizia cache periodica
        {
            let mut data_map = state.market_data.lock().unwrap();
            if data_map.len() > 100 {
                let tokens_to_remove: Vec<String> = data_map.keys()
                    .filter(|k| !tokens_to_check.contains(k))
                    .take(50)
                    .cloned()
                    .collect();
                for k in tokens_to_remove { data_map.remove(&k); }
            }
        }
        
        sleep(Duration::from_secs(20)).await;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SNIPER LISTENER - Snipa nuovi token con verifiche di sicurezza
// ═══════════════════════════════════════════════════════════════════════════════

async fn run_sniper_listener(net: Arc<network::NetworkClient>, state: Arc<AppState>, pool: sqlx::SqlitePool) {
    let raydium_id = Pubkey::from_str(crate::raydium::RAYDIUM_V4_PROGRAM_ID).unwrap();
    let ws_client = net.clone();

    loop {
        match ws_client.pubsub.logs_subscribe(
             RpcTransactionLogsFilter::Mentions(vec![raydium_id.to_string()]),
             RpcTransactionLogsConfig { commitment: Some(CommitmentConfig::processed()) }
        ).await {
            Ok((mut stream, _)) => {
                info!("✅ AMMS Sniper Attivo.");
                while let Some(log) = stream.next().await {
                    if log.value.logs.iter().any(|l| l.contains("initialize2")) {
                        let sig_str = log.value.signature;
                        
                        if !is_new_signature(&state, &sig_str) { continue; }

                        let n_an = net.clone();
                        let s_an = state.clone();
                        let p_an = pool.clone();
                        
                        tokio::spawn(async move {
                            if let Ok(sig) = solana_sdk::signature::Signature::from_str(&sig_str) {
                                if let Ok(tx) = n_an.rpc.get_transaction_with_config(
                                    &sig, 
                                    RpcTransactionConfig { 
                                        encoding: Some(UiTransactionEncoding::Json), 
                                        commitment: Some(CommitmentConfig::confirmed()), 
                                        max_supported_transaction_version: Some(0) 
                                    }
                                ).await {
                                    if let Some(meta) = tx.transaction.meta {
                                        let balances = match meta.post_token_balances { 
                                            OptionSerializer::Some(b) => b, 
                                            _ => vec![] 
                                        };
                                        let wsol = "So11111111111111111111111111111111111111112";
                                        
                                        for b in balances {
                                            let mint = b.mint;
                                            if mint != wsol && b.ui_token_amount.decimals > 0 {
                                                if let Ok(pk) = Pubkey::from_str(&mint) {
                                                    // Safety check
                                                    if let Ok(rep) = safety::check_token_safety(&n_an, &pk).await {
                                                        if rep.is_safe {
                                                            sleep(Duration::from_secs(2)).await;
                                                            if let Ok(mkt) = jupiter::get_token_market_data(&mint).await {
                                                                // Filtro qualità
                                                                if mkt.liquidity_usd > 5000.0 && mkt.price > 0.0 {
                                                                    info!("💎 SNIPER HIT: {} (${:.8}) Liq: ${:.0}", 
                                                                        mkt.symbol, mkt.price, mkt.liquidity_usd);
                                                                    
                                                                    // Aggiungi a gems
                                                                    {
                                                                        let mut g = s_an.found_gems.lock().unwrap();
                                                                        g.insert(0, GemData { 
                                                                            token: mint.clone(), 
                                                                            symbol: mkt.symbol.clone(), 
                                                                            name: mkt.name.clone(),
                                                                            price: mkt.price, 
                                                                            safety_score: mkt.score, 
                                                                            liquidity_usd: mkt.liquidity_usd,
                                                                            market_cap: mkt.market_cap,
                                                                            volume_24h: mkt.volume_24h,
                                                                            change_1h: mkt.change_1h,
                                                                            change_24h: mkt.change_24h,
                                                                            image_url: mkt.image_url.clone(),
                                                                            timestamp: chrono::Utc::now().timestamp(), 
                                                                            source: "SNIPER".into() 
                                                                        });
                                                                        g.sort_by(|a, b| b.safety_score.cmp(&a.safety_score));
                                                                        if g.len() > 30 { g.pop(); }
                                                                    }
                                                                    
                                                                    // Auto buy con dati esterni - Sniper = nuovi token = BREAKOUT mode
                                                                    let ext = strategy::ExternalData {
                                                                        price: mkt.price,
                                                                        change_5m: 5.0, // Nuovo = pump
                                                                        change_1h: mkt.change_1h,
                                                                        change_24h: mkt.change_24h,
                                                                        volume_24h: mkt.volume_24h,
                                                                        liquidity_usd: mkt.liquidity_usd,
                                                                        market_cap: mkt.market_cap,
                                                                    };
                                                                    
                                                                    // Nuovi token = BREAKOUT (cavalcano l'onda del lancio)
                                                                    execute_amms_auto_buy(&p_an, &n_an, &s_an, &pk, Some(&ext), strategy::TradingMode::Breakout).await;
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
            Err(e) => {
                warn!("⚠️ Sniper WS error: {}", e);
                sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// GEM DISCOVERY - Trova token promettenti
// ═══════════════════════════════════════════════════════════════════════════════

async fn run_gem_discovery(state: Arc<AppState>, net: Arc<network::NetworkClient>) {
    info!("💎 AMMS Gem Discovery avviato");
    
    // FALLBACK: Monete principali sempre disponibili
    let default_gems = vec![
        GemData {
            token: "JUPyiwrYJFskUPiHa7hkeR8VUtKCw785HvjeyzmEgGz".to_string(),
            symbol: "JUP".to_string(),
            name: "Jupiter".to_string(),
            price: 0.0,
            safety_score: 95,
            liquidity_usd: 50_000_000.0,
            market_cap: 1_500_000_000.0,
            volume_24h: 100_000_000.0,
            change_1h: 0.0,
            change_24h: 0.0,
            image_url: "https://static.jup.ag/jup/icon.png".to_string(),
            timestamp: chrono::Utc::now().timestamp(),
            source: "TOP".to_string(),
        },
        GemData {
            token: "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263".to_string(),
            symbol: "BONK".to_string(),
            name: "Bonk".to_string(),
            price: 0.0,
            safety_score: 90,
            liquidity_usd: 20_000_000.0,
            market_cap: 2_000_000_000.0,
            volume_24h: 50_000_000.0,
            change_1h: 0.0,
            change_24h: 0.0,
            image_url: "https://arweave.net/hQiPZOsRZXGXBJd_82PhVdlM_hACsT_q6wqwf5cSY7I".to_string(),
            timestamp: chrono::Utc::now().timestamp(),
            source: "TOP".to_string(),
        },
        GemData {
            token: "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm".to_string(),
            symbol: "WIF".to_string(),
            name: "dogwifhat".to_string(),
            price: 0.0,
            safety_score: 88,
            liquidity_usd: 30_000_000.0,
            market_cap: 3_000_000_000.0,
            volume_24h: 80_000_000.0,
            change_1h: 0.0,
            change_24h: 0.0,
            image_url: "https://bafkreibk3covs5ltyqxa272uodhculbr6kea6betidfwy3ajsav2vjzyum.ipfs.nftstorage.link".to_string(),
            timestamp: chrono::Utc::now().timestamp(),
            source: "TOP".to_string(),
        },
    ];
    
    // Inizializza con le monete di default e aggiorna i prezzi
    {
        let mut found = state.found_gems.lock().unwrap();
        if found.is_empty() {
            *found = default_gems.clone();
            info!("📊 Caricate {} monete di fallback", found.len());
        }
    }
    
    let mut cycle = 0u32;
    
    loop {
        // ═══════════════════════════════════════════════════════════════
        // AGGIORNA PREZZI LIVE di TUTTE le gems esistenti
        // ═══════════════════════════════════════════════════════════════
        {
            let tokens_to_update: Vec<(String, String)> = {
                let found = state.found_gems.lock().unwrap();
                found.iter().map(|g| (g.token.clone(), g.symbol.clone())).collect()
            };
            
            for (token, _symbol) in &tokens_to_update {
                match jupiter::get_token_market_data(token).await {
                    Ok(mkt) => {
                        let mut found = state.found_gems.lock().unwrap();
                        if let Some(g) = found.iter_mut().find(|g| &g.token == token) {
                            g.price = mkt.price;
                            g.change_1h = mkt.change_1h;
                            g.change_24h = mkt.change_24h;
                            g.volume_24h = mkt.volume_24h;
                            g.market_cap = mkt.market_cap;
                            g.liquidity_usd = mkt.liquidity_usd;
                            g.image_url = if mkt.image_url.is_empty() { g.image_url.clone() } else { mkt.image_url };
                            g.timestamp = chrono::Utc::now().timestamp();
                        }
                    }
                    Err(_) => {
                        // Se fallisce, mantieni i dati esistenti
                    }
                }
                // Piccola pausa per non sovraccaricare le API
                sleep(Duration::from_millis(100)).await;
            }
            
            if !tokens_to_update.is_empty() {
                info!("💰 Prezzi aggiornati per {} token", tokens_to_update.len());
            }
        }
        
        let mut all_gems: Vec<GemData> = Vec::new();
        
        // PARTE 1: ALTCOIN AFFERMATE
        match jupiter::find_profitable_altcoins().await {
            Ok(altcoins) => {
                debug!("📊 {} altcoin affermate trovate", altcoins.len());
                for coin in altcoins {
                    all_gems.push(GemData {
                        token: coin.address.clone(),
                        symbol: coin.symbol.clone(),
                        name: coin.name.clone(),
                        price: coin.price,
                        safety_score: coin.score.max(70),
                        liquidity_usd: coin.liquidity_usd,
                        market_cap: coin.market_cap,
                        volume_24h: coin.volume_24h,
                        change_1h: coin.change_1h,
                        change_24h: coin.change_24h,
                        image_url: coin.image_url.clone(),
                        timestamp: chrono::Utc::now().timestamp(),
                        source: "ALTCOIN".into(),
                    });
                }
            },
            Err(e) => debug!("⚠️ Altcoin fetch: {}", e),
        }
        
        // PARTE 2: NUOVE GEMME (ogni 3 cicli)
        if cycle % 3 == 0 {
            match jupiter::discover_trending_gems().await {
                Ok(gems) => {
                    debug!("💎 {} nuove gemme trovate", gems.len());
                    
                    for gem in gems {
                        if let Ok(pk) = Pubkey::from_str(&gem.address) {
                            if let Ok(report) = safety::check_token_safety(&net, &pk).await {
                                if report.is_safe && gem.score >= 45 {
                                    if !all_gems.iter().any(|g| g.token == gem.address) {
                                        all_gems.push(GemData {
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
                    }
                },
                Err(e) => debug!("⚠️ Gem discovery: {}", e),
            }
        }
        
        // AGGIORNA STATO
        {
            let mut found = state.found_gems.lock().unwrap();
            
            if !all_gems.is_empty() {
                // Ordina: prima le altcoin affermate, poi per score
                all_gems.sort_by(|a, b| {
                    let a_priority = if a.market_cap > 10_000_000.0 { 1 } else { 0 };
                    let b_priority = if b.market_cap > 10_000_000.0 { 1 } else { 0 };
                    
                    match b_priority.cmp(&a_priority) {
                        std::cmp::Ordering::Equal => b.safety_score.cmp(&a.safety_score),
                        other => other
                    }
                });
                
                all_gems.truncate(25);
                *found = all_gems;
                info!("💎 Aggiornate {} gems", found.len());
            } else if found.is_empty() {
                // Se le API falliscono e non ci sono gems, ripristina fallback
                *found = default_gems.clone();
                warn!("⚠️ API non raggiungibili, usando {} gems di fallback", found.len());
            }
            // Se ci sono già gems ma le API falliscono, mantieni quelle esistenti
        }
        
        cycle = cycle.wrapping_add(1);
        sleep(Duration::from_secs(45)).await;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MAIN - Entry point
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() {
    dotenv().ok();
    if env::var("RUST_LOG").is_err() { env::set_var("RUST_LOG", "info"); }
    env_logger::init();
    
    info!("═══════════════════════════════════════════════════════════════");
    info!("  🚀 GOD SNIPER - AMMS (Adaptive Multi-Phase Momentum Strategy)");
    info!("═══════════════════════════════════════════════════════════════");

    let _master = env::var("MASTER_KEY").expect("Manca MASTER_KEY");
    let _rpc = env::var("RPC_URL").expect("Manca RPC_URL");
    let _db = env::var("DATABASE_URL").expect("Manca DATABASE_URL");

    let pool = db::connect().await;
    let net = Arc::new(network::init_clients().await);

    let state = Arc::new(AppState { 
        found_gems: Mutex::new(Vec::new()), 
        math_signals: Mutex::new(Vec::new()),
        buy_cooldowns: Mutex::new(HashMap::new()),
        processed_sigs: Mutex::new(HashSet::new()),
        market_data: Mutex::new(HashMap::new()),
        open_positions: Mutex::new(HashMap::new()),
        portfolio_stats: Mutex::new(HashMap::new()),
        bot_active_users: Mutex::new(HashMap::new()),
    });

    // Telegram Bot
    let p1 = pool.clone();
    let n1 = net.clone();
    tokio::spawn(async move { telegram_bot::start_bot(p1, n1).await; });

    // API Server
    let p2 = pool.clone();
    let n2 = net.clone();
    let s2 = state.clone();
    tokio::spawn(async move { api::start_server(p2, n2, s2).await; });

    // AMMS Market Strategy
    let p3 = pool.clone();
    let n3 = net.clone();
    let s3 = state.clone();
    tokio::spawn(async move { run_market_strategy(n3, s3, p3).await; });

    // AMMS Position Manager (trailing stop)
    let p4 = pool.clone();
    let n4 = net.clone();
    let s4 = state.clone();
    tokio::spawn(async move { run_position_manager(p4, n4, s4).await; });

    // Sniper Listener
    let p5 = pool.clone();
    let n5 = net.clone();
    let s5 = state.clone();
    tokio::spawn(async move { run_sniper_listener(n5, s5, p5).await; });

    // Gem Discovery
    let n6 = net.clone();
    let s6 = state.clone();
    tokio::spawn(async move { run_gem_discovery(s6, n6).await; });

    info!("✅ Tutti i moduli AMMS avviati");
    info!("   • Market Strategy: EMA, RSI, ATR, Bollinger");
    info!("   • Position Manager: Trailing Stop ATR-based");
    info!("   • Auto-Reinvestment: Wealth-adaptive");
    info!("   • Sniper: Anti-rug, Anti-honeypot");

    match tokio::signal::ctrl_c().await {
        Ok(()) => info!("🛑 Chiusura sicura..."),
        Err(_) => {}
    }
    pool.close().await;
}
