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
use spl_associated_token_account;
use teloxide::Bot;

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
// WATCHLIST - Token monitorati per segnali (NO stablecoins, NO SOL)
// ═══════════════════════════════════════════════════════════════════════════════
const WATCHLIST: &[&str] = &[
    "JUPyiwrYJFskUPiHa7hkeR8VUtKCw785HvjeyzmEgGz",  // JUP
    "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", // WIF
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", // BONK
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr", // POPCAT
    "rndrizKT3MK1iimdxRdWabcF7Zg7AR5T4nud4EkHBof",  // RENDER
    "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", // jitoSOL
];

// Token da ESCLUDERE sempre (SOL, stablecoins)
const EXCLUDED_TOKENS: &[&str] = &[
    "So11111111111111111111111111111111111111112",  // SOL - non puoi comprare SOL con SOL
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", // USDT
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // USDC
    "USDH1SM1ojwWUga67PGrgFWUHibbjqMvuMaDkRJTgkX",  // USDH
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
    let mint_str = token_mint.to_string();
    
    // Verifica che non sia un token escluso
    if EXCLUDED_TOKENS.contains(&mint_str.as_str()) {
        debug!("⏭️ Token escluso: {}", &mint_str[..8]);
        return;
    }
    
    // Ottieni utenti attivi (bot_active O manualmente attivato)
    let users = sqlx::query("SELECT tg_id FROM users WHERE is_active = 1").fetch_all(pool).await;
    if let Ok(rows) = users {
        if rows.is_empty() { 
            debug!("⏭️ Nessun utente attivo per auto-buy");
            return; 
        }
        
        let mode_str = match trading_mode {
            strategy::TradingMode::Dip => "DIP",
            strategy::TradingMode::Breakout => "BREAKOUT",
            strategy::TradingMode::None => "AUTO",
        };
        info!("🤖 AMMS {} BUY: {} utenti per {}", mode_str, rows.len(), &mint_str[..8]);

        // Fetch Pool Keys UNA volta (per Raydium fallback)
        let pool_keys = match raydium::fetch_pool_keys_by_mint(net, token_mint).await {
            Ok(k) => Some(k),
            Err(e) => {
                debug!("⚠️ Pool keys non disponibili (Jupiter only): {}", e);
                None
            }
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
                match wallet_manager::get_decrypted_wallet(&pool_c, &uid).await {
                    Ok(payer) => {
                        let bal = net_c.get_balance_fast(&payer.pubkey()).await;
                        let bal_sol = bal as f64 / 1_000_000_000.0;
                        
                        // Verifica saldo minimo per trading (0.01 SOL = ~$2)
                        if bal_sol < 0.01 { 
                            debug!("⏭️ {} saldo insufficiente: {:.4} SOL", uid, bal_sol);
                            return; 
                        }

                        // Calcola ATR se abbiamo market data
                        let atr_pct = if let Ok(mkt_data) = state_c.market_data.lock() {
                            mkt_data.get(&token_c)
                                .and_then(|d| strategy::analyze_market_full(d))
                                .map(|a| (a.atr / ext_data.as_ref().map(|e| e.price).unwrap_or(1.0)) * 100.0)
                        } else { None };

                        // Calcola importo da investire basato sulla strategia
                        let mut amt_sol = match mode {
                            strategy::TradingMode::Breakout => strategy::calculate_breakout_investment(bal_sol, atr_pct),
                            _ => strategy::calculate_investment_amount(bal_sol, atr_pct),
                        };
                        
                        // Assicurati che l'importo sia valido e non superi il 95% del saldo
                        amt_sol = amt_sol.min(bal_sol * 0.95).min(0.5); // Max 0.5 SOL per auto-trade
                        
                        // Minimo 0.005 SOL per trade
                        if amt_sol < 0.005 {
                            debug!("⏭️ {} importo troppo basso: {:.6} SOL", uid, amt_sol);
                            return;
                        }
                        
                        let amt_lam = (amt_sol * 1_000_000_000.0) as u64;
                        let input = "So11111111111111111111111111111111111111112";
                        let mut success = false;
                        let mut entry_price = 0.0;
                        let mut tx_sig = String::new();
                        
                        info!("🛒 BUY {} | User: {} | Amount: {:.4} SOL", &token_c[..8], uid, amt_sol);

                        // JUPITER FIRST (con VersionedTransaction) - slippage 1.5%
                        match jupiter::get_jupiter_swap_tx(
                            &payer.pubkey().to_string(), 
                            input, 
                            &token_c, 
                            amt_lam, 
                            150  // 1.5% slippage
                        ).await {
                            Ok(tx) => {
                                match net_c.rpc.get_latest_blockhash().await {
                                    Ok(bh) => {
                                        match jupiter::sign_versioned_transaction(&tx, &payer, bh) {
                                            Ok(signed_tx) => {
                                                match net_c.send_versioned_transaction(&signed_tx).await {
                                                    Ok(sig) => {
                                                        let mode_str = match mode {
                                                            strategy::TradingMode::Dip => "DIP",
                                                            strategy::TradingMode::Breakout => "BREAKOUT",
                                                            _ => "AUTO",
                                                        };
                                                        info!("✅ {} JUPITER ({}) | {:.4} SOL | TX: {}", mode_str, uid, amt_sol, sig);
                                                        let _ = db::record_buy_with_mode(&pool_c, &uid, &token_c, &sig, amt_lam, mode_str).await;
                                                        success = true;
                                                        tx_sig = sig;
                                                        entry_price = ext_data.as_ref().map(|e| e.price).unwrap_or(0.0);
                                                    },
                                                    Err(e) => warn!("❌ Jupiter TX send fallita: {}", e),
                                                }
                                            },
                                            Err(e) => warn!("❌ Jupiter TX sign fallita: {}", e),
                                        }
                                    },
                                    Err(e) => warn!("❌ Blockhash fallito: {}", e),
                                }
                            },
                            Err(e) => debug!("⚠️ Jupiter quote fallita, provo Raydium: {}", e),
                        }

                        // RAYDIUM FALLBACK - solo se Jupiter ha fallito
                        if !success {
                            if let Some(keys) = keys_c {
                                match raydium::execute_swap(&net_c, &payer, &keys, mint_key, amt_lam, 200).await {
                                    Ok(sig) => {
                                        let mode_str = match mode {
                                            strategy::TradingMode::Dip => "DIP",
                                            strategy::TradingMode::Breakout => "BREAKOUT",
                                            _ => "AUTO",
                                        };
                                        info!("✅ {} RAYDIUM ({}) | {:.4} SOL | TX: {}", mode_str, uid, amt_sol, sig);
                                        let _ = db::record_buy_with_mode(&pool_c, &uid, &token_c, &sig, amt_lam, mode_str).await;
                                        success = true;
                                        tx_sig = sig.clone();
                                        entry_price = ext_data.as_ref().map(|e| e.price).unwrap_or(0.0);
                                    },
                                    Err(e) => warn!("❌ Raydium swap fallito: {}", e),
                                }
                            } else {
                                debug!("⚠️ Raydium non disponibile per questo token");
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
                        } else if !success {
                            warn!("❌ Trade fallito per {} su {}", uid, &token_c[..8]);
                        }
                    },
                    Err(e) => {
                        warn!("❌ Wallet error per {}: {}", uid, e);
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
                                // Ottieni il bilancio REALE del token nel wallet
                                let ata = spl_associated_token_account::get_associated_token_address(&payer.pubkey(), &mint);
                                let token_balance = match net.rpc.get_token_account_balance(&ata).await {
                                    Ok(balance) => balance.amount.parse::<u64>().unwrap_or(0),
                                    Err(_) => {
                                        warn!("⚠️ Nessun token trovato per {}", &pos.token_address[..8]);
                                        continue;
                                    }
                                };
                                
                                if token_balance == 0 {
                                    warn!("⚠️ Bilancio token = 0 per {}", &pos.token_address[..8]);
                                    continue;
                                }
                                
                                // Tenta Jupiter sell con quantità REALE di token
                                let output = "So11111111111111111111111111111111111111112";
                                info!("💰 Auto-sell {} token (balance: {})", &pos.token_address[..8], token_balance);
                                
                                match jupiter::get_jupiter_swap_tx(
                                    &payer.pubkey().to_string(),
                                    &pos.token_address,
                                    output,
                                    token_balance, // USA IL BILANCIO REALE!
                                    200 // 2% slippage per sell
                                ).await {
                                    Ok(tx) => {
                                        if let Ok(bh) = net.rpc.get_latest_blockhash().await {
                                            if let Ok(signed_tx) = jupiter::sign_versioned_transaction(&tx, &payer, bh) {
                                                if let Ok(sig) = net.send_versioned_transaction(&signed_tx).await {
                                                    info!("✅ AMMS SELL [{}] {} | PnL: {:+.1}% | TX: {}", 
                                                        user_id, &pos.token_address[..8], pnl_pct, sig);
                                            
                                                    // Registra nel DB
                                                    let _ = db::record_sell(&pool, &user_id, &pos.token_address, &sig, pnl_pct).await;
                                            
                                                    // Update stats con modalità
                                                    let hold_time = (chrono::Utc::now().timestamp() - pos.entry_time) as f64 / 60.0;
                                                    let pnl_sol = pos.amount_sol * (pnl_pct / 100.0);
                                            
                                                    if let Ok(mut stats) = state.portfolio_stats.lock() {
                                                        let user_stats = stats.entry(user_id.clone()).or_default();
                                                        user_stats.record_trade(pnl_pct, pnl_sol, hold_time, pos.mode);
                                                    }
                                                    
                                                    // 📱 NOTIFICA TELEGRAM
                                                    if let Ok(bot_token) = std::env::var("TELEGRAM_BOT_TOKEN") {
                                                        let bot = Bot::new(&bot_token);
                                                        let symbol = pos.token_address[..8].to_string();
                                                        telegram_bot::notify_sell(&bot, &user_id, &symbol, pnl_pct, pnl_sol, &sig).await;
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
// DAILY REPORT - Invia report serale a tutti gli utenti attivi
// ═══════════════════════════════════════════════════════════════════════════════

async fn run_daily_report_task(
    pool: sqlx::SqlitePool,
    net: Arc<network::NetworkClient>,
    bot_token: String,
) {
    if bot_token.is_empty() {
        warn!("⚠️ TELEGRAM_BOT_TOKEN non configurato, report disabilitati");
        return;
    }
    
    let bot = teloxide::Bot::new(&bot_token);
    info!("📅 Daily Report Task avviato (21:00 ogni sera)");
    
    loop {
        // Calcola tempo fino alle 21:00
        let now = chrono::Utc::now();
        let target_hour = 21; // 21:00 UTC (22:00 ora italiana)
        
        let mut next_report = now.date_naive().and_hms_opt(target_hour, 0, 0).unwrap();
        if now.time() >= chrono::NaiveTime::from_hms_opt(target_hour, 0, 0).unwrap() {
            // Se è già passato, vai a domani
            next_report = next_report + chrono::Duration::days(1);
        }
        
        let wait_secs = (next_report - now.naive_utc()).num_seconds().max(0) as u64;
        info!("📅 Prossimo report tra {} ore", wait_secs / 3600);
        
        sleep(Duration::from_secs(wait_secs)).await;
        
        info!("🌙 Invio report giornalieri...");
        
        // Ottieni tutti gli utenti Telegram attivi
        let users: Vec<String> = sqlx::query_scalar("SELECT tg_id FROM users WHERE tg_id LIKE 'tg_%'")
            .fetch_all(&pool)
            .await
            .unwrap_or_default();
        
        let sol_price = api::get_sol_price().await;
        
        for user_id in users {
            // Ottieni bilancio
            if let Ok(pubkey_str) = wallet_manager::create_user_wallet(&pool, &user_id).await {
                if let Ok(pubkey) = Pubkey::from_str(&pubkey_str) {
                    let balance = net.get_balance_fast(&pubkey).await as f64 / 1_000_000_000.0;
                    
                    // Invia report solo se ha un bilancio > 0 o trade attivi
                    let has_trades = db::count_open_trades(&pool, &user_id).await.unwrap_or(0) > 0;
                    if balance > 0.001 || has_trades {
                        telegram_bot::send_daily_report(&bot, &pool, &user_id, balance, sol_price).await;
                        sleep(Duration::from_millis(100)).await; // Rate limit
                    }
                }
            }
        }
        
        info!("✅ Report giornalieri inviati");
        
        // Aspetta almeno 1 ora prima di ricalcolare (evita loop)
        sleep(Duration::from_secs(3600)).await;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// GEM DISCOVERY - Trova token promettenti
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifica se un token deve essere escluso
fn is_excluded_token(address: &str, symbol: &str) -> bool {
    // Token esclusi per indirizzo
    if EXCLUDED_TOKENS.contains(&address) {
        return true;
    }
    // Stablecoins escluse per simbolo
    let stable_symbols = ["USDC", "USDT", "USDH", "DAI", "USDD", "BUSD", "TUSD"];
    if stable_symbols.contains(&symbol.to_uppercase().as_str()) {
        return true;
    }
    false
}

/// Ottieni URL immagine con fallback DexScreener
fn get_image_url(original: &str, token_address: &str) -> String {
    if original.is_empty() || original.contains("undefined") || original.contains("null") {
        format!("https://dd.dexscreener.com/ds-data/tokens/solana/{}.png", token_address)
    } else {
        original.to_string()
    }
}

async fn run_gem_discovery(state: Arc<AppState>, net: Arc<network::NetworkClient>) {
    info!("💎 AMMS Gem Discovery avviato");
    
    // TOP ALTCOIN - Solo indirizzi, dati REALI dalle API (NO SOL, NO stablecoins)
    let top_token_addresses = vec![
        ("JUPyiwrYJFskUPiHa7hkeR8VUtKCw785HvjeyzmEgGz", "JUP"),
        ("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", "BONK"),
        ("EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", "WIF"),
        ("7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr", "POPCAT"),
        ("rndrizKT3MK1iimdxRdWabcF7Zg7AR5T4nud4EkHBof", "RENDER"),
        ("4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R", "RAY"),
        ("orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE", "ORCA"),
    ];
    
    // Carica dati LIVE per le top altcoin all'avvio
    info!("📊 Caricamento dati LIVE per {} top altcoin...", top_token_addresses.len());
    
    for (token_addr, fallback_symbol) in &top_token_addresses {
        // Salta token esclusi
        if is_excluded_token(token_addr, fallback_symbol) {
            continue;
        }
        
        match jupiter::get_token_market_data(token_addr).await {
            Ok(mkt) => {
                if mkt.price > 0.0 && mkt.liquidity_usd > 5000.0 {
                    let gem = GemData {
                        token: mkt.address.clone(),
                        symbol: if mkt.symbol == "UNK" { fallback_symbol.to_string() } else { mkt.symbol.clone() },
                        name: mkt.name.clone(),
                        price: mkt.price,
                        safety_score: mkt.score.max(85), // Top altcoin hanno score alto
                        liquidity_usd: mkt.liquidity_usd,
                        market_cap: mkt.market_cap,
                        volume_24h: mkt.volume_24h,
                        change_1h: mkt.change_1h,
                        change_24h: mkt.change_24h,
                        image_url: get_image_url(&mkt.image_url, token_addr),
                        timestamp: chrono::Utc::now().timestamp(),
                        source: "TOP".to_string(),
                    };
                    
                    let mut found = state.found_gems.lock().unwrap();
                    if !found.iter().any(|g| g.token == gem.token) {
                        info!("✅ {} | ${:.6} | MCap: ${:.0}M | Vol: ${:.0}K", 
                            gem.symbol, mkt.price, mkt.market_cap / 1_000_000.0, mkt.volume_24h / 1_000.0);
                        found.push(gem);
                    }
                }
            }
            Err(e) => {
                warn!("⚠️ API error per {}: {}", fallback_symbol, e);
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    
    {
        let found = state.found_gems.lock().unwrap();
        info!("📊 {} altcoin caricate con dati LIVE", found.len());
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
        
        // PARTE 1: ALTCOIN AFFERMATE (filtrate)
        match jupiter::find_profitable_altcoins().await {
            Ok(altcoins) => {
                debug!("📊 {} altcoin affermate trovate", altcoins.len());
                for coin in altcoins {
                    // Salta token esclusi (SOL, stablecoins)
                    if is_excluded_token(&coin.address, &coin.symbol) {
                        continue;
                    }
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
                        image_url: get_image_url(&coin.image_url, &coin.address),
                        timestamp: chrono::Utc::now().timestamp(),
                        source: "ALTCOIN".into(),
                    });
                }
            },
            Err(e) => debug!("⚠️ Altcoin fetch: {}", e),
        }
        
        // PARTE 2: NUOVE GEMME (ogni 3 cicli) - filtrate
        if cycle % 3 == 0 {
            match jupiter::discover_trending_gems().await {
                Ok(gems) => {
                    debug!("💎 {} nuove gemme trovate", gems.len());
                    
                    for gem in gems {
                        // Salta token esclusi (SOL, stablecoins)
                        if is_excluded_token(&gem.address, &gem.symbol) {
                            continue;
                        }
                        
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
                                            image_url: get_image_url(&gem.image_url, &gem.address),
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
            }
            // Se ci sono già gems ma le API falliscono, mantieni quelle esistenti
            // Le top altcoin vengono caricate all'avvio, quindi ci saranno sempre dati
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

    // ═══════════════════════════════════════════════════════════════
    // PRECARICA GEMME PRIMA DI AVVIARE API (fix per UI vuota)
    // ═══════════════════════════════════════════════════════════════
    info!("💎 Precaricamento gemme per UI...");
    {
        // Top token da caricare subito (NO SOL, NO stablecoins)
        let top_tokens = vec![
            ("JUPyiwrYJFskUPiHa7hkeR8VUtKCw785HvjeyzmEgGz", "JUP", "https://static.jup.ag/jup/icon.png"),
            ("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", "BONK", "https://arweave.net/hQiPZOsRZXGXBJd_82PhVdlM_hACsT_q6wqwf5cSY7I"),
            ("EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", "WIF", "https://dd.dexscreener.com/ds-data/tokens/solana/EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm.png"),
            ("7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr", "POPCAT", "https://dd.dexscreener.com/ds-data/tokens/solana/7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr.png"),
            ("rndrizKT3MK1iimdxRdWabcF7Zg7AR5T4nud4EkHBof", "RENDER", "https://dd.dexscreener.com/ds-data/tokens/solana/rndrizKT3MK1iimdxRdWabcF7Zg7AR5T4nud4EkHBof.png"),
            ("4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R", "RAY", "https://dd.dexscreener.com/ds-data/tokens/solana/4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R.png"),
            ("orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE", "ORCA", "https://dd.dexscreener.com/ds-data/tokens/solana/orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE.png"),
        ];
        
        let mut preloaded_gems: Vec<GemData> = Vec::new();
        for (addr, fallback_symbol, fallback_img) in &top_tokens {
            match jupiter::get_token_market_data(addr).await {
                Ok(mkt) => {
                    if mkt.price > 0.0 && mkt.liquidity_usd > 5000.0 {
                        // Usa immagine DexScreener se API non restituisce immagine valida
                        let image = if mkt.image_url.is_empty() || mkt.image_url.contains("undefined") {
                            fallback_img.to_string()
                        } else {
                            mkt.image_url.clone()
                        };
                        
                        preloaded_gems.push(GemData {
                            token: mkt.address.clone(),
                            symbol: if mkt.symbol == "UNK" { fallback_symbol.to_string() } else { mkt.symbol.clone() },
                            name: mkt.name.clone(),
                            price: mkt.price,
                            safety_score: mkt.score.max(85),
                            liquidity_usd: mkt.liquidity_usd,
                            market_cap: mkt.market_cap,
                            volume_24h: mkt.volume_24h,
                            change_1h: mkt.change_1h,
                            change_24h: mkt.change_24h,
                            image_url: image,
                            timestamp: chrono::Utc::now().timestamp(),
                            source: "TOP".to_string(),
                        });
                        info!("  ✓ {} | ${:.6} | Liq: ${:.0}K", mkt.symbol, mkt.price, mkt.liquidity_usd/1000.0);
                    }
                },
                Err(e) => {
                    warn!("  ⚠️ {} - Errore: {}", fallback_symbol, e);
                }
            }
            sleep(Duration::from_millis(150)).await;
        }
        
        if !preloaded_gems.is_empty() {
            let mut found = state.found_gems.lock().unwrap();
            *found = preloaded_gems;
            info!("✅ {} gemme precaricate per UI", found.len());
        }
    }

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

    // Daily Report Task (ogni sera alle 21:00)
    let p7 = pool.clone();
    let n7 = net.clone();
    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    tokio::spawn(async move { run_daily_report_task(p7, n7, bot_token).await; });

    info!("✅ Tutti i moduli AMMS avviati");
    info!("   • Market Strategy: EMA, RSI, ATR, Bollinger");
    info!("   • Position Manager: Trailing Stop ATR-based");
    info!("   • Auto-Reinvestment: Wealth-adaptive");
    info!("   • Sniper: Anti-rug, Anti-honeypot");
    info!("   • Daily Report: 21:00 ogni sera");

    match tokio::signal::ctrl_c().await {
        Ok(()) => info!("🛑 Chiusura sicura..."),
        Err(_) => {}
    }
    pool.close().await;
}
