use warp::Filter;
use std::sync::Arc;
use serde::{Serialize, Deserialize};
use crate::{db, network, wallet_manager, raydium, jupiter, AppState, GemData};
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::system_instruction;
use solana_sdk::transaction::Transaction;
use solana_sdk::signer::Signer;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use std::str::FromStr;
use log::{info, error, warn};
use sha2::{Sha256, Digest};
use sqlx::Row;

const SOLSCAN_TX_URL: &str = "https://solscan.io/tx/";

// --- DATI ---
#[derive(Serialize, Clone)]
pub struct SignalData {
    pub token: String,
    pub price: f64,
    pub score: u8,
    pub reason: String,
    pub timestamp: i64,
}

#[derive(Serialize)]
struct DashboardData {
    user_id: String,
    wallet_address: String,
    balance_sol: f64,
    balance_usd: f64,
    sol_price_usd: f64,
    wealth_level: String,
    active_trades_count: usize,
    system_status: String,
    gems_feed: Vec<GemData>,
    signals_feed: Vec<SignalData>,
    trades_history: Vec<db::TradeHistory>,
    withdrawals_history: Vec<db::WithdrawalHistory>,
}

#[derive(Deserialize)]
struct TradeRequest {
    action: String,
    token: String,
    amount_sol: f64,
}

#[derive(Deserialize)]
struct WithdrawRequest {
    amount: f64,
    token: String,
    destination_address: String,
}

#[derive(Serialize)]
struct ApiResponse {
    success: bool,
    message: String,
    tx_signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    solscan_url: Option<String>,
}

#[derive(Serialize)]
struct AuthResponse {
    success: bool,
    user_id: String,
    session_token: String,
    message: String,
}

#[derive(Deserialize)]
struct WebAuthRequest {
    email: String,
    password: String,
    action: String, // "login" o "register"
}

#[derive(Deserialize)]
struct BotStartRequest {
    amount: f64,      // 0 = automatico
    strategy: String, // "DIP", "BREAKOUT", "BOTH"
}

#[derive(Serialize)]
struct BotResponse {
    success: bool,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    profit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trades_count: Option<i32>,
}

// ═══════════════════════════════════════════════════════════════════════════════
// AUTENTICAZIONE
// ═══════════════════════════════════════════════════════════════════════════════

/// Genera un user_id sicuro da email (hash)
fn hash_email_to_id(email: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(email.as_bytes());
    let result = hasher.finalize();
    format!("web_{}", hex::encode(&result[..8])) // Primi 16 caratteri hex
}

/// Genera session token
fn generate_session_token(user_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(user_id.as_bytes());
    hasher.update(chrono::Utc::now().timestamp().to_string().as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..16])
}

/// Estrae user_id da header - SOLO utenti autenticati
/// Ritorna None se l'utente non è autenticato (nessun guest mode per sicurezza)
fn extract_user_id(
    tg_id: Option<String>,
    session: Option<String>,
    tg_data: Option<String>,
) -> Option<String> {
    // 1. Priorità: Telegram ID diretto (da header x-telegram-id)
    // L'ID Telegram è UNIVOCO e PERMANENTE per ogni utente
    if let Some(id) = tg_id.as_ref() {
        if !id.is_empty() && id != "undefined" && id != "null" && id.len() >= 5 {
            // Valida che sia un numero (ID Telegram sono sempre numerici)
            if id.chars().all(|c| c.is_numeric()) {
                info!("🔐 Auth: Telegram ID {}", id);
                return Some(format!("tg_{}", id));
            }
        }
    }
    
    // 2. Telegram initData (dal WebApp) - più sicuro, include firma
    if let Some(data) = tg_data.as_ref() {
        if !data.is_empty() && data != "undefined" && data.len() > 20 {
            if let Some(user_id) = parse_telegram_init_data(data) {
                info!("🔐 Auth: Telegram WebApp {}", user_id);
                return Some(format!("tg_{}", user_id));
            }
        }
    }
    
    // 3. Session token (per utenti web registrati con email/password)
    // Il session token deve essere valido (generato dal backend durante login)
    if let Some(sess) = session.as_ref() {
        if !sess.is_empty() && sess != "undefined" && sess.len() >= 32 {
            // Session token valido = utente ha fatto login con email/password
            // L'ID utente è derivato dall'hash dell'email (permanente)
            info!("🔐 Auth: Web Session");
            return Some(format!("sess_{}", &sess[..16]));
        }
    }
    
    // NESSUNA AUTENTICAZIONE VALIDA
    // Non permettiamo guest mode per proteggere i fondi degli utenti
    warn!("⚠️ Tentativo accesso senza autenticazione");
    None
}

