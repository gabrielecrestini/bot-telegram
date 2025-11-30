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
use log::{info, error};

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
    wallet_address: String,
    balance_sol: f64,
    active_trades_count: usize,
    system_status: String,
    gems_feed: Vec<GemData>,
    signals_feed: Vec<SignalData>,
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

// --- SERVER ---
pub async fn start_server(pool: sqlx::SqlitePool, net: Arc<network::NetworkClient>, state: Arc<AppState>) {
    let pool_filter = warp::any().map(move || pool.clone());
    let net_filter = warp::any().map(move || net.clone());
    let state_filter = warp::any().map(move || state.clone());

    let status = warp::path("status")
        .and(warp::get())
        .and(pool_filter.clone())
        .and(net_filter.clone())
        .and(state_filter.clone())
        .and_then(handle_status);

    let trade = warp::path("trade")
        .and(warp::post())
        .and(warp::body::json())
        .and(pool_filter.clone())
        .and(net_filter.clone())
        .and_then(handle_trade);

    let withdraw = warp::path("withdraw")
        .and(warp::post())
        .and(warp::body::json())
        .and(pool_filter.clone())
        .and(net_filter.clone())
        .and_then(handle_withdraw);

    let cors = warp::cors()
        .allow_origin("https://god-sniper-pro.netlify.app")
        .allow_methods(vec!["GET", "POST"])
        .allow_headers(vec!["content-type"]);

    let routes = status.or(trade).or(withdraw).with(cors);

    info!("🌍 API Server: Ready (Port 3000)");
    warp::serve(routes).run(([0, 0, 0, 0], 3000)).await;
}

// --- HANDLERS ---

async fn handle_status(
    pool: sqlx::SqlitePool,
    net: Arc<network::NetworkClient>,
    state: Arc<AppState>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let user_id = "admin";

    let pubkey_str = wallet_manager::create_user_wallet(&pool, user_id)
        .await
        .unwrap_or_default();

    let mut balance = 0.0;
    if let Ok(pk) = Pubkey::from_str(&pubkey_str) {
        balance = net.get_balance_fast(&pk).await as f64 / LAMPORTS_PER_SOL as f64;
    }

    let gems = state.found_gems.lock().unwrap().clone();
    let signals = state.math_signals.lock().unwrap().clone();

    let active_trades = match db::get_open_trades(&pool).await {
        Ok(t) => t.len(),
        Err(_) => 0,
    };

    Ok(warp::reply::json(&DashboardData {
        wallet_address: pubkey_str,
        balance_sol: balance,
        active_trades_count: active_trades,
        system_status: "ONLINE".to_string(),
        gems_feed: gems,
        signals_feed: signals,
    }))
}

async fn handle_trade(
    req: TradeRequest,
    pool: sqlx::SqlitePool,
    net: Arc<network::NetworkClient>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let user_id = "admin";
    info!("📨 Trade Request: {} {} SOL -> {}", req.action, req.amount_sol, req.token);

    let payer = match wallet_manager::get_decrypted_wallet(&pool, user_id).await {
        Ok(k) => k,
        Err(e) => {
            error!("❌ Errore recupero wallet: {}", e);
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Wallet Error".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    let bal = net.get_balance_fast(&payer.pubkey()).await;
    let amount_lamports = (req.amount_sol * LAMPORTS_PER_SOL as f64) as u64;

    if req.action == "BUY" {
        if bal < (amount_lamports + 5000) {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Fondi Insufficienti".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }

        // JUPITER SWAP (Priority)
        let input = "So11111111111111111111111111111111111111112"; // SOL
        match jupiter::get_jupiter_swap_tx(&payer.pubkey().to_string(), input, &req.token, amount_lamports, 100).await {
            Ok(mut tx) => {
                if let Ok(bh) = net.rpc.get_latest_blockhash().await {
                    tx.sign(&[&payer], bh);
                    if let Ok(sig) = net.rpc.send_transaction(&tx).await {
                        let sig_str = sig.to_string();
                        let _ = db::record_buy(&pool, user_id, &req.token, &sig_str, amount_lamports).await;
                        return Ok(warp::reply::json(&ApiResponse {
                            success: true,
                            message: "Buy Eseguito (Jupiter)".into(),
                            tx_signature: sig_str.clone(),
                            solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig_str)),
                        }));
                    }
                }
            }
            Err(_) => {
                // RAYDIUM FALLBACK
                if let Ok(mint) = Pubkey::from_str(&req.token) {
                    if let Ok(keys) = raydium::fetch_pool_keys_by_mint(&net, &mint).await {
                        if let Ok(sig) = raydium::execute_swap(&net, &payer, &keys, mint, amount_lamports, 0).await {
                            let _ = db::record_buy(&pool, user_id, &req.token, &sig, amount_lamports).await;
                            return Ok(warp::reply::json(&ApiResponse {
                                success: true,
                                message: "Buy Eseguito (Raydium)".into(),
                                tx_signature: sig.clone(),
                                solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
                            }));
                        }
                    }
                }
            }
        }
    } else if req.action == "SELL" {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Funzione Sell Manuale in arrivo. Usa Jupiter DApp per vendere ora.".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    Ok(warp::reply::json(&ApiResponse {
        success: false,
        message: "Errore generico".into(),
        tx_signature: "".into(),
        solscan_url: None,
    }))
}

