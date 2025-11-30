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

/// Ottiene il prezzo SOL in tempo reale da Jupiter
async fn get_sol_price() -> f64 {
    match jupiter::get_token_market_data("So11111111111111111111111111111111111111112").await {
        Ok(data) => data.price,
        Err(_) => {
            // Fallback: prova CoinGecko
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

// --- SERVER ---
pub async fn start_server(pool: sqlx::SqlitePool, net: Arc<network::NetworkClient>, state: Arc<AppState>) {
    let pool_filter = warp::any().map(move || pool.clone());
    let net_filter = warp::any().map(move || net.clone());
    let state_filter = warp::any().map(move || state.clone());

    // Health check endpoint
    let health = warp::path("health")
        .and(warp::get())
        .map(|| warp::reply::json(&serde_json::json!({"status": "ok"})));

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

    // CORS - allow_any_origin per massima compatibilità in produzione
    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec!["GET", "POST", "PUT", "DELETE", "OPTIONS"])
        .allow_headers(vec![
            "Content-Type", 
            "content-type",
            "Authorization", 
            "authorization",
            "X-User-Id",
            "x-user-id",
            "Accept",
            "Origin",
            "Access-Control-Request-Method",
            "Access-Control-Request-Headers",
        ])
        .max_age(86400); // Cache preflight per 24h

    let routes = health
        .or(status)
        .or(trade)
        .or(withdraw)
        .with(cors)
        .with(warp::log("api"));

    info!("🌍 API Server LIVE: Porta 3000");
    info!("   ✓ CORS: any origin");
    info!("   ✓ Endpoints: /health, /status, /trade, /withdraw");
    
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

    // Prezzo SOL in tempo reale
    let sol_price = get_sol_price().await;
    let balance_usd = balance * sol_price;

    // Calcola livello ricchezza basato su EUR (SOL ~= USD per semplicità)
    let balance_eur = balance_usd * 0.92; // Conversione approssimativa
    let wealth_level = if balance_eur < 5.0 {
        "MICRO".to_string()
    } else if balance_eur < 15.0 {
        "POOR".to_string()
    } else if balance_eur < 50.0 {
        "LOW_MEDIUM".to_string()
    } else if balance_eur < 100.0 {
        "MEDIUM".to_string()
    } else if balance_eur < 200.0 {
        "HIGH_MEDIUM".to_string()
    } else {
        "RICH".to_string()
    };

    let mut gems = state.found_gems.lock().unwrap().clone();
    gems.sort_by(|a, b| b.safety_score.cmp(&a.safety_score));
    
    let signals = state.math_signals.lock().unwrap().clone();

    let active_trades = match db::get_open_trades(&pool).await {
        Ok(t) => t.len(),
        Err(_) => 0,
    };

    let (trades_history, withdrawals_history) = match db::get_all_history(&pool, user_id).await {
        Ok((t, w)) => (t, w),
        Err(_) => (vec![], vec![]),
    };

    Ok(warp::reply::json(&DashboardData {
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
        // Verifica fondi con margine per fee
        let min_required = amount_lamports + 10_000; // 0.00001 SOL per fee
        if bal < min_required {
            return Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: format!("Fondi Insufficienti. Hai {:.4} SOL, servono {:.4} SOL", 
                    bal as f64 / LAMPORTS_PER_SOL as f64,
                    min_required as f64 / LAMPORTS_PER_SOL as f64),
                tx_signature: "".into(),
                solscan_url: None,
            }));
        }

        // JUPITER SWAP (Priority) - 1% slippage
        let input = "So11111111111111111111111111111111111111112";
        match jupiter::get_jupiter_swap_tx(&payer.pubkey().to_string(), input, &req.token, amount_lamports, 100).await {
            Ok(mut tx) => {
                if let Ok(bh) = net.rpc.get_latest_blockhash().await {
                    tx.sign(&[&payer], bh);
                    
                    // Usa TPU per velocità massima
                    match net.send_transaction_fast(&tx).await {
                        Ok(sig) => {
                            let _ = db::record_buy(&pool, user_id, &req.token, &sig, amount_lamports).await;
                            info!("✅ BUY completato via Jupiter: {}", sig);
                            return Ok(warp::reply::json(&ApiResponse {
                                success: true,
                                message: "Buy Eseguito (Jupiter)".into(),
                                tx_signature: sig.clone(),
                                solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
                            }));
                        }
                        Err(e) => {
                            warn!("⚠️ Jupiter TX fallita: {}", e);
                        }
                    }
                }
            }
            Err(e) => {
                warn!("⚠️ Jupiter quote fallita: {}, provo Raydium...", e);
            }
        }

        // RAYDIUM FALLBACK - 2% slippage
        if let Ok(mint) = Pubkey::from_str(&req.token) {
            if let Ok(keys) = raydium::fetch_pool_keys_by_mint(&net, &mint).await {
                if let Ok(sig) = raydium::execute_swap(&net, &payer, &keys, mint, amount_lamports, 200).await {
                    let _ = db::record_buy(&pool, user_id, &req.token, &sig, amount_lamports).await;
                    info!("✅ BUY completato via Raydium: {}", sig);
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
            message: "Trade fallito. Token non trovato o liquidità insufficiente.".into(),
            tx_signature: "".into(),
            solscan_url: None,
        }));

    } else if req.action == "SELL" {
        // SELL via Jupiter
        let output = "So11111111111111111111111111111111111111112"; // SOL
        match jupiter::get_jupiter_swap_tx(&payer.pubkey().to_string(), &req.token, output, amount_lamports, 200).await {
            Ok(mut tx) => {
                if let Ok(bh) = net.rpc.get_latest_blockhash().await {
                    tx.sign(&[&payer], bh);
                    
                    match net.send_transaction_fast(&tx).await {
                        Ok(sig) => {
                            let _ = db::record_sell(&pool, user_id, &req.token, &sig, 0.0).await;
                            info!("✅ SELL completato: {}", sig);
                            return Ok(warp::reply::json(&ApiResponse {
                                success: true,
                                message: "Vendita Eseguita".into(),
                                tx_signature: sig.clone(),
                                solscan_url: Some(format!("{}{}", SOLSCAN_TX_URL, sig)),
                            }));
                        }
                        Err(e) => {
                            error!("❌ Sell TX fallita: {}", e);
                        }
                    }
                }
            }
            Err(e) => {
                error!("❌ Jupiter sell quote fallita: {}", e);
            }
        }

        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: "Vendita fallita. Prova su Jupiter DApp.".into(),
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

    // 4. Check Fondi (lascia margine per fee)
    let fee_reserve = 10_000; // 0.00001 SOL
    if bal < (amount + fee_reserve) {
        return Ok(warp::reply::json(&ApiResponse {
            success: false,
            message: format!("Fondi Insufficienti. Disponibili: {:.4} SOL", bal as f64 / LAMPORTS_PER_SOL as f64),
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

    // 7. Prepara transazione CON PRIORITY FEES
    let instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(100_000), // Priority fee
        ComputeBudgetInstruction::set_compute_unit_limit(50_000),
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

    // 8. Invia via TPU (più veloce) con fallback RPC
    match net.send_transaction_fast(&tx).await {
        Ok(sig) => {
            let _ = db::confirm_withdrawal(&pool, withdrawal_id, &sig).await;
            let solscan_link = format!("{}{}", SOLSCAN_TX_URL, sig);
            info!("✅ Prelievo completato: {} (ID: {})", sig, withdrawal_id);
            Ok(warp::reply::json(&ApiResponse {
                success: true,
                message: "Prelievo Inviato!".into(),
                tx_signature: sig,
                solscan_url: Some(solscan_link),
            }))
        }
        Err(e) => {
            error!("❌ Errore invio transazione prelievo: {}", e);
            let _ = db::mark_withdrawal_failed(&pool, withdrawal_id).await;
            Ok(warp::reply::json(&ApiResponse {
                success: false,
                message: format!("Errore invio: {}", e),
                tx_signature: "".into(),
                solscan_url: None,
            }))
        }
    }
}