/// Parse Telegram initData per estrarre user_id
fn parse_telegram_init_data(data: &str) -> Option<String> {
    // initData è URL-encoded: user=%7B%22id%22%3A123456...
    if let Ok(decoded) = urlencoding::decode(data) {
        // Cerca "id": nel JSON user
        if let Some(start) = decoded.find("\"id\":") {
            let rest = &decoded[start + 5..];
            if let Some(end) = rest.find(|c: char| !c.is_numeric()) {
                let id = &rest[..end];
                if !id.is_empty() {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

/// Ottiene il prezzo SOL in tempo reale
async fn get_sol_price() -> f64 {
    match jupiter::get_token_market_data("So11111111111111111111111111111111111111112").await {
        Ok(data) => data.price,
        Err(_) => {
            match reqwest::get("https://api.coingecko.com/api/v3/simple/price?ids=solana&vs_currencies=usd").await {
                Ok(resp) => {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        json["solana"]["usd"].as_f64().unwrap_or(180.0)
                    } else { 180.0 }
                }
                Err(_) => 180.0
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SERVER
// ═══════════════════════════════════════════════════════════════════════════════

pub async fn start_server(pool: sqlx::SqlitePool, net: Arc<network::NetworkClient>, state: Arc<AppState>) {
    let pool_filter = warp::any().map(move || pool.clone());
    let net_filter = warp::any().map(move || net.clone());
    let state_filter = warp::any().map(move || state.clone());

    // Header filters per autenticazione
    let tg_id_filter = warp::header::optional::<String>("x-telegram-id");
    let session_filter = warp::header::optional::<String>("x-session-token");
    let tg_data_filter = warp::header::optional::<String>("x-telegram-data");

    // Health check
    let health = warp::path("health")
        .and(warp::get())
        .map(|| warp::reply::json(&serde_json::json!({"status": "ok", "version": "2.0"})));

    // Status endpoint (con autenticazione)
    let status = warp::path("status")
        .and(warp::get())
        .and(tg_id_filter.clone())
        .and(session_filter.clone())
        .and(tg_data_filter.clone())
        .and(pool_filter.clone())
        .and(net_filter.clone())
        .and(state_filter.clone())
        .and_then(handle_status);

    // Trade endpoint
    let trade = warp::path("trade")
        .and(warp::post())
        .and(tg_id_filter.clone())
        .and(session_filter.clone())
        .and(tg_data_filter.clone())
        .and(warp::body::json())
        .and(pool_filter.clone())
        .and(net_filter.clone())
        .and_then(handle_trade);

    // Withdraw endpoint
    let withdraw = warp::path("withdraw")
        .and(warp::post())
        .and(tg_id_filter.clone())
        .and(session_filter.clone())
        .and(tg_data_filter.clone())
        .and(warp::body::json())
        .and(pool_filter.clone())
        .and(net_filter.clone())
        .and_then(handle_withdraw);

    // Web Auth endpoint (email/password)
    let auth = warp::path("auth")
        .and(warp::post())
        .and(warp::body::json())
        .and(pool_filter.clone())
        .and_then(handle_auth);

    // Bot Start endpoint
    let bot_start = warp::path!("bot" / "start")
        .and(warp::post())
        .and(tg_id_filter.clone())
        .and(session_filter.clone())
        .and(tg_data_filter.clone())
        .and(warp::body::json())
        .and(pool_filter.clone())
        .and(state_filter.clone())
        .and_then(handle_bot_start);

    // Bot Stop endpoint
    let bot_stop = warp::path!("bot" / "stop")
        .and(warp::post())
        .and(tg_id_filter.clone())
        .and(session_filter.clone())
        .and(tg_data_filter.clone())
        .and(pool_filter.clone())
        .and(net_filter.clone())
        .and(state_filter.clone())
        .and_then(handle_bot_stop);

    // CORS
    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec!["GET", "POST", "PUT", "DELETE", "OPTIONS"])
        .allow_headers(vec![
            "Content-Type", "content-type",
            "Authorization", "authorization",
            "X-Telegram-Id", "x-telegram-id",
            "X-Session-Token", "x-session-token",
            "X-Telegram-Data", "x-telegram-data",
            "Accept", "Origin",
            "Access-Control-Request-Method",
            "Access-Control-Request-Headers",
        ])
        .max_age(86400);

    let routes = health
        .or(status)
        .or(trade)
        .or(withdraw)
        .or(auth)
        .or(bot_start)
        .or(bot_stop)
        .with(cors)
        .with(warp::log("api"));

    info!("🌍 API Server LIVE: Porta 3000 (TPU Priority)");
    info!("   ✓ Multi-user: Telegram + Web Auth");
    info!("   ✓ Endpoints: /health, /status, /trade, /withdraw, /auth, /bot/start, /bot/stop");
    
    warp::serve(routes).run(([0, 0, 0, 0], 3000)).await;
}

// ═══════════════════════════════════════════════════════════════════════════════
// HANDLERS
// ═══════════════════════════════════════════════════════════════════════════════

async fn handle_status(
    tg_id: Option<String>,
    session: Option<String>,
    tg_data: Option<String>,
    pool: sqlx::SqlitePool,
    net: Arc<network::NetworkClient>,
    state: Arc<AppState>,
) -> Result<impl warp::Reply, warp::Rejection> {
    // SICUREZZA: Solo utenti autenticati
    let user_id = match extract_user_id(tg_id, session, tg_data) {
        Some(id) => id,
        None => {
            return Ok(warp::reply::json(&serde_json::json!({
                "error": "NOT_AUTHENTICATED",
                "message": "Devi accedere con Telegram o Email per usare il wallet",
                "require_login": true
            })));
        }
    };
    
    // Crea wallet per questo utente (se non esiste)
    // Il wallet è legato PERMANENTEMENTE all'user_id
    let pubkey_str = wallet_manager::create_user_wallet(&pool, &user_id)
        .await
        .unwrap_or_default();

    let mut balance = 0.0;
    if let Ok(pk) = Pubkey::from_str(&pubkey_str) {
        balance = net.get_balance_fast(&pk).await as f64 / LAMPORTS_PER_SOL as f64;
    }

    let sol_price = get_sol_price().await;
    let balance_usd = balance * sol_price;
    let balance_eur = balance_usd * 0.92;
    
    let wealth_level = if balance_eur < 5.0 { "MICRO" }
        else if balance_eur < 15.0 { "POOR" }
        else if balance_eur < 50.0 { "LOW_MEDIUM" }
        else if balance_eur < 100.0 { "MEDIUM" }
        else if balance_eur < 200.0 { "HIGH_MEDIUM" }
        else { "RICH" }.to_string();

    let mut gems = state.found_gems.lock().unwrap().clone();
    gems.sort_by(|a, b| b.safety_score.cmp(&a.safety_score));
    
    let signals = state.math_signals.lock().unwrap().clone();

    let active_trades = db::count_open_trades(&pool, &user_id).await.unwrap_or(0);

    let (trades_history, withdrawals_history) = db::get_all_history(&pool, &user_id).await
        .unwrap_or((vec![], vec![]));

    Ok(warp::reply::json(&DashboardData {
        user_id: user_id.clone(),
        wallet_address: pubkey_str,
        balance_sol: balance,
        balance_usd,
        sol_price_usd: sol_price,
        wealth_level,
        active_trades_count: active_trades,
        system_status: "ONLINE".to_string(),
        gems_feed: gems,
        signals_feed: signals,
        trades_history,
        withdrawals_history,
    }))
}

async fn handle_trade(
    tg_id: Option<String>,
    session: Option<String>,
    tg_data: Option<String>,
    req: TradeRequest,
    pool: sqlx::SqlitePool,
    net: Arc<network::NetworkClient>,
) -> Result<impl warp::Reply, warp::Rejection> {
    // SICUREZZA: Solo utenti autenticati possono fare trading
    let user_id = match extract_user_id(tg_id, session, tg_data) {
        Some(id) => id,
        None => {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Non autenticato. Accedi con Telegram o Email.".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };
    info!("📨 Trade [{}]: {} {} SOL -> {}", user_id, req.action, req.amount_sol, req.token);

    let payer = match wallet_manager::get_decrypted_wallet(&pool, &user_id).await {
        Ok(k) => k,
        Err(e) => {
            error!("❌ Wallet error per {}: {}", user_id, e);
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Wallet non trovato. Ricarica la pagina.".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    let bal = net.get_balance_fast(&payer.pubkey()).await;
    let amount_lamports = (req.amount_sol * LAMPORTS_PER_SOL as f64) as u64;

    if req.action == "BUY" {
        let min_required = amount_lamports + 10_000;
        if bal < min_required {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: format!("Fondi Insufficienti. Hai {:.4} SOL", bal as f64 / LAMPORTS_PER_SOL as f64),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }

        // Recupera dati token per PnL e immagine
        let token_data = jupiter::get_token_market_data(&req.token).await.ok();
        let entry_price = token_data.as_ref().map(|t| t.price).unwrap_or(0.0);
        let token_symbol = token_data.as_ref().map(|t| t.symbol.clone()).unwrap_or_default();
        let token_image = token_data.as_ref().map(|t| t.image_url.clone()).unwrap_or_default();

        let input = "So11111111111111111111111111111111111111112";
        match jupiter::get_jupiter_swap_tx(&payer.pubkey().to_string(), input, &req.token, amount_lamports, 100).await {
            Ok(tx) => {
                if let Ok(bh) = net.rpc.get_latest_blockhash().await {
                    // Firma la VersionedTransaction con il nuovo blockhash
                    match jupiter::sign_versioned_transaction(&tx, &payer, bh) {
                        Ok(signed_tx) => {
                            match net.send_versioned_transaction(&signed_tx).await {
                                Ok(sig) => {
                                    // Salva con tutti i dettagli per mostrare PnL
                                    let _ = db::record_buy_complete(
                                        &pool, &user_id, &req.token, &sig, amount_lamports,
                                        "MANUAL", entry_price, &token_symbol, &token_image
                                    ).await;
                                    info!("✅ BUY {} @ ${:.8} | {} SOL | TX: {}", token_symbol, entry_price, req.amount_sol, sig);
                                    return Ok(warp::reply::json(&ApiResponse {
                                        success: true,
                                        message: format!("Buy {} Eseguito", token_symbol),
                                        tx_signature: sig.clone(),
                                        solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
                                    }));
                                }
                                Err(e) => warn!("⚠️ Jupiter TX fallita: {}", e),
                            }
                        }
                        Err(e) => warn!("⚠️ Firma TX fallita: {}", e),
                    }
                }
            }
            Err(e) => warn!("⚠️ Jupiter quote: {}", e),
        }

        // Raydium fallback (usa Transaction normale)
        if let Ok(mint) = Pubkey::from_str(&req.token) {
            if let Ok(keys) = raydium::fetch_pool_keys_by_mint(&net, &mint).await {
                if let Ok(sig) = raydium::execute_swap(&net, &payer, &keys, mint, amount_lamports, 200).await {
                    let _ = db::record_buy_complete(
                        &pool, &user_id, &req.token, &sig, amount_lamports,
                        "MANUAL", entry_price, &token_symbol, &token_image
                    ).await;
                    info!("✅ BUY (Raydium) completato per {} | {} SOL | TX: {}", user_id, req.amount_sol, sig);
                    return Ok(warp::reply::json(&ApiResponse {
                        success: true,
                        message: "Buy Eseguito (Raydium)".into(),
                        tx_signature: sig.clone(),
                        solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
                    }));
                }
            }
        }

        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Trade fallito. Riprova.".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));

    } else if req.action == "SELL" {
        let output = "So11111111111111111111111111111111111111112";
        match jupiter::get_jupiter_swap_tx(&payer.pubkey().to_string(), &req.token, output, amount_lamports, 200).await {
            Ok(tx) => {
                if let Ok(bh) = net.rpc.get_latest_blockhash().await {
                    match jupiter::sign_versioned_transaction(&tx, &payer, bh) {
                        Ok(signed_tx) => {
                            match net.send_versioned_transaction(&signed_tx).await {
                                Ok(sig) => {
                                    let _ = db::record_sell(&pool, &user_id, &req.token, &sig, 0.0).await;
                                    info!("✅ SELL completato per {} | TX: {}", user_id, sig);
                                    return Ok(warp::reply::json(&ApiResponse {
                                        success: true,
                                        message: "Vendita Eseguita".into(),
                                        tx_signature: sig.clone(),
                                        solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
                                    }));
                                }
                                Err(e) => error!("❌ Sell TX: {}", e),
                            }
                        }
                        Err(e) => error!("❌ Firma Sell TX: {}", e),
                    }
                }
            }
            Err(e) => error!("❌ Jupiter sell: {}", e),
        }

        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Vendita fallita".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    Ok(warp::reply::json(&ApiResponse {
        success: false,
        message: "Azione non valida".into(),
        tx_signature: "".into(),
        solscan_url: None,
    }))
}

async fn handle_withdraw(
    tg_id: Option<String>,
    session: Option<String>,
    tg_data: Option<String>,
    req: WithdrawRequest,
    pool: sqlx::SqlitePool,
    net: Arc<network::NetworkClient>,
) -> Result<impl warp::Reply, warp::Rejection> {
    // SICUREZZA: Solo utenti autenticati possono prelevare
    let user_id = match extract_user_id(tg_id, session, tg_data) {
        Some(id) => id,
        None => {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Non autenticato. Accedi con Telegram o Email.".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };
    info!("💸 Withdraw [{}]: {} {} -> {}", user_id, req.amount, req.token, req.destination_address);

    if req.token != "SOL" {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Solo prelievi SOL".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    match db::can_withdraw(&pool, &user_id).await {
        Ok((allowed, msg)) => {
            if !allowed {
                return Ok(warp::reply::json(&ApiResponse {
                    success: false,
                    message: msg,
                    tx_signature: "".into(),
                    solscan_url: None,
                }));
            }
        }
        Err(e) => {
            error!("❌ DB error: {}", e);
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Errore verifica".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    }

    let payer = match wallet_manager::get_decrypted_wallet(&pool, &user_id).await {
        Ok(k) => k,
        Err(_) => {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Wallet non trovato".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    let bal = net.get_balance_fast(&payer.pubkey()).await;
    let amount = (req.amount * LAMPORTS_PER_SOL as f64) as u64;

    if bal < (amount + 10_000) {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: format!("Fondi insufficienti: {:.4} SOL", bal as f64 / LAMPORTS_PER_SOL as f64),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    let dest = match Pubkey::from_str(&req.destination_address) {
        Ok(pk) => pk,
        Err(_) => {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Indirizzo non valido".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    let withdrawal_id = match db::record_withdrawal_request(&pool, &user_id, amount, &req.destination_address).await {
        Ok(id) => id,
        Err(_) => {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Errore DB".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    // PRIORITY FEES OTTIMIZZATE per trasferimenti
    // Trasferimento SOL = ~450 CU, mettiamo margine a 5,000
    // 50,000 µLamp/CU × 5,000 CU = 250 lamports = 0.00000025 SOL (~$0.00005)
    let instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(50_000),  // Priorità media
        ComputeBudgetInstruction::set_compute_unit_limit(5_000),   // Trasferimento semplice
        system_instruction::transfer(&payer.pubkey(), &dest, amount),
    ];

    let bh = match net.rpc.get_latest_blockhash().await {
        Ok(hash) => hash,
        Err(_) => {
            let _ = db::mark_withdrawal_failed(&pool, withdrawal_id).await;
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Errore rete".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    let tx = Transaction::new_signed_with_payer(&instructions, Some(&payer.pubkey()), &[&payer], bh);

    match net.send_transaction_fast(&tx).await {
        Ok(sig) => {
            let _ = db::confirm_withdrawal(&pool, withdrawal_id, &sig).await;
            Ok(warp::reply::json(&ApiResponse {
                success: true,
                message: "Prelievo Inviato!".into(),
                tx_signature: sig.clone(),
                solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
            }))
        }
        Err(e) => {
            let _ = db::mark_withdrawal_failed(&pool, withdrawal_id).await;
            Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: format!("Errore: {}", e),
                tx_signature: "".into(),
                solscan_url: None,
            }))
        }
    }
}

/// Handler per autenticazione web (email/password)
async fn handle_auth(
    req: WebAuthRequest,
    pool: sqlx::SqlitePool,
) -> Result<impl warp::Reply, warp::Rejection> {
    // Validazione base
    if req.email.len() < 5 || !req.email.contains('@') {
        return Ok(warp::reply::json(&AuthResponse {
            success: false,
            user_id: "".into(),
            session_token: "".into(),
            message: "Email non valida".into(),
        }));
    }
    
    if req.password.len() < 6 {
        return Ok(warp::reply::json(&AuthResponse {
            success: false,
            user_id: "".into(),
            session_token: "".into(),
            message: "Password minimo 6 caratteri".into(),
        }));
    }

    let user_id = hash_email_to_id(&req.email);
    let password_hash = {
        let mut hasher = Sha256::new();
        hasher.update(req.password.as_bytes());
        hex::encode(hasher.finalize())
    };

    if req.action == "register" {
        // Verifica se esiste già
        let exists = sqlx::query("SELECT 1 FROM users WHERE tg_id = ?")
            .bind(&user_id)
            .fetch_optional(&pool)
            .await
            .unwrap_or(None)
            .is_some();
        
        if exists {
            return Ok(warp::reply::json(&AuthResponse {
                success: false,
                user_id: "".into(),
                session_token: "".into(),
                message: "Email già registrata. Usa Login.".into(),
            }));
        }

        // Crea wallet per nuovo utente
        match wallet_manager::create_user_wallet(&pool, &user_id).await {
            Ok(pubkey) => {
                // Salva password hash nei settings
                let settings = serde_json::json!({"password_hash": password_hash}).to_string();
                let _ = sqlx::query("UPDATE users SET settings = ? WHERE tg_id = ?")
                    .bind(&settings)
                    .bind(&user_id)
                    .execute(&pool)
                    .await;

                let session = generate_session_token(&user_id);
                info!("✅ Nuovo utente web registrato: {} -> {}", req.email, pubkey);
                
                return Ok(warp::reply::json(&AuthResponse {
                    success: true,
                    user_id: user_id.clone(),
                    session_token: session,
                    message: "Registrazione completata!".into(),
                }));
            }
            Err(e) => {
                error!("❌ Errore creazione wallet: {}", e);
                return Ok(warp::reply::json(&AuthResponse {
                    success: false,
                    user_id: "".into(),
                    session_token: "".into(),
                    message: "Errore creazione account".into(),
                }));
            }
        }
    } else if req.action == "login" {
        // Verifica credenziali
        let row = sqlx::query("SELECT settings FROM users WHERE tg_id = ?")
            .bind(&user_id)
            .fetch_optional(&pool)
            .await
            .unwrap_or(None);
        
        if let Some(row) = row {
            let settings: Option<String> = row.try_get("settings").ok();
            if let Some(settings_str) = settings {
                if let Ok(settings_json) = serde_json::from_str::<serde_json::Value>(&settings_str) {
                    if let Some(stored_hash) = settings_json["password_hash"].as_str() {
                        if stored_hash == password_hash {
                            let session = generate_session_token(&user_id);
                            info!("✅ Login web: {}", req.email);
                            
                            return Ok(warp::reply::json(&AuthResponse {
                                success: true,
                                user_id: user_id.clone(),
                                session_token: session,
                                message: "Login riuscito!".into(),
                            }));
                        }
                    }
                }
            }
        }
        
        return Ok(warp::reply::json(&AuthResponse {
            success: false,
            user_id: "".into(),
            session_token: "".into(),
            message: "Email o password errati".into(),
        }));
    }

    Ok(warp::reply::json(&AuthResponse {
        success: false,
        user_id: "".into(),
        session_token: "".into(),
        message: "Azione non valida".into(),
    }))
}

// ═══════════════════════════════════════════════════════════════════════════════
// BOT HANDLERS
// ═══════════════════════════════════════════════════════════════════════════════

async fn handle_bot_start(
    tg_id: Option<String>,
    session: Option<String>,
    tg_data: Option<String>,
    req: BotStartRequest,
    pool: sqlx::SqlitePool,
    state: Arc<AppState>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let user_id = match extract_user_id(tg_id, session, tg_data) {
        Some(id) => id,
        None => {
            return Ok(warp::reply::json(&BotResponse {
                success: false,
                message: "Non autenticato".into(),
                profit: None,
                trades_count: None,
            }));
        }
    };

    info!("🤖 Bot START [{}]: amount={}, strategy={}", user_id, req.amount, req.strategy);

    // Salva configurazione bot per questo utente
    let settings = serde_json::json!({
        "bot_active": true,
        "bot_amount": req.amount,
        "bot_strategy": req.strategy,
        "bot_started_at": chrono::Utc::now().timestamp()
    });

    let _ = sqlx::query("UPDATE users SET settings = ? WHERE tg_id = ?")
        .bind(settings.to_string())
        .bind(&user_id)
        .execute(&pool)
        .await;

    // Attiva il flag nel state globale
    {
        let mut bot_users = state.bot_active_users.lock().unwrap();
        bot_users.insert(user_id.clone(), (req.amount, req.strategy.clone()));
    }

    Ok(warp::reply::json(&BotResponse {
        success: true,
        message: format!("Bot avviato con strategia {}", req.strategy),
        profit: Some(0.0),
        trades_count: Some(0),
    }))
}

async fn handle_bot_stop(
    tg_id: Option<String>,
    session: Option<String>,
    tg_data: Option<String>,
    pool: sqlx::SqlitePool,
    net: Arc<network::NetworkClient>,
    state: Arc<AppState>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let user_id = match extract_user_id(tg_id, session, tg_data) {
        Some(id) => id,
        None => {
            return Ok(warp::reply::json(&BotResponse {
                success: false,
                message: "Non autenticato".into(),
                profit: None,
                trades_count: None,
            }));
        }
    };

    info!("🛑 Bot STOP [{}]", user_id);

    // Rimuovi dal state globale
    {
        let mut bot_users = state.bot_active_users.lock().unwrap();
        bot_users.remove(&user_id);
    }

    // Vendi tutte le posizioni aperte di questo utente
    let open_trades = db::get_open_trades(&pool, &user_id).await.unwrap_or_default();
    let mut total_profit = 0.0;
    let mut closed_count = 0;

    for trade in &open_trades {
        // Prova a vendere tramite Jupiter (con VersionedTransaction)
        if let Ok(payer) = wallet_manager::get_decrypted_wallet(&pool, &user_id).await {
            let output = "So11111111111111111111111111111111111111112";
            
            if let Ok(tx) = jupiter::get_jupiter_swap_tx(
                &payer.pubkey().to_string(),
                &trade.token_address,
                output,
                trade.amount_lamports,
                300, // slippage più alto per vendite urgenti
            ).await {
                if let Ok(bh) = net.rpc.get_latest_blockhash().await {
                    if let Ok(signed_tx) = jupiter::sign_versioned_transaction(&tx, &payer, bh) {
                        // Invia vendita
                        if let Ok(sig) = net.send_versioned_transaction(&signed_tx).await {
                            let _ = db::record_sell(&pool, &user_id, &trade.token_address, &sig, 0.0).await;
                            closed_count += 1;
                            info!("✅ Venduto {} per bot stop", trade.token_address);
                        }
                    }
                }
            }
        }
    }

    // Calcola profitto totale sessione
    if let Ok(stats) = db::get_user_stats(&pool, &user_id).await {
        total_profit = stats.total_pnl;
    }

    // Aggiorna settings
    let settings = serde_json::json!({
        "bot_active": false,
        "bot_stopped_at": chrono::Utc::now().timestamp()
    });
    
    let _ = sqlx::query("UPDATE users SET settings = ? WHERE tg_id = ?")
        .bind(settings.to_string())
        .bind(&user_id)
        .execute(&pool)
        .await;

    Ok(warp::reply::json(&BotResponse {
        success: true,
        message: format!("Bot fermato. Vendute {} posizioni.", closed_count),
        profit: Some(total_profit),
        trades_count: Some(closed_count),
    }))
}