async fn handle_withdraw(
    req: WithdrawRequest,
    pool: sqlx::SqlitePool,
    net: Arc<network::NetworkClient>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let user_id = "admin";
    info!("💸 Withdraw Request: {} {} -> {}", req.amount, req.token, req.destination_address);

    // 1. Sicurezza: Solo SOL
    if req.token != "SOL" {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Per sicurezza, preleva solo SOL. Converti gli altri token prima.".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    // 2. Check Blocco 24h
    match db::can_withdraw(&pool, user_id).await {
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
            error!("❌ Errore DB can_withdraw: {}", e);
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Errore verifica stato prelievo".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    }

    // 3. Recupera wallet
    let payer = match wallet_manager::get_decrypted_wallet(&pool, user_id).await {
        Ok(k) => k,
        Err(e) => {
            error!("❌ Errore recupero wallet: {}", e);
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Errore accesso wallet".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    let bal = net.get_balance_fast(&payer.pubkey()).await;
    let amount = (req.amount * LAMPORTS_PER_SOL as f64) as u64;

    // 4. Check Fondi
    if bal < (amount + 5000) {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Fondi Insufficienti (Lascia 0.005 SOL per le fee)".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));
    }

    // 5. Valida indirizzo destinazione
    let dest = match Pubkey::from_str(&req.destination_address) {
        Ok(pk) => pk,
        Err(_) => {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Indirizzo destinazione non valido".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    // 6. Registra richiesta prelievo PRIMA di inviare (Crash Protection)
    let withdrawal_id = match db::record_withdrawal_request(&pool, user_id, amount, &req.destination_address).await {
        Ok(id) => id,
        Err(e) => {
            error!("❌ Errore registrazione prelievo nel DB: {}", e);
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Errore registrazione prelievo".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };
    info!("📝 Prelievo registrato con ID: {}", withdrawal_id);

    // 7. Prepara transazione CON PRIORITY FEES per velocità massima
    let mut instructions = vec![
        // Priority fee: 500k microlamports (~0.0005 SOL) per saltare la coda
        ComputeBudgetInstruction::set_compute_unit_price(500_000),
        ComputeBudgetInstruction::set_compute_unit_limit(50_000),
        // Transfer effettivo
        system_instruction::transfer(&payer.pubkey(), &dest, amount),
    ];

    let bh = match net.rpc.get_latest_blockhash().await {
        Ok(hash) => hash,
        Err(e) => {
            error!("❌ Errore get_latest_blockhash: {}", e);
            let _ = db::mark_withdrawal_failed(&pool, withdrawal_id).await;
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Errore rete Solana".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }
    };

    let tx = Transaction::new_signed_with_payer(&instructions, Some(&payer.pubkey()), &[&payer], bh);

    // 8. Invia transazione (TPU first, poi RPC fallback)
    match net.send_transaction_fast(&tx).await {
        Ok(sig) => {
            // ✅ Conferma prelievo con l'ID corretto
            let _ = db::confirm_withdrawal(&pool, withdrawal_id, &sig).await;
            let solscan_link = format!("{}{}", SOLSCAN_TX_URL, sig);
            info!("✅ Prelievo completato: {} (ID: {})", sig, withdrawal_id);
            Ok(warp::reply::json(&ApiResponse {
                success: true,
                message: format!("Prelievo Inviato! Vedi su Solscan"),
                tx_signature: sig,
                solscan_url: Some(solscan_link),
            }))
        }
        Err(e) => {
            error!("❌ Errore invio transazione prelievo: {}", e);
            // ❌ Marca come fallito nel DB
            let _ = db::mark_withdrawal_failed(&pool, withdrawal_id).await;
            Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: "Errore invio transazione".into(),
                tx_signature: "".into(),
                solscan_url: None,
            }))
        }
    }
}
