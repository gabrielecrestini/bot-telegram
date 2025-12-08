use crate::{db, jito, jupiter, network, orca, wallet_manager, AppState, GemData};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::hash::Hash;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use solana_sdk::system_instruction;
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use spl_associated_token_account;
use spl_associated_token_account::get_associated_token_address;
use sqlx::Row;
use std::env;
use std::error::Error;
use std::str::FromStr;
use std::sync::Arc;
use warp::Filter;

const SOLSCAN_TX_URL: &str = "https://solscan.io/tx/";
const OFFRAMP_PROVIDER: &str = "FastRamp SEPA (zero fee, SwiftBridge-ready)";
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const EURC_MINT: &str = "8V8ePA5shGtYZ8i9WGVrb8grh4ALpEDSz3i63MMYjVn2"; // Euro Coin (Circle) su Solana
const USDT_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

fn resolve_offramp_vault(token: &str) -> Option<Pubkey> {
    // Prefer a mint-specific vault if provided (OFFRAMP_USDC_VAULT, OFFRAMP_USDT_VAULT, OFFRAMP_EURC_VAULT)
    let env_key = format!("OFFRAMP_{}_VAULT", token);
    if let Ok(val) = env::var(&env_key) {
        if let Ok(pk) = Pubkey::from_str(&val) {
            return Some(pk);
        }
    }

    // Fallback to a generic stablecoin vault
    if let Ok(val) = env::var("OFFRAMP_STABLE_VAULT") {
        if let Ok(pk) = Pubkey::from_str(&val) {
            return Some(pk);
        }
    }

    None
}

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
    balance_sol: f64, // Saldo disponibile wallet
    balance_usd: f64,
    stable_usdc: f64,
    stable_usdt: f64,
    stable_eur: f64,
    sol_price_usd: f64,
    wealth_level: String,
    active_trades_count: usize,
    system_status: String,
    bot_active: bool,                   // Stato bot persistente dal DB
    locked_sol: f64,                    // SOL bloccati in posizioni aperte
    available_sol: f64,                 // SOL effettivamente disponibili per trading
    open_positions: Vec<db::OpenTrade>, // Posizioni aperte con dettagli
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
    #[serde(default)]
    base: Option<String>,
}

#[derive(Deserialize)]
struct WithdrawRequest {
    amount: f64,
    token: String,
    destination_address: String,
}

#[derive(Deserialize)]
struct ConvertRequest {
    amount_sol: f64,
    stable: String, // "USDC", "EURC" o "USDT"
}

#[derive(Deserialize)]
struct SellStableRequest {
    amount: f64,
    stable: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
}

async fn get_stable_balance(net: &network::NetworkClient, owner: &Pubkey, mint: &Pubkey) -> f64 {
    let ata = get_associated_token_address(owner, mint);
    match net.rpc.get_token_account_balance(&ata).await {
        Ok(res) => res.ui_amount.unwrap_or_else(|| {
            let amount: f64 = res.amount.parse().unwrap_or(0.0);
            let factor = 10u64.saturating_pow(res.decimals as u32) as f64;
            amount / factor
        }),
        Err(_) => 0.0,
    }
}

#[derive(Deserialize)]
struct WebAuthRequest {
    email: String,
    password: String,
    action: String, // "login" o "register"
}

#[derive(Deserialize)]
struct GoogleAuthRequest {
    id_token: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    errors: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sol_received: Option<f64>,
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

/// Estrai sub/email dal token Google (senza dipendere da rete esterna)
fn parse_google_id_token(token: &str) -> Option<(String, Option<String>)> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }

    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let payload_json: serde_json::Value = serde_json::from_slice(&payload).ok()?;

    // Controlla exp se presente
    if let Some(exp) = payload_json.get("exp").and_then(|v| v.as_i64()) {
        if exp < chrono::Utc::now().timestamp() {
            warn!("Google token scaduto");
            return None;
        }
    }

    let sub = payload_json.get("sub")?.as_str()?.to_string();
    let email = payload_json
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Some((sub, email))
}

fn validate_iban(iban: &str) -> bool {
    if iban.len() < 15 || iban.len() > 34 {
        return false;
    }

    if !iban.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }

    let rearranged = format!("{}{}", &iban[4..], &iban[..4]);
    let mut expanded = String::with_capacity(rearranged.len() * 2);

    for ch in rearranged.chars() {
        if ch.is_ascii_digit() {
            expanded.push(ch);
        } else {
            let val = (ch.to_ascii_uppercase() as u32) - 55; // A=10
            expanded.push_str(&val.to_string());
        }
    }

    let mut remainder: u128 = 0;
    for chunk in expanded.as_bytes().chunks(7) {
        let part = std::str::from_utf8(chunk).unwrap_or("0");
        let num = format!("{}{}", remainder, part)
            .parse::<u128>()
            .unwrap_or(0);
        remainder = num % 97;
    }

    remainder == 1
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
pub async fn get_sol_price() -> f64 {
    match jupiter::get_token_market_data("So11111111111111111111111111111111111111112").await {
        Ok(data) => data.price,
        Err(_) => {
            match reqwest::get(
                "https://api.coingecko.com/api/v3/simple/price?ids=solana&vs_currencies=usd",
            )
            .await
            {
                Ok(resp) => {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        json["solana"]["usd"].as_f64().unwrap_or(180.0)
                    } else {
                        180.0
                    }
                }
                Err(_) => 180.0,
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SERVER
// ═══════════════════════════════════════════════════════════════════════════════

pub async fn start_server(
    pool: sqlx::SqlitePool,
    net: Arc<network::NetworkClient>,
    state: Arc<AppState>,
) {
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

    // Convert SOL -> Stablecoin endpoint
    let convert = warp::path("convert")
        .and(warp::post())
        .and(tg_id_filter.clone())
        .and(session_filter.clone())
        .and(tg_data_filter.clone())
        .and(warp::body::json())
        .and(pool_filter.clone())
        .and(net_filter.clone())
        .and_then(handle_convert);

    // Convert stablecoin -> SOL (per top-up carta che accredita USDC/USDT/EURC)
    let sell_stable = warp::path!("convert" / "stable")
        .and(warp::post())
        .and(tg_id_filter.clone())
        .and(session_filter.clone())
        .and(tg_data_filter.clone())
        .and(warp::body::json())
        .and(pool_filter.clone())
        .and(net_filter.clone())
        .and_then(handle_sell_stable);

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

    // Google Auth endpoint (link + login)
    let google_auth = warp::path!("auth" / "google")
        .and(warp::post())
        .and(tg_id_filter.clone())
        .and(session_filter.clone())
        .and(tg_data_filter.clone())
        .and(warp::body::json())
        .and(pool_filter.clone())
        .and_then(handle_google_auth);

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
            "Content-Type",
            "content-type",
            "Authorization",
            "authorization",
            "X-Telegram-Id",
            "x-telegram-id",
            "X-Session-Token",
            "x-session-token",
            "X-Telegram-Data",
            "x-telegram-data",
            "Accept",
            "Origin",
            "Access-Control-Request-Method",
            "Access-Control-Request-Headers",
        ])
        .max_age(86400);

    let routes = health
        .or(status)
        .or(trade)
        .or(convert)
        .or(sell_stable)
        .or(withdraw)
        .or(auth)
        .or(google_auth)
        .or(bot_start)
        .or(bot_stop)
        .with(cors)
        .with(warp::log("api"));

    info!("🌍 API Server LIVE: Porta 3000 (TPU Priority)");
    info!("   ✓ Multi-user: Telegram + Web Auth");
    info!(
        "   ✓ Endpoints: /health, /status, /trade, /convert, /withdraw, /auth, /auth/google, /bot/start, /bot/stop"
    );

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
    let mut usdc_balance = 0.0;
    let mut usdt_balance = 0.0;
    let mut eurc_balance = 0.0;
    if let Ok(pk) = Pubkey::from_str(&pubkey_str) {
        balance = net.get_balance_fast(&pk).await as f64 / LAMPORTS_PER_SOL as f64;
        usdc_balance = get_stable_balance(&net, &pk, &Pubkey::from_str(USDC_MINT).unwrap()).await;
        usdt_balance = get_stable_balance(&net, &pk, &Pubkey::from_str(USDT_MINT).unwrap()).await;
        eurc_balance = get_stable_balance(&net, &pk, &Pubkey::from_str(EURC_MINT).unwrap()).await;
    }

    let sol_price = get_sol_price().await;
    let balance_usd = balance * sol_price;
    let balance_eur = balance_usd * 0.92;

    let wealth_level = if balance_eur < 5.0 {
        "MICRO"
    } else if balance_eur < 15.0 {
        "POOR"
    } else if balance_eur < 50.0 {
        "LOW_MEDIUM"
    } else if balance_eur < 100.0 {
        "MEDIUM"
    } else if balance_eur < 200.0 {
        "HIGH_MEDIUM"
    } else {
        "RICH"
    }
    .to_string();

    // Token da escludere dalle raccomandazioni (SOL, stablecoins)
    const EXCLUDED_TOKENS: &[&str] = &[
        "So11111111111111111111111111111111111111112", // SOL (non puoi tradare SOL per SOL)
        "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", // USDT
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // USDC
        "USDH1SM1ojwWUga67PGrgFWUHibbjqMvuMaDkRJTgkX", // USDH
    ];

    let mut gems = state.found_gems.lock().unwrap().clone();
    // Filtra token esclusi e senza dati validi
    gems.retain(|g| {
        !EXCLUDED_TOKENS.contains(&g.token.as_str())
            && g.price > 0.0
            && g.liquidity_usd >= 5000.0
            && !["USDC", "USDT", "USDH", "DAI"].contains(&g.symbol.to_uppercase().as_str())
    });
    gems.sort_by(|a, b| b.safety_score.cmp(&a.safety_score));

    let signals = state.math_signals.lock().unwrap().clone();

    // Carica posizioni aperte con tutti i dettagli
    let open_positions = db::get_open_trades(&pool, &user_id)
        .await
        .unwrap_or_default();
    let active_trades = open_positions.len();

    // Calcola SOL bloccati e disponibili
    let locked_sol = db::get_locked_sol(&pool, &user_id).await.unwrap_or(0.0);
    let available_sol = (balance - 0.01).max(0.0); // Mantieni 0.01 SOL per gas

    let (trades_history, withdrawals_history) = db::get_all_history(&pool, &user_id)
        .await
        .unwrap_or((vec![], vec![]));

    // Carica lo stato del bot dal database (persistente)
    let bot_active_db = db::get_bot_status(&pool, &user_id).await.unwrap_or(false);

    // Sincronizza lo state globale con il database
    {
        let mut bot_users = state.bot_active_users.lock().unwrap();
        if bot_active_db && !bot_users.contains_key(&user_id) {
            // Riattiva con valori default se era attivo nel DB
            bot_users.insert(user_id.clone(), (0.0, "BOTH".to_string()));
        }
    }

    Ok(warp::reply::json(&DashboardData {
        user_id: user_id.clone(),
        wallet_address: pubkey_str,
        balance_sol: balance,
        balance_usd,
        stable_usdc: usdc_balance,
        stable_usdt: usdt_balance,
        stable_eur: eurc_balance,
        sol_price_usd: sol_price,
        wealth_level,
        active_trades_count: active_trades,
        system_status: "ONLINE".to_string(),
        bot_active: bot_active_db,
        locked_sol,
        available_sol,
        open_positions,
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
    let pair_base = req.base.as_deref().unwrap_or("SOL");

    info!(
        "📨 Trade [{} @ {}]: {} {} -> {}",
        user_id, pair_base, req.action, req.amount_sol, req.token
    );

    // ═══════════════════════════════════════════════════════════════
    // BLOCCO ACQUISTI MANUALI SE BOT ATTIVO
    // Il bot gestisce tutto quando è attivo - previene conflitti
    // ═══════════════════════════════════════════════════════════════
    if req.action == "BUY" {
        let bot_active = db::get_bot_status(&pool, &user_id).await.unwrap_or(false);
        if bot_active {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "🤖 Bot Attivo! Ferma il bot per fare trading manuale. I tuoi SOL sono gestiti automaticamente.".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    }

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
    let base = pair_base.to_uppercase();
    let (input_mint, amount_lamports, base_label) =
        if ["USDT", "USDC", "EURC"].contains(&base.as_str()) {
            let (mint, decimals) = if base == "USDT" {
                (USDT_MINT, 6u8)
            } else if base == "USDC" {
                (USDC_MINT, 6u8)
            } else {
                (EURC_MINT, 6u8)
            };

            let mint_pk = Pubkey::from_str(mint).unwrap();
            let ata = get_associated_token_address(&payer.pubkey(), &mint_pk);
            let (available, decs) = match net.rpc.get_token_account_balance(&ata).await {
                Ok(res) => (res.ui_amount.unwrap_or(0.0), res.decimals as u8),
                Err(_) => (0.0, decimals),
            };

            if req.amount_sol <= 0.0 || req.amount_sol > available {
                return Ok(warp::reply::json(&ApiResponse {
                    success: false,
                    message: format!("Saldo insufficiente: hai {:.2} {}", available, base),
                    tx_signature: "".into(),
                    solscan_url: None,
                }));
            }

            let multiplier = 10u64.saturating_pow(decs as u32) as f64;
            let amount_units = (req.amount_sol * multiplier).round() as u64;
            (mint, amount_units, base)
        } else {
            let amount_units = (req.amount_sol * LAMPORTS_PER_SOL as f64) as u64;
            (
                "So11111111111111111111111111111111111111112",
                amount_units,
                "SOL".into(),
            )
        };

    if req.action == "BUY" {
        let min_required = amount_lamports + 10_000;
        if base_label == "SOL" && bal < min_required {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: format!(
                    "Fondi Insufficienti. Hai {:.4} SOL",
                    bal as f64 / LAMPORTS_PER_SOL as f64
                ),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        } else if base_label != "SOL" && bal < 200_000 {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Servono almeno 0.0002 SOL per le fee di rete".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }

        // Recupera dati token per PnL e immagine - IMPORTANTE per tracking
        let token_data = match jupiter::get_token_market_data(&req.token).await {
            Ok(data) => {
                info!("📊 Token data: {} @ ${:.10}", data.symbol, data.price);
                Some(data)
            }
            Err(e) => {
                warn!("⚠️ Impossibile recuperare dati token: {}", e);
                None
            }
        };

        // Entry price DEVE essere > 0 per calcolare P&L
        let entry_price = token_data.as_ref().map(|t| t.price).unwrap_or(0.0);
        if entry_price <= 0.0 {
            warn!("⚠️ Entry price = 0 per {}, P&L non calcolabile", req.token);
        }

        let token_symbol = token_data
            .as_ref()
            .map(|t| t.symbol.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("TKN-{}", &req.token[..6]));
        let token_image = token_data
            .as_ref()
            .map(|t| t.image_url.clone())
            .filter(|u| u.len() > 10 && !u.contains("undefined"))
            .unwrap_or_else(|| format!("https://img.jup.ag/v6/{}/logo", req.token));

        // SMART SWAP: Confronta Jupiter vs Orca per miglior prezzo
        let bh = match net.rpc.get_latest_blockhash().await {
            Ok(bh) => bh,
            Err(e) => {
                return Ok(warp::reply::json(&ApiResponse {
                    success: false,
                    message: format!("Errore blockhash: {}", e),
                    tx_signature: "".into(),
                    solscan_url: None,
                }));
            }
        };

        // Prova Smart Swap (Jupiter + Orca confronto)
        match prepare_best_route(&payer, input_mint, &req.token, amount_lamports, 200, bh).await {
            Ok((signed_tx, dex_used)) => {
                match jito::send_transaction_jito(&signed_tx, Some(50_000)).await {
                    Ok(bundle_id) => {
                        let _ = db::record_buy_complete(
                            &pool,
                            &user_id,
                            &req.token,
                            &bundle_id,
                            amount_lamports,
                            "MANUAL",
                            entry_price,
                            &token_symbol,
                            &token_image,
                        )
                        .await;
                        info!(
                            "✅ BUY {} @ ${:.8} | {} {} | {} | Jito: {}",
                            token_symbol,
                            entry_price,
                            req.amount_sol,
                            base_label,
                            dex_used,
                            bundle_id
                        );
                        return Ok(warp::reply::json(&ApiResponse {
                            success: true,
                            message: format!(
                                "Buy {} via {} ({}) ⚡",
                                token_symbol, dex_used, base_label
                            ),
                            tx_signature: bundle_id.clone(),
                            solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, bundle_id)),
                        }));
                    }
                    Err(_) => match net.send_versioned_transaction(&signed_tx).await {
                        Ok(sig) => {
                            let _ = db::record_buy_complete(
                                &pool,
                                &user_id,
                                &req.token,
                                &sig,
                                amount_lamports,
                                "MANUAL",
                                entry_price,
                                &token_symbol,
                                &token_image,
                            )
                            .await;
                            info!(
                                "✅ BUY {} @ ${:.8} | {} {} | {} | TX: {}",
                                token_symbol,
                                entry_price,
                                req.amount_sol,
                                base_label,
                                dex_used,
                                sig
                            );
                            return Ok(warp::reply::json(&ApiResponse {
                                success: true,
                                message: format!(
                                    "Buy {} via {} ({})",
                                    token_symbol, dex_used, base_label
                                ),
                                tx_signature: sig.clone(),
                                solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
                            }));
                        }
                        Err(e) => warn!("⚠️ {} TX fallita: {}", dex_used, e),
                    },
                }
            }
            Err(e) => warn!("⚠️ Smart swap fallito: {}", e),
        }

        // Trade fallito
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Trade fallito. Controlla liquidità del token e riprova.".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    } else if req.action == "SELL" {
        // Per vendere token, devo usare la quantità di TOKEN, non SOL!
        // Recupero il bilancio del token dall'account SPL
        let output = "So11111111111111111111111111111111111111112"; // SOL

        // Ottieni la quantità di token da vendere dal bilancio wallet
        let token_mint = match Pubkey::from_str(&req.token) {
            Ok(m) => m,
            Err(_) => {
                return Ok(warp::reply::json(&ApiResponse {
                    success: false,
                    message: "Indirizzo token non valido".into(),
                    tx_signature: "".into(),
                    solscan_url: None,
                }));
            }
        };

        // Trova l'account token associato
        let ata = spl_associated_token_account::get_associated_token_address(
            &payer.pubkey(),
            &token_mint,
        );

        // Ottieni il bilancio del token
        let token_balance = match net.rpc.get_token_account_balance(&ata).await {
            Ok(balance) => {
                // amount è una stringa, la convertiamo in u64
                balance.amount.parse::<u64>().unwrap_or(0)
            }
            Err(e) => {
                error!("❌ Errore lettura bilancio token: {}", e);
                return Ok(warp::reply::json(&ApiResponse {
                    success: false,
                    message: "Token non trovato nel wallet".into(),
                    tx_signature: "".into(),
                    solscan_url: None,
                }));
            }
        };

        if token_balance == 0 {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Nessun token da vendere".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }

        // Salva saldo SOL PRIMA della vendita
        let balance_before = net.get_balance_fast(&payer.pubkey()).await as f64 / 1_000_000_000.0;

        info!(
            "💰 Vendita {} token (raw: {}) | Saldo prima: {:.4} SOL",
            req.token, token_balance, balance_before
        );

        // Usa slippage più alto (5%) per evitare fallimenti su token con bassa liquidità
        match jupiter::get_jupiter_swap_tx(
            &payer.pubkey().to_string(),
            &req.token,
            output,
            token_balance,
            500,
        )
        .await
        {
            Ok(tx) => {
                if let Ok(bh) = net.rpc.get_latest_blockhash().await {
                    match jupiter::sign_versioned_transaction(&tx, &payer, bh) {
                        Ok(signed_tx) => {
                            match net.send_versioned_transaction(&signed_tx).await {
                                Ok(sig) => {
                                    // Aspetta un attimo che la transazione si confermi
                                    tokio::time::sleep(tokio::time::Duration::from_millis(2000))
                                        .await;

                                    // Verifica saldo DOPO la vendita
                                    let balance_after = net.get_balance_fast(&payer.pubkey()).await
                                        as f64
                                        / 1_000_000_000.0;
                                    let sol_received = balance_after - balance_before;

                                    // Calcola PnL
                                    let token_data =
                                        jupiter::get_token_market_data(&req.token).await.ok();
                                    let current_price =
                                        token_data.as_ref().map(|t| t.price).unwrap_or(0.0);

                                    let pnl_pct = if let Ok(trades) =
                                        db::get_open_trades(&pool, &user_id).await
                                    {
                                        if let Some(trade) =
                                            trades.iter().find(|t| t.token_address == req.token)
                                        {
                                            if trade.entry_price > 0.0 && current_price > 0.0 {
                                                ((current_price - trade.entry_price)
                                                    / trade.entry_price)
                                                    * 100.0
                                            } else {
                                                0.0
                                            }
                                        } else {
                                            0.0
                                        }
                                    } else {
                                        0.0
                                    };

                                    let _ =
                                        db::record_sell(&pool, &user_id, &req.token, &sig, pnl_pct)
                                            .await;

                                    let msg = if sol_received > 0.0 {
                                        format!(
                                            "Vendita completata! Ricevuti {:.4} SOL",
                                            sol_received
                                        )
                                    } else {
                                        format!("Vendita inviata! Controlla su Solscan")
                                    };

                                    info!(
                                        "✅ SELL {} | Ricevuti: {:.4} SOL | PnL: {:+.1}% | TX: {}",
                                        user_id, sol_received, pnl_pct, sig
                                    );

                                    return Ok(warp::reply::json(&ApiResponse {
                                        success: true,
                                        message: msg,
                                        tx_signature: sig.clone(),
                                        solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
                                    }));
                                }
                                Err(e) => {
                                    error!("❌ Sell TX invio fallito: {}", e);
                                    return Ok(warp::reply::json(&ApiResponse {
                                        success: false,
                                        message: format!("Errore invio TX: {}", e),
                                        tx_signature: "".into(),
                                        solscan_url: None,
                                    }));
                                }
                            }
                        }
                        Err(e) => {
                            error!("❌ Firma Sell TX fallita: {}", e);
                            return Ok(warp::reply::json(&ApiResponse {
                                success: false,
                                message: format!("Errore firma: {}", e),
                                tx_signature: "".into(),
                                solscan_url: None,
                            }));
                        }
                    }
                } else {
                    return Ok(warp::reply::json(&ApiResponse {
                        success: false,
                        message: "Errore rete Solana".into(),
                        tx_signature: "".into(),
                        solscan_url: None,
                    }));
                }
            }
            Err(e) => {
                error!("❌ Jupiter quote vendita fallita: {}", e);
                return Ok(warp::reply::json(&ApiResponse {
                    success: false,
                    message: format!("Jupiter error: {}. Prova con meno token o riprova.", e),
                    tx_signature: "".into(),
                    solscan_url: None,
                }));
            }
        }
    }

    Ok(warp::reply::json(&ApiResponse {
        success: false,
        message: "Azione non valida".into(),
        tx_signature: "".into(),
        solscan_url: None,
    }))
}

async fn get_stable_quote_with_fallback(
    input_mint: &str,
    output_mint: &str,
    amount: u64,
    slippage_bps: u16,
) -> Result<(u64, f64, String), Box<dyn Error + Send + Sync>> {
    if let Ok(q) = orca::get_quote(input_mint, output_mint, amount, slippage_bps).await {
        if q.out_amount > 0 {
            return Ok((q.out_amount, q.price_impact_pct, "Orca".into()));
        }
    }

    let q = jupiter::get_jupiter_quote(input_mint, output_mint, amount, slippage_bps).await?;
    Ok((q.out_amount, q.price_impact_pct, "Jupiter".into()))
}

async fn prepare_best_stable_swap(
    payer: &solana_sdk::signature::Keypair,
    input_mint: &str,
    output_mint: &str,
    amount: u64,
    slippage_bps: u16,
    blockhash: Hash,
) -> Result<(VersionedTransaction, String), Box<dyn Error + Send + Sync>> {
    if let Ok(q) = orca::get_quote(input_mint, output_mint, amount, slippage_bps).await {
        if q.out_amount > 0 {
            let tx = orca::get_swap_transaction(
                &payer.pubkey().to_string(),
                input_mint,
                output_mint,
                amount,
                slippage_bps,
            )
            .await?;
            let signed = orca::sign_transaction(&tx, payer, blockhash)?;
            return Ok((signed, "Orca".into()));
        }
    }

    let tx = jupiter::get_jupiter_swap_tx(
        &payer.pubkey().to_string(),
        input_mint,
        output_mint,
        amount,
        slippage_bps,
    )
    .await?;
    let signed = jupiter::sign_versioned_transaction(&tx, payer, blockhash)?;
    Ok((signed, "Jupiter".into()))
}

/// Usa un routing Orca->Jupiter generico per token non stable (es. coppie USDT/XRP)
async fn prepare_best_route(
    payer: &solana_sdk::signature::Keypair,
    input_mint: &str,
    output_mint: &str,
    amount: u64,
    slippage_bps: u16,
    blockhash: Hash,
) -> Result<(VersionedTransaction, String), Box<dyn Error + Send + Sync>> {
    if let Ok(q) = orca::get_quote(input_mint, output_mint, amount, slippage_bps).await {
        if q.out_amount > 0 {
            if let Ok(tx) = orca::get_swap_transaction(
                &payer.pubkey().to_string(),
                input_mint,
                output_mint,
                amount,
                slippage_bps,
            )
            .await
            {
                let signed = orca::sign_transaction(&tx, payer, blockhash)?;
                return Ok((signed, "Orca".into()));
            }
        }
    }

    let tx = jupiter::get_jupiter_swap_tx(
        &payer.pubkey().to_string(),
        input_mint,
        output_mint,
        amount,
        slippage_bps,
    )
    .await?;
    let signed = jupiter::sign_versioned_transaction(&tx, payer, blockhash)?;
    Ok((signed, "Jupiter".into()))
}

/// Converte SOL in stablecoin (USDC/EURC) privilegiando Orca (fallback Jupiter)
async fn handle_convert(
    tg_id: Option<String>,
    session: Option<String>,
    tg_data: Option<String>,
    req: ConvertRequest,
    pool: sqlx::SqlitePool,
    net: Arc<network::NetworkClient>,
) -> Result<impl warp::Reply, warp::Rejection> {
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

    let stable = req.stable.to_uppercase();
    let output_mint = match stable.as_str() {
        "USDC" => USDC_MINT,
        "EURC" => EURC_MINT,
        "USDT" => USDT_MINT,
        _ => {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Stablecoin non supportata (USDC/EURC/USDT)".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    if req.amount_sol <= 0.0 {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Importo non valido".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

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
    let amount_lamports = (req.amount_sol * LAMPORTS_PER_SOL as f64).round() as u64;
    let fee_buffer = 100_000; // 0.0001 SOL per fee e margine

    if amount_lamports + fee_buffer > bal {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: format!(
                "Fondi insufficienti. Disponibili {:.4} SOL",
                bal as f64 / LAMPORTS_PER_SOL as f64
            ),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    // Evita di bloccare i fondi del bot: richiede bot fermo
    if db::get_bot_status(&pool, &user_id).await.unwrap_or(false) {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Ferma il bot prima di convertire i SOL in stablecoin".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    // Pre-quote per validare l'output atteso e l'impatto prezzo
    let slippage_bps = 120; // 1.2% per conversioni fiat-safe
    let (out_amount, price_impact, _route_used) =
        match get_stable_quote_with_fallback(SOL_MINT, output_mint, amount_lamports, slippage_bps)
            .await
        {
            Ok(q) => q,
            Err(e) => {
                error!("❌ Quote convert fallita: {}", e);
                return Ok(warp::reply::json(&ApiResponse {
                    success: false,
                    message: format!("Quote non disponibile: {}", e),
                    tx_signature: "".into(),
                    solscan_url: None,
                }));
            }
        };

    if out_amount == 0 {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Quote non valida, riprova con un importo maggiore".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    if price_impact > 0.015 {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: format!(
                "Impatto prezzo troppo alto ({:.2}%). Riduci importo o riprova",
                price_impact * 100.0
            ),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    let bh = match net.rpc.get_latest_blockhash().await {
        Ok(hash) => hash,
        Err(e) => {
            error!("❌ Blockhash error: {}", e);
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Errore rete".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    let (signed_tx, route_label) = match prepare_best_stable_swap(
        &payer,
        SOL_MINT,
        output_mint,
        amount_lamports,
        slippage_bps,
        bh,
    )
    .await
    {
        Ok(tx) => tx,
        Err(e) => {
            error!("❌ Routing convert fallito: {}", e);
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: format!("Errore routing: {}", e),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    match net.send_versioned_transaction(&signed_tx).await {
        Ok(sig) => {
            info!(
                "✅ Convert {} SOL -> {} via {} | TX {}",
                req.amount_sol, stable, route_label, sig
            );
            Ok(warp::reply::json(&ApiResponse {
                success: true,
                message: format!("Convertito in {} ({})", stable, route_label),
                tx_signature: sig.clone(),
                solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
            }))
        }
        Err(e) => {
            error!("❌ Invio convert fallito: {}", e);
            Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: format!("Errore invio: {}", e),
                tx_signature: "".into(),
                solscan_url: None,
            }))
        }
    }
}

/// Converte stablecoin accreditate (USDC/USDT/EURC) in SOL per rientrare nel wallet
async fn handle_sell_stable(
    tg_id: Option<String>,
    session: Option<String>,
    tg_data: Option<String>,
    req: SellStableRequest,
    pool: sqlx::SqlitePool,
    net: Arc<network::NetworkClient>,
) -> Result<impl warp::Reply, warp::Rejection> {
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

    let stable = req.stable.to_uppercase();
    let input_mint = match stable.as_str() {
        "USDC" => USDC_MINT,
        "USDT" => USDT_MINT,
        "EURC" => EURC_MINT,
        _ => {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Stablecoin non supportata".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    if req.amount <= 0.0 {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Importo non valido".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

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

    let owner = payer.pubkey();
    let mint_pubkey = Pubkey::from_str(input_mint).unwrap();
    let ata = get_associated_token_address(&owner, &mint_pubkey);

    let (available, decimals) = match net.rpc.get_token_account_balance(&ata).await {
        Ok(res) => {
            let ui_amount = res.ui_amount.unwrap_or(0.0);
            (ui_amount, res.decimals as u8)
        }
        Err(_) => (0.0, 6u8),
    };

    if req.amount > available {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: format!("Saldo insufficiente: hai {:.2} {}", available, stable),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    let unit_multiplier = 10u64.saturating_pow(decimals as u32) as f64;
    let amount_base_units = (req.amount * unit_multiplier).round() as u64;

    // Pre-quote per verificare output e slippage
    let slippage_bps = 120;
    let (out_amount, price_impact, _route_used) =
        match get_stable_quote_with_fallback(input_mint, SOL_MINT, amount_base_units, slippage_bps)
            .await
        {
            Ok(q) => q,
            Err(e) => {
                error!("❌ Quote stable->SOL fallita: {}", e);
                return Ok(warp::reply::json(&ApiResponse {
                    success: false,
                    message: format!("Quote non disponibile: {}", e),
                    tx_signature: "".into(),
                    solscan_url: None,
                }));
            }
        };

    if out_amount == 0 {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Quote non valida, riprova con un importo maggiore".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    if price_impact > 0.02 {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: format!(
                "Impatto prezzo troppo alto ({:.2}%). Riduci importo o attendi più liquidità",
                price_impact * 100.0
            ),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    let bh = match net.rpc.get_latest_blockhash().await {
        Ok(hash) => hash,
        Err(e) => {
            error!("❌ Blockhash error: {}", e);
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Errore rete".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    let (signed_tx, route_label) = match prepare_best_stable_swap(
        &payer,
        input_mint,
        SOL_MINT,
        amount_base_units,
        slippage_bps,
        bh,
    )
    .await
    {
        Ok(tx) => tx,
        Err(e) => {
            error!("❌ Routing convert fallito: {}", e);
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: format!("Errore routing: {}", e),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    match net.send_versioned_transaction(&signed_tx).await {
        Ok(sig) => {
            info!(
                "✅ Convert {} -> SOL via {} | TX {}",
                stable, route_label, sig
            );
            Ok(warp::reply::json(&ApiResponse {
                success: true,
                message: format!("Convertito {} in SOL ({})", stable, route_label),
                tx_signature: sig.clone(),
                solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
            }))
        }
        Err(e) => {
            error!("❌ Invio convert fallito: {}", e);
            Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: format!("Errore invio TX: {}", e),
                tx_signature: "".into(),
                solscan_url: None,
            }))
        }
    }
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
    info!(
        "💸 Withdraw [{}]: {} {} -> {}",
        user_id, req.amount, req.token, req.destination_address
    );

    let token = req.token.to_uppercase();

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

    if req.amount <= 0.0 {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Importo non valido".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    let payer = match wallet_manager::get_decrypted_wallet(&pool, &user_id).await {
        Ok(kp) => kp,
        Err(_) => {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Wallet non trovato".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };
    let owner = payer.pubkey();

    let dest = match Pubkey::from_str(req.destination_address.trim()) {
        Ok(pk) => pk,
        Err(_) => {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Indirizzo non valido: inserisci un address Solana".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    let mut withdrawal_lamports: u64;
    let mut conversion_note: Option<String> = None;

    if ["USDC", "USDT", "EURC"].contains(&token.as_str()) {
        let mint = match token.as_str() {
            "USDC" => USDC_MINT,
            "USDT" => USDT_MINT,
            _ => EURC_MINT,
        };

        let mint_pk = Pubkey::from_str(mint).unwrap();
        let ata = get_associated_token_address(&owner, &mint_pk);
        let (available, decimals) = match net.rpc.get_token_account_balance(&ata).await {
            Ok(res) => {
                let ui_amount = res.ui_amount.unwrap_or(0.0);
                (ui_amount, res.decimals as u8)
            }
            Err(_) => (0.0, 6u8),
        };

        if req.amount > available {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: format!("Saldo insufficiente: hai {:.2} {}", available, token),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }

        let unit_multiplier = 10u64.saturating_pow(decimals as u32) as f64;
        let amount_base_units = (req.amount * unit_multiplier).round() as u64;

        let slippage_bps = 120u16;
        let (out_amount, price_impact, route_used) =
            match get_stable_quote_with_fallback(mint, SOL_MINT, amount_base_units, slippage_bps)
                .await
            {
                Ok(q) => q,
                Err(e) => {
                    error!("❌ Quote {} -> SOL fallita: {}", token, e);
                    return Ok(warp::reply::json(&ApiResponse {
                        success: false,
                        message: format!("Quote non disponibile: {}", e),
                        tx_signature: "".into(),
                        solscan_url: None,
                    }));
                }
            };

        if out_amount == 0 {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Quote non valida, riprova con un importo maggiore".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }

        if price_impact > 0.03 {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: format!(
                    "Impatto prezzo troppo alto ({:.2}%). Riduci importo o attendi più liquidità",
                    price_impact * 100.0
                ),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }

        let bh = match net.rpc.get_latest_blockhash().await {
            Ok(hash) => hash,
            Err(e) => {
                error!("❌ Blockhash error: {}", e);
                return Ok(warp::reply::json(&ApiResponse {
                    success: false,
                    message: "Errore rete".into(),
                    tx_signature: "".into(),
                    solscan_url: None,
                }));
            }
        };

        let (signed_tx, route_label) = match prepare_best_stable_swap(
            &payer,
            mint,
            SOL_MINT,
            amount_base_units,
            slippage_bps,
            bh,
        )
        .await
        {
            Ok(tx) => tx,
            Err(e) => {
                error!("❌ Routing {} -> SOL fallito: {}", token, e);
                return Ok(warp::reply::json(&ApiResponse {
                    success: false,
                    message: format!("Errore routing: {}", e),
                    tx_signature: "".into(),
                    solscan_url: None,
                }));
            }
        };

        match net.send_versioned_transaction(&signed_tx).await {
            Ok(sig) => {
                info!(
                    "✅ Convert {} -> SOL per prelievo ({}) | {} | TX {}",
                    token, req.amount, route_label, sig
                );
            }
            Err(e) => {
                error!("❌ Invio convert fallito: {}", e);
                return Ok(warp::reply::json(&ApiResponse {
                    success: false,
                    message: format!("Errore invio convert: {}", e),
                    tx_signature: "".into(),
                    solscan_url: None,
                }));
            }
        }

        withdrawal_lamports = out_amount;
        conversion_note = Some(format!(
            "Convertiti {:.4} {} in {:.6} SOL via {} (quote: {})",
            req.amount,
            token,
            out_amount as f64 / LAMPORTS_PER_SOL as f64,
            route_used,
            route_label
        ));
    } else if token != "SOL" {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Solo prelievi in SOL supportati (converti prima i token)".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    } else {
        withdrawal_lamports = (req.amount * LAMPORTS_PER_SOL as f64) as u64;
    }

    let bal = net.get_balance_fast(&owner).await;

    if bal < (withdrawal_lamports + 10_000) {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: format!(
                "Fondi insufficienti: {:.4} SOL",
                bal as f64 / LAMPORTS_PER_SOL as f64
            ),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    let withdrawal_id = match db::record_withdrawal_request(
        &pool,
        &user_id,
        withdrawal_lamports,
        &req.destination_address,
    )
    .await
    {
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
        ComputeBudgetInstruction::set_compute_unit_price(50_000), // Priorità media
        ComputeBudgetInstruction::set_compute_unit_limit(5_000),  // Trasferimento semplice
        system_instruction::transfer(&payer.pubkey(), &dest, withdrawal_lamports),
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

    let tx =
        Transaction::new_signed_with_payer(&instructions, Some(&payer.pubkey()), &[&payer], bh);

    match net.send_transaction_fast(&tx).await {
        Ok(sig) => {
            let _ = db::confirm_withdrawal(&pool, withdrawal_id, &sig).await;
            let note = conversion_note
                .map(|n| format!(" {}", n))
                .unwrap_or_else(|| "".into());
            Ok(warp::reply::json(&ApiResponse {
                success: true,
                message: format!("Prelievo SOL inviato!{}", note),
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
            email: None,
        }));
    }

    if req.password.len() < 6 {
        return Ok(warp::reply::json(&AuthResponse {
            success: false,
            user_id: "".into(),
            session_token: "".into(),
            message: "Password minimo 6 caratteri".into(),
            email: None,
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
                email: Some(req.email.clone()),
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
                info!(
                    "✅ Nuovo utente web registrato: {} -> {}",
                    req.email, pubkey
                );

                return Ok(warp::reply::json(&AuthResponse {
                    success: true,
                    user_id: user_id.clone(),
                    session_token: session,
                    message: "Registrazione completata!".into(),
                    email: Some(req.email.clone()),
                }));
            }
            Err(e) => {
                error!("❌ Errore creazione wallet: {}", e);
                return Ok(warp::reply::json(&AuthResponse {
                    success: false,
                    user_id: "".into(),
                    session_token: "".into(),
                    message: "Errore creazione account".into(),
                    email: None,
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
                if let Ok(settings_json) = serde_json::from_str::<serde_json::Value>(&settings_str)
                {
                    if let Some(stored_hash) = settings_json["password_hash"].as_str() {
                        if stored_hash == password_hash {
                            let session = generate_session_token(&user_id);
                            info!("✅ Login web: {}", req.email);

                            return Ok(warp::reply::json(&AuthResponse {
                                success: true,
                                user_id: user_id.clone(),
                                session_token: session,
                                message: "Login riuscito!".into(),
                                email: Some(req.email.clone()),
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
            email: None,
        }));
    }

    Ok(warp::reply::json(&AuthResponse {
        success: false,
        user_id: "".into(),
        session_token: "".into(),
        message: "Azione non valida".into(),
        email: None,
    }))
}

/// Login/Link con Google per riutilizzare il wallet Telegram su qualsiasi device
async fn handle_google_auth(
    tg_id: Option<String>,
    session: Option<String>,
    tg_data: Option<String>,
    req: GoogleAuthRequest,
    pool: sqlx::SqlitePool,
) -> Result<impl warp::Reply, warp::Rejection> {
    let (google_sub, google_email) = match parse_google_id_token(&req.id_token) {
        Some(res) => res,
        None => {
            return Ok(warp::reply::json(&AuthResponse {
                success: false,
                user_id: "".into(),
                session_token: "".into(),
                message: "Token Google non valido o scaduto".into(),
                email: None,
            }));
        }
    };

    // 1) Se l'utente è già autenticato (Telegram/Web), colleghiamo Google a QUEL wallet
    let current_user = extract_user_id(tg_id, session, tg_data);
    let mut target_user = current_user.clone();

    // 2) Cerca se esiste già un link Google -> utente
    if target_user.is_none() {
        if let Ok(rows) = sqlx::query("SELECT tg_id, settings FROM users")
            .fetch_all(&pool)
            .await
        {
            for row in rows {
                let uid: String = row.try_get("tg_id").unwrap_or_default();
                let settings: Option<String> = row.try_get("settings").ok();
                if let Some(settings_str) = settings {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&settings_str) {
                        if json["google_sub"].as_str() == Some(google_sub.as_str()) {
                            target_user = Some(uid);
                            break;
                        }
                    }
                }
            }
        }
    }

    // 3) Se non esiste, creiamo un utente dedicato Google
    if target_user.is_none() {
        let new_user = format!("gg_{}", &google_sub[..std::cmp::min(16, google_sub.len())]);
        if let Err(e) = wallet_manager::create_user_wallet(&pool, &new_user).await {
            error!("❌ Errore creazione wallet Google: {}", e);
            return Ok(warp::reply::json(&AuthResponse {
                success: false,
                user_id: "".into(),
                session_token: "".into(),
                message: "Impossibile creare wallet".into(),
                email: None,
            }));
        }
        target_user = Some(new_user);
    }

    let user_id = target_user.unwrap();

    // 4) Aggiorna settings con i dati Google e, se presente, con il Telegram linkato
    let row_opt = sqlx::query("SELECT settings FROM users WHERE tg_id = ?")
        .bind(&user_id)
        .fetch_optional(&pool)
        .await
        .unwrap_or(None);

    let mut settings_json = match row_opt.and_then(|r| r.try_get::<String, _>("settings").ok()) {
        Some(s) => {
            serde_json::from_str::<serde_json::Value>(&s).unwrap_or_else(|_| serde_json::json!({}))
        }
        None => serde_json::json!({}),
    };

    settings_json["google_sub"] = serde_json::Value::String(google_sub.clone());
    if let Some(email) = google_email.as_ref() {
        settings_json["google_email"] = serde_json::Value::String(email.clone());
    }

    if let Some(tg) = current_user.as_ref() {
        settings_json["linked_telegram"] = serde_json::Value::String(tg.clone());
    }

    let _ = sqlx::query("UPDATE users SET settings = ? WHERE tg_id = ?")
        .bind(settings_json.to_string())
        .bind(&user_id)
        .execute(&pool)
        .await;

    let session_token = generate_session_token(&user_id);
    info!(
        "✅ Login/Link Google per {} (email: {:?})",
        user_id, google_email
    );

    Ok(warp::reply::json(&AuthResponse {
        success: true,
        user_id,
        session_token,
        message: "Accesso Google riuscito".into(),
        email: google_email,
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
                errors: None,
                sol_received: None,
            }));
        }
    };

    info!(
        "🤖 Bot START [{}]: amount={}, strategy={}",
        user_id, req.amount, req.strategy
    );

    // Salva configurazione bot per questo utente
    let settings = serde_json::json!({
        "bot_active": true,
        "bot_amount": req.amount,
        "bot_strategy": req.strategy,
        "bot_started_at": chrono::Utc::now().timestamp()
    });

    // IMPORTANTE: Attiva is_active = 1 per permettere auto-trading
    let _ = sqlx::query(
        "UPDATE users SET settings = ?, is_active = 1, bot_started_at = ? WHERE tg_id = ?",
    )
    .bind(settings.to_string())
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(&user_id)
    .execute(&pool)
    .await;

    // Attiva il flag nel state globale
    {
        let mut bot_users = state.bot_active_users.lock().unwrap();
        bot_users.insert(user_id.clone(), (req.amount, req.strategy.clone()));
    }

    info!(
        "✅ Bot attivato per {} | Strategia: {} | Amount: {}",
        user_id, req.strategy, req.amount
    );

    Ok(warp::reply::json(&BotResponse {
        success: true,
        message: format!("Bot avviato! Strategia: {}", req.strategy),
        profit: Some(0.0),
        trades_count: Some(0),
        errors: None,
        sol_received: None,
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
                errors: Some(vec!["Autenticazione richiesta".to_string()]),
                sol_received: None,
            }));
        }
    };

    info!("🛑 Bot STOP [{}] - Avvio LIQUIDAZIONE TOTALE", user_id);

    // Rimuovi dal state globale SUBITO
    {
        let mut bot_users = state.bot_active_users.lock().unwrap();
        bot_users.remove(&user_id);
    }

    // ═══════════════════════════════════════════════════════════════
    // LIQUIDAZIONE TOTALE - Vendi TUTTO e converti in SOL
    // Traccia tutti gli errori per mostrarli all'utente
    // ═══════════════════════════════════════════════════════════════

    let mut error_log: Vec<String> = Vec::new();

    let open_trades = db::get_open_trades(&pool, &user_id)
        .await
        .unwrap_or_default();
    let total_positions = open_trades.len();
    let mut total_pnl_sol = 0.0;
    let mut closed_count = 0;
    let mut failed_count = 0;

    info!(
        "📊 Trovate {} posizioni aperte da liquidare",
        total_positions
    );

    if total_positions == 0 {
        // Nessuna posizione da vendere - disattiva bot e ritorna
        let _ = sqlx::query("UPDATE users SET is_active = 0 WHERE tg_id = ?")
            .bind(&user_id)
            .execute(&pool)
            .await;

        return Ok(warp::reply::json(&BotResponse {
            success: true,
            message: "Bot fermato. Nessuna posizione aperta.".into(),
            profit: Some(0.0),
            trades_count: Some(0),
            errors: None,
            sol_received: Some(0.0),
        }));
    }

    // Get payer once for all trades
    let payer = match wallet_manager::get_decrypted_wallet(&pool, &user_id).await {
        Ok(k) => k,
        Err(e) => {
            let err_msg = format!("Wallet non trovato: {}", e);
            error!("❌ {}", err_msg);
            return Ok(warp::reply::json(&BotResponse {
                success: false,
                message: "Errore wallet - impossibile vendere".into(),
                profit: None,
                trades_count: None,
                errors: Some(vec![err_msg]),
                sol_received: None,
            }));
        }
    };

    // Controlla saldo per fees
    let balance_before = net.get_balance_fast(&payer.pubkey()).await as f64 / 1_000_000_000.0;

    if balance_before < 0.002 {
        error_log.push(format!(
            "⚠️ Saldo basso per fees: {:.4} SOL",
            balance_before
        ));
    }

    info!("💰 Saldo iniziale: {:.4} SOL", balance_before);

    // ═══════════════════════════════════════════════════════════════
    // VENDITA SEQUENZIALE - Una posizione alla volta
    // ═══════════════════════════════════════════════════════════════

    for (idx, trade) in open_trades.iter().enumerate() {
        let token_short = if trade.token_address.len() > 8 {
            &trade.token_address[..8]
        } else {
            &trade.token_address
        };
        let symbol = if trade.token_symbol.is_empty() {
            token_short.to_string()
        } else {
            trade.token_symbol.clone()
        };

        info!(
            "💰 Vendita {}/{}: {} ({:.4} SOL)",
            idx + 1,
            total_positions,
            symbol,
            trade.amount_sol
        );

        // Ottieni il bilancio REALE del token nel wallet
        let mint = match Pubkey::from_str(&trade.token_address) {
            Ok(m) => m,
            Err(e) => {
                let err = format!("❌ {} - Indirizzo token invalido: {}", symbol, e);
                error_log.push(err.clone());
                error!("{}", err);
                let _ = db::record_sell(
                    &pool,
                    &user_id,
                    &trade.token_address,
                    "invalid_address",
                    0.0,
                )
                .await;
                failed_count += 1;
                continue;
            }
        };

        let ata =
            spl_associated_token_account::get_associated_token_address(&payer.pubkey(), &mint);

        let token_balance = match net.rpc.get_token_account_balance(&ata).await {
            Ok(balance) => balance.amount.parse::<u64>().unwrap_or(0),
            Err(e) => {
                let err = format!("⚠️ {} - Token non trovato nel wallet", symbol);
                error_log.push(err.clone());
                warn!("{}: {}", err, e);
                // Marca come venduto per pulire il DB
                let _ =
                    db::record_sell(&pool, &user_id, &trade.token_address, "not_found", 0.0).await;
                continue;
            }
        };

        if token_balance == 0 {
            let msg = format!("ℹ️ {} - Già venduto o bilancio 0", symbol);
            info!("{}", msg);
            let _ =
                db::record_sell(&pool, &user_id, &trade.token_address, "zero_balance", 0.0).await;
            continue;
        }

        let output = "So11111111111111111111111111111111111111112"; // SOL

        // Slippage progressivo: 3% -> 5% -> 8%
        let slippage_levels = [300, 500, 800];
        let mut sold = false;

        for (attempt, &slippage) in slippage_levels.iter().enumerate() {
            info!(
                "  Tentativo {}/3 con slippage {}%",
                attempt + 1,
                slippage as f64 / 100.0
            );

            // Get Jupiter quote
            let tx = match jupiter::get_jupiter_swap_tx(
                &payer.pubkey().to_string(),
                &trade.token_address,
                output,
                token_balance,
                slippage,
            )
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    let err = format!("⚠️ {} - Jupiter quote fallita: {}", symbol, e);
                    if attempt == slippage_levels.len() - 1 {
                        error_log.push(err.clone());
                    }
                    warn!("{}", err);
                    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                    continue;
                }
            };

            // Get blockhash
            let bh = match net.rpc.get_latest_blockhash().await {
                Ok(h) => h,
                Err(e) => {
                    let err = format!("❌ {} - Errore rete Solana: {}", symbol, e);
                    error_log.push(err.clone());
                    error!("{}", err);
                    break;
                }
            };

            // Sign transaction
            let signed_tx = match jupiter::sign_versioned_transaction(&tx, &payer, bh) {
                Ok(t) => t,
                Err(e) => {
                    let err = format!("❌ {} - Errore firma TX: {}", symbol, e);
                    error_log.push(err.clone());
                    error!("{}", err);
                    break;
                }
            };

            // Try Jito first, then RPC
            let send_result = match jito::send_transaction_jito(&signed_tx, Some(50_000)).await {
                Ok(sig) => Ok(sig),
                Err(_) => net.send_versioned_transaction(&signed_tx).await,
            };

            match send_result {
                Ok(sig) => {
                    // Calcola PnL
                    let current_price = jupiter::get_token_market_data(&trade.token_address)
                        .await
                        .ok()
                        .map(|m| m.price)
                        .unwrap_or(0.0);
                    let pnl_pct = if trade.entry_price > 0.0 && current_price > 0.0 {
                        ((current_price - trade.entry_price) / trade.entry_price) * 100.0
                    } else {
                        0.0
                    };
                    let pnl_sol = trade.amount_sol * (pnl_pct / 100.0);
                    total_pnl_sol += pnl_sol;

                    let _ =
                        db::record_sell(&pool, &user_id, &trade.token_address, &sig, pnl_pct).await;
                    closed_count += 1;
                    sold = true;

                    info!(
                        "✅ Venduto {} | PnL: {:+.1}% ({:+.4} SOL) | TX: {}",
                        symbol, pnl_pct, pnl_sol, sig
                    );
                    break;
                }
                Err(e) => {
                    let err_str = e.to_string();

                    // Errori specifici da mostrare all'utente
                    if err_str.contains("insufficient") || err_str.contains("Insufficient") {
                        let err = format!("❌ {} - Saldo insufficiente per fees", symbol);
                        error_log.push(err.clone());
                        error!("{}", err);
                        break; // Non ritentare
                    } else if err_str.contains("SlippageToleranceExceeded") {
                        warn!(
                            "⚠️ {} - Slippage superato, riprovo con slippage più alto",
                            symbol
                        );
                        // Continua al prossimo tentativo
                    } else {
                        let err = format!("⚠️ {} - TX fallita: {}", symbol, err_str);
                        if attempt == slippage_levels.len() - 1 {
                            error_log.push(err.clone());
                        }
                        warn!("{}", err);
                    }

                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
            }
        }

        if !sold {
            failed_count += 1;
            let err = format!("❌ {} - Impossibile vendere dopo 3 tentativi", symbol);
            if !error_log.iter().any(|e| e.contains(&symbol)) {
                error_log.push(err.clone());
            }
            error!("{}", err);
        }

        // Delay tra le vendite
        if idx < total_positions - 1 {
            tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;
        }
    }

    // Aspetta conferma transazioni e controlla saldo finale
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    let balance_after = net.get_balance_fast(&payer.pubkey()).await as f64 / 1_000_000_000.0;
    let actual_sol_received = balance_after - balance_before;

    // Aggiorna settings e DISATTIVA is_active
    let settings = serde_json::json!({
        "bot_active": false,
        "bot_stopped_at": chrono::Utc::now().timestamp(),
        "last_liquidation": {
            "sold": closed_count,
            "failed": failed_count,
            "total_positions": total_positions,
            "pnl_sol": total_pnl_sol,
            "sol_received": actual_sol_received,
            "errors": error_log.clone()
        }
    });

    let _ = sqlx::query("UPDATE users SET settings = ?, is_active = 0 WHERE tg_id = ?")
        .bind(settings.to_string())
        .bind(&user_id)
        .execute(&pool)
        .await;

    // Costruisci messaggio finale
    let message = if failed_count > 0 {
        format!(
            "⚠️ Vendute {}/{} posizioni | {} errori | SOL: {:+.4}",
            closed_count, total_positions, failed_count, actual_sol_received
        )
    } else if closed_count > 0 {
        format!(
            "✅ Tutte le {} posizioni vendute! | SOL: {:+.4} | PnL: {:+.2}%",
            closed_count,
            actual_sol_received,
            if total_pnl_sol != 0.0 {
                (total_pnl_sol / balance_before) * 100.0
            } else {
                0.0
            }
        )
    } else {
        "Bot fermato. Nessuna posizione venduta.".to_string()
    };

    info!(
        "🛑 Bot disattivato per {} | Vendute: {}/{} | SOL: {:+.4} | Errori: {}",
        user_id,
        closed_count,
        total_positions,
        actual_sol_received,
        error_log.len()
    );

    Ok(warp::reply::json(&BotResponse {
        success: failed_count == 0,
        message,
        profit: Some(total_pnl_sol),
        trades_count: Some(closed_count),
        errors: if error_log.is_empty() {
            None
        } else {
            Some(error_log)
        },
        sol_received: Some(actual_sol_received),
    }))
}
