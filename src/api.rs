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

/// Estrae user_id da header o query param
fn extract_user_id(
    tg_id: Option<String>,
    session: Option<String>,
    tg_data: Option<String>,
) -> String {
    // 1. Priorità: Telegram ID diretto (da header x-telegram-id)
    if let Some(id) = tg_id.as_ref() {
        if !id.is_empty() && id != "undefined" && id != "null" && id.len() > 3 {
            return format!("tg_{}", id);
        }
    }
    
    // 2. Telegram initData (dal WebApp)
    if let Some(data) = tg_data.as_ref() {
        if !data.is_empty() && data != "undefined" {
            if let Some(user_id) = parse_telegram_init_data(data) {
                return format!("tg_{}", user_id);
            }
        }
    }
    
    // 3. Session token (per utenti web registrati)
    if let Some(sess) = session.as_ref() {
        if !sess.is_empty() && sess != "undefined" && sess.len() >= 16 {
            // Il session token è l'ID completo per utenti web
            return format!("sess_{}", &sess[..16.min(sess.len())]);
        }
    }
    
    // 4. Guest ID persistente (passato dal frontend)
    // Il frontend deve generare e salvare un guest_id nel localStorage
    // e passarlo nell'header x-session-token come "guest_XXXXX"
    if let Some(sess) = session.as_ref() {
        if sess.starts_with("guest_") && sess.len() > 6 {
            return sess.clone();
        }
    }
    
    // 5. FALLBACK: Genera guest deterministico (per retrocompatibilità)
    // NOTA: Questo causa wallet diversi ad ogni sessione
    // Il frontend DEVE passare un guest_id persistente
    warn!("⚠️ Utente non autenticato - wallet temporaneo");
    format!("guest_{}", chrono::Utc::now().timestamp() % 100000)
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
        .with(cors)
        .with(warp::log("api"));

    info!("🌍 API Server LIVE: Porta 3000");
    info!("   ✓ Multi-user: Telegram + Web Auth");
    info!("   ✓ Endpoints: /health, /status, /trade, /withdraw, /auth");
    
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
    let user_id = extract_user_id(tg_id, session, tg_data);
    
    // Crea wallet per questo utente (se non esiste)
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
    let user_id = extract_user_id(tg_id, session, tg_data);
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

        let input = "So11111111111111111111111111111111111111112";
        match jupiter::get_jupiter_swap_tx(&payer.pubkey().to_string(), input, &req.token, amount_lamports, 100).await {
            Ok(mut tx) => {
                if let Ok(bh) = net.rpc.get_latest_blockhash().await {
                    tx.sign(&[&payer], bh);
                    
                    match net.send_transaction_fast(&tx).await {
                        Ok(sig) => {
                            let _ = db::record_buy(&pool, &user_id, &req.token, &sig, amount_lamports).await;
                            return Ok(warp::reply::json(&ApiResponse {
                                success: true,
                                message: "Buy Eseguito".into(),
                                tx_signature: sig.clone(),
                                solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
                            }));
                        }
                        Err(e) => warn!("⚠️ Jupiter TX fallita: {}", e),
                    }
                }
            }
            Err(e) => warn!("⚠️ Jupiter quote: {}", e),
        }

        // Raydium fallback
        if let Ok(mint) = Pubkey::from_str(&req.token) {
            if let Ok(keys) = raydium::fetch_pool_keys_by_mint(&net, &mint).await {
                if let Ok(sig) = raydium::execute_swap(&net, &payer, &keys, mint, amount_lamports, 200).await {
                    let _ = db::record_buy(&pool, &user_id, &req.token, &sig, amount_lamports).await;
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
            Ok(mut tx) => {
                if let Ok(bh) = net.rpc.get_latest_blockhash().await {
                    tx.sign(&[&payer], bh);
                    
                    match net.send_transaction_fast(&tx).await {
                        Ok(sig) => {
                            let _ = db::record_sell(&pool, &user_id, &req.token, &sig, 0.0).await;
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
    let user_id = extract_user_id(tg_id, session, tg_data);
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

    let instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(100_000),
        ComputeBudgetInstruction::set_compute_unit_limit(50_000),
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
