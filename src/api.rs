use warp::Filter;
use warp::reply::Response;
use warp::http::StatusCode;
use std::sync::{Arc, Mutex};
use serde::{Serialize, Deserialize};
use serde_json::json;
use crate::{db, network, wallet_manager, raydium, jupiter, AppState, GemData};
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::system_instruction;
use solana_sdk::transaction::Transaction;
use solana_sdk::signer::Signer;
use std::str::FromStr;
use log::{info, error};

// --- DATI ---
#[derive(Serialize, Clone)]
pub struct SignalData {
    pub token: String, pub price: f64, pub score: u8, pub reason: String, pub timestamp: i64,
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
struct TradeRequest { action: String, token: String, amount_sol: f64 }

#[derive(Deserialize)]
struct WithdrawRequest { amount: f64, token: String, destination_address: String }

#[derive(Serialize)]
struct ApiResponse { success: bool, message: String, tx_signature: String }

// --- SERVER ---
pub async fn start_server(pool: sqlx::SqlitePool, net: Arc<network::NetworkClient>, state: Arc<AppState>) {
    let pf = warp::any().map(move || pool.clone());
    let nf = warp::any().map(move || net.clone());
    let sf = warp::any().map(move || state.clone());

    let user = warp::header::header::<String>("x-user-id");

    let status = warp::path("status")
        .and(warp::get())
        .and(user.clone())
        .and(pf.clone())
        .and(nf.clone())
        .and(sf.clone())
        .and_then(handle_status);

    let trade = warp::path("trade")
        .and(warp::post())
        .and(user.clone())
        .and(warp::body::json())
        .and(pf.clone())
        .and(nf.clone())
        .and_then(handle_trade);

    let withdraw = warp::path("withdraw")
        .and(warp::post())
        .and(user.clone())
        .and(warp::body::json())
        .and(pf.clone())
        .and(nf.clone())
        .and_then(handle_withdraw);

    let cors = warp::cors()
        .allow_origin("https://god-sniper-pro.netlify.app")
        .allow_methods(vec!["GET", "POST"])
        .allow_headers(vec!["content-type", "x-user-id"]);
    let routes = status.or(trade).or(withdraw).with(cors);
    
    info!("🌍 API Server: Ready (Port 3000)");
    warp::serve(routes).run(([0, 0, 0, 0], 3000)).await;
}

// --- HANDLERS ---

async fn handle_status(user_id: String, pool: sqlx::SqlitePool, net: Arc<network::NetworkClient>, state: Arc<AppState>) -> Result<Response, warp::Rejection> {
    let pubkey_str = match wallet_manager::create_user_wallet(&pool, &user_id).await {
        Ok(pk) => pk,
        Err(e) => {
            error!("wallet creation failed for {}: {}", user_id, e);
            let body = json!({
                "success": false,
                "message": "WALLET_INIT_FAILED"
            });
            return Ok(warp::reply::with_status(warp::reply::json(&body), StatusCode::INTERNAL_SERVER_ERROR).into_response());
        }
    };
    
    let mut balance = 0.0;
    if let Ok(pk) = Pubkey::from_str(&pubkey_str) {
        balance = net.get_balance_fast(&pk).await as f64 / LAMPORTS_PER_SOL as f64;
    }

    let gems = state.found_gems.lock().unwrap().clone();
    let signals = state.math_signals.lock().unwrap().clone(); 
    
    // Conteggio reale posizioni aperte
    let active_trades = match db::count_open_trades(&pool, &user_id).await { Ok(c) => c, Err(_) => 0 };
    
    Ok(warp::reply::json(&DashboardData {
        wallet_address: pubkey_str,
        balance_sol: balance,
        active_trades_count: active_trades, 
        system_status: "ONLINE".to_string(),
        gems_feed: gems,
        signals_feed: signals,
    }).into_response())
}

async fn handle_trade(user_id: String, req: TradeRequest, pool: sqlx::SqlitePool, net: Arc<network::NetworkClient>) -> Result<Response, warp::Rejection> {
    info!("📨 Trade Request [{}]: {} {} SOL -> {}", user_id, req.action, req.amount_sol, req.token);

    let payer = match wallet_manager::get_decrypted_wallet(&pool, &user_id).await {
        Ok(k) => k,
        Err(_) => {
            return Ok(warp::reply::json(&ApiResponse { success: false, message: "Wallet Error".into(), tx_signature: "".into() }).into_response());
        }
    };

    let bal = net.get_balance_fast(&payer.pubkey()).await;
    let amount_lamports = (req.amount_sol * LAMPORTS_PER_SOL as f64) as u64;

    if req.action == "BUY" {
        if bal < (amount_lamports + 5000) {
            return Ok(warp::reply::json(&ApiResponse { success: false, message: "Fondi Insufficienti".into(), tx_signature: "".into() }).into_response());
        }
        
        // JUPITER SWAP (Priority)
        let input = "So11111111111111111111111111111111111111112"; // SOL
        match jupiter::get_jupiter_swap_tx(&payer.pubkey().to_string(), input, &req.token, amount_lamports, 100).await {
            Ok(mut tx) => {
                let bh = net.rpc.get_latest_blockhash().await.unwrap();
                tx.sign(&[&payer], bh);
                if let Ok(sig) = net.rpc.send_transaction(&tx).await {
                    let _ = db::record_buy(&pool, &user_id, &req.token, &sig.to_string(), amount_lamports).await;
                    return Ok(warp::reply::json(&ApiResponse { success: true, message: "Buy Eseguito (Jupiter)".into(), tx_signature: sig.to_string() }).into_response());
                }
            },
            Err(_) => {
                // RAYDIUM FALLBACK
                if let Ok(mint) = Pubkey::from_str(&req.token) {
                     if let Ok(keys) = raydium::fetch_pool_keys_by_mint(&net, &mint).await {
                         if let Ok(sig) = raydium::execute_swap(&net, &payer, &keys, mint, amount_lamports, 0).await {
                             let _ = db::record_buy(&pool, &user_id, &req.token, &sig, amount_lamports).await;
                             return Ok(warp::reply::json(&ApiResponse { success: true, message: "Buy Eseguito (Raydium)".into(), tx_signature: sig }).into_response());
                         }
                     }
                }
            }
        }
    } else if req.action == "SELL" {
        // Logica di vendita (Per ora placeholder, ma sicura)
        return Ok(warp::reply::json(&ApiResponse { success: false, message: "Funzione Sell Manuale in arrivo. Usa Jupiter DApp per vendere ora.".into(), tx_signature: "".into() }).into_response());
    }
    
    Ok(warp::reply::json(&ApiResponse { success: false, message: "Errore generico".into(), tx_signature: "".into() }).into_response())
}

async fn handle_withdraw(user_id: String, req: WithdrawRequest, pool: sqlx::SqlitePool, net: Arc<network::NetworkClient>) -> Result<Response, warp::Rejection> {
    
    // 1. Sicurezza: Solo SOL
    if req.token != "SOL" {
         return Ok(warp::reply::json(&ApiResponse { success: false, message: "Per sicurezza, preleva solo SOL. Converti gli altri token prima.".into(), tx_signature: "".into() }).into_response());
    }

    // 2. Check Blocco 24h
    if let Ok((allowed, msg)) = db::can_withdraw(&pool, &user_id).await {
        if !allowed { return Ok(warp::reply::json(&ApiResponse { success: false, message: msg, tx_signature: "".into() }).into_response()); }
    }

    let payer = match wallet_manager::get_decrypted_wallet(&pool, &user_id).await {
        Ok(p) => p,
        Err(_) => return Ok(warp::reply::json(&ApiResponse { success: false, message: "Wallet Error".into(), tx_signature: "".into() }).into_response()),
    };
    let bal = net.get_balance_fast(&payer.pubkey()).await;
    let amount = (req.amount * LAMPORTS_PER_SOL as f64) as u64;

    // 3. Check Fondi
    if bal < (amount + 5000) { 
        return Ok(warp::reply::json(&ApiResponse { success: false, message: "Fondi Insufficienti (Lascia 0.005 SOL per le fee)".into(), tx_signature: "".into() }).into_response()); 
    }

    // 4. Esegui
    if let Ok(dest) = Pubkey::from_str(&req.destination_address) {
        let ix = system_instruction::transfer(&payer.pubkey(), &dest, amount);
        let bh = net.rpc.get_latest_blockhash().await.unwrap();
        let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], bh);
        
        if let Ok(sig) = net.rpc.send_transaction(&tx).await {
             let _ = db::record_withdrawal_request(&pool, &user_id, amount, &req.destination_address).await;
             let _ = db::confirm_withdrawal(&pool, 0, &sig.to_string()).await;
             return Ok(warp::reply::json(&ApiResponse { success: true, message: "Prelievo Inviato!".into(), tx_signature: sig.to_string() }).into_response());
        }
    }

    Ok(warp::reply::json(&ApiResponse { success: false, message: "Indirizzo Invalido o Errore Rete".into(), tx_signature: "".into() }).into_response())
}