use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteJournalMode};
use sqlx::{SqlitePool, ConnectOptions, Row};
use std::env;
use std::str::FromStr;
use std::fs;
use std::path::Path;
use log::{info, warn, error};
use chrono::{Utc, Duration, DateTime};

/// Connette al DB con Backup di Sicurezza e WAL Mode
pub async fn connect() -> SqlitePool {
    let db_url = env::var("DATABASE_URL").expect("❌ Manca DATABASE_URL nel file .env");
    
    // --- 1. BACKUP DI SICUREZZA AUTOMATICO ---
    if let Some(path_str) = db_url.strip_prefix("sqlite://") {
        let path = Path::new(path_str);
        if path.exists() {
            let backup_path = format!("{}.bak", path_str);
            // Ignoriamo errori di copia per non bloccare l'avvio se il file è lockato
            let _ = fs::copy(path, &backup_path); 
        }
    }

    info!("🗄️  Connessione al Database...");

    // --- 2. CONFIGURAZIONE ROBUSTA (WAL) ---
    let mut connection_options = SqliteConnectOptions::from_str(&db_url)
        .expect("URL Database non valido")
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
        
    connection_options = connection_options.log_statements(log::LevelFilter::Off);

    let pool = SqlitePoolOptions::new()
        .max_connections(10)
        .connect_with(connection_options)
        .await
        .expect("❌ Impossibile connettersi a SQLite");

    init_schema(&pool).await;
    pool
}

/// Crea o Aggiorna lo Schema delle Tabelle
async fn init_schema(pool: &SqlitePool) {
    // Tabella UTENTI
    let schema_users = r#"
    CREATE TABLE IF NOT EXISTS users (
        tg_id TEXT PRIMARY KEY,
        pubkey TEXT NOT NULL,
        private_key_enc TEXT NOT NULL,
        is_active INTEGER DEFAULT 0,
        bot_started_at TEXT, 
        created_at TEXT DEFAULT CURRENT_TIMESTAMP,
        settings TEXT
    );
    "#;

    // Tabella TRADES (Con highest_price per Trailing Stop + trading_mode)
    let schema_trades = r#"
    CREATE TABLE IF NOT EXISTS trades (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id TEXT NOT NULL,
        token_address TEXT NOT NULL,
        tx_signature TEXT NOT NULL,
        amount_in_lamports INTEGER NOT NULL,
        status TEXT DEFAULT 'OPEN', -- OPEN, SOLD, FAILED
        entry_time TEXT DEFAULT CURRENT_TIMESTAMP,
        exit_time TEXT,
        profit_loss_sol REAL DEFAULT 0.0,
        highest_price_lamports INTEGER DEFAULT 0,
        trading_mode TEXT DEFAULT 'AUTO' -- DIP, BREAKOUT, AUTO
    );
    "#;

    // Tabella PRELIEVI (Per gestire i crash durante i prelievi)
    let schema_withdrawals = r#"
    CREATE TABLE IF NOT EXISTS withdrawals (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id TEXT NOT NULL,
        amount_lamports INTEGER NOT NULL,
        destination TEXT NOT NULL,
        status TEXT DEFAULT 'PENDING', -- PENDING, COMPLETED, FAILED
        tx_signature TEXT,
        created_at TEXT DEFAULT CURRENT_TIMESTAMP
    );
    "#;

    // Eseguiamo le query singolarmente per gestire errori specifici
    if let Err(e) = sqlx::query(schema_users).execute(pool).await {
        error!("❌ Errore Critico Tabella USERS: {}", e);
    }
    if let Err(e) = sqlx::query(schema_trades).execute(pool).await {
        error!("❌ Errore Critico Tabella TRADES: {}", e);
    }
    if let Err(e) = sqlx::query(schema_withdrawals).execute(pool).await {
        error!("❌ Errore Critico Tabella WITHDRAWALS: {}", e);
    }
    
    // ═══════════════════════════════════════════════════════════════
    // MIGRAZIONI - Aggiunge colonne mancanti a tabelle esistenti
    // ═══════════════════════════════════════════════════════════════
    
    // Migrazione: Aggiungi trading_mode se non esiste
    let _ = sqlx::query("ALTER TABLE trades ADD COLUMN trading_mode TEXT DEFAULT 'AUTO'")
        .execute(pool)
        .await; // Ignora errore se colonna esiste già
    
    // Migrazione: Aggiungi settings a users se non esiste
    let _ = sqlx::query("ALTER TABLE users ADD COLUMN settings TEXT")
        .execute(pool)
        .await;
    
    info!("✅ Schema Database verificato (Full Features + Migrazioni).");
}

// --- FUNZIONI OPERATIVE (Tutte PUBBLICHE) ---

/// Avvia il ciclo di 24h per l'utente
pub async fn start_daily_cycle(pool: &SqlitePool, tg_id: &str) -> Result<(), sqlx::Error> {
    let now_str = Utc::now().to_rfc3339(); 
    
    sqlx::query("UPDATE users SET is_active = 1, bot_started_at = ? WHERE tg_id = ?")
        .bind(now_str)
        .bind(tg_id)
        .execute(pool)
        .await?;
        
    info!("🕒 Ciclo giornaliero avviato per {}", tg_id);
    Ok(())
}

/// Controlla se è possibile prelevare (Blocco 24h)
pub async fn can_withdraw(pool: &SqlitePool, tg_id: &str) -> Result<(bool, String), sqlx::Error> {
    let row_opt = sqlx::query("SELECT bot_started_at, is_active FROM users WHERE tg_id = ?")
        .bind(tg_id)
        .fetch_optional(pool)
        .await?;

    if let Some(row) = row_opt {
        let is_active: i32 = row.try_get("is_active").unwrap_or(0);
        
        if is_active == 0 {
            return Ok((true, "Prelievo consentito".to_string()));
        }

        let start_time_str: Option<String> = row.try_get("bot_started_at").ok();

        if let Some(s) = start_time_str {
            if let Ok(start_time) = DateTime::parse_from_rfc3339(&s) {
                let start_utc = start_time.with_timezone(&Utc);
                let unlock_time = start_utc + Duration::hours(24);
                let now = Utc::now();

                if now < unlock_time {
                    let remaining = unlock_time - now;
                    let msg = format!(
                        "⚠️ **Prelievo Bloccato!**\nIl Bot sta lavorando.\nTempo rimanente: {} ore e {} minuti.", 
                        remaining.num_hours(), remaining.num_minutes() % 60
                    );
                    return Ok((false, msg));
                }
            }
        }
    }
    
    Ok((true, "✅ Prelievo sbloccato!".to_string()))
}

/// Registra un acquisto (Buy) con modalità trading
pub async fn record_buy(
    pool: &SqlitePool, 
    tg_id: &str, 
    token_addr: &str, 
    signature: &str, 
    amount: u64
) -> Result<(), sqlx::Error> {
    record_buy_with_mode(pool, tg_id, token_addr, signature, amount, "AUTO").await
}

/// Registra un acquisto con modalità specifica (DIP, BREAKOUT, AUTO)
pub async fn record_buy_with_mode(
    pool: &SqlitePool, 
    tg_id: &str, 
    token_addr: &str, 
    signature: &str, 
    amount: u64,
    mode: &str
) -> Result<(), sqlx::Error> {
    let amount_i64 = amount as i64;
    // All'inizio, il prezzo più alto (highest) è uguale al prezzo di entrata
    sqlx::query("INSERT INTO trades (user_id, token_address, tx_signature, amount_in_lamports, highest_price_lamports, trading_mode, status) VALUES (?, ?, ?, ?, ?, ?, 'OPEN')")
        .bind(tg_id)
        .bind(token_addr)
        .bind(signature)
        .bind(amount_i64)
        .bind(amount_i64)
        .bind(mode)
        .execute(pool)
        .await?;
        
    info!("📝 Trade {} registrato nel DB per {}", mode, token_addr);
    Ok(())
}

/// Aggiorna il prezzo massimo raggiunto (Trailing Stop)
pub async fn update_highest_price(pool: &SqlitePool, trade_id: i32, new_high: u64) {
    let _ = sqlx::query("UPDATE trades SET highest_price_lamports = ? WHERE id = ?")
        .bind(new_high as i64)
        .bind(trade_id)
        .execute(pool)
        .await;
}

/// Registra una vendita (Sell) - chiude un trade aperto
pub async fn record_sell(
    pool: &SqlitePool, 
    user_id: &str, 
    token_addr: &str, 
    signature: &str,
    pnl_pct: f64
) -> Result<(), sqlx::Error> {
    let exit_time = chrono::Utc::now().to_rfc3339();
    
    // Trova il trade aperto più recente per questo token
    let trade_opt = sqlx::query(
        "SELECT id, amount_in_lamports FROM trades 
         WHERE user_id = ? AND token_address = ? AND status = 'OPEN' 
         ORDER BY entry_time DESC LIMIT 1"
    )
    .bind(user_id)
    .bind(token_addr)
    .fetch_optional(pool)
    .await?;
    
    if let Some(row) = trade_opt {
        let trade_id: i32 = row.get("id");
        let amount_lamports: i64 = row.get("amount_in_lamports");
        let amount_sol = amount_lamports as f64 / 1_000_000_000.0;
        let pnl_sol = amount_sol * (pnl_pct / 100.0);
        
        sqlx::query(
            "UPDATE trades SET status = 'SOLD', exit_time = ?, profit_loss_sol = ?, tx_signature = ? WHERE id = ?"
        )
        .bind(&exit_time)
        .bind(pnl_sol)
        .bind(signature)
        .bind(trade_id)
        .execute(pool)
        .await?;
        
        info!("📝 Trade {} chiuso | PnL: {:+.4} SOL ({:+.1}%)", trade_id, pnl_sol, pnl_pct);
    } else {
        // Se non troviamo un trade aperto, registriamo comunque come nuovo record
        sqlx::query(
            "INSERT INTO trades (user_id, token_address, tx_signature, amount_in_lamports, status, exit_time, profit_loss_sol) 
             VALUES (?, ?, ?, 0, 'SOLD', ?, 0)"
        )
        .bind(user_id)
        .bind(token_addr)
        .bind(signature)
        .bind(&exit_time)
        .execute(pool)
        .await?;
    }
    
    Ok(())
}

/// Registra un prelievo PRIMA di inviarlo (Crash Protection)
pub async fn record_withdrawal_request(pool: &SqlitePool, tg_id: &str, amount: u64, dest: &str) -> Result<i64, sqlx::Error> {
    let id = sqlx::query("INSERT INTO withdrawals (user_id, amount_lamports, destination) VALUES (?, ?, ?)")
        .bind(tg_id)
        .bind(amount as i64)
        .bind(dest)
        .execute(pool)
        .await?
        .last_insert_rowid();
    Ok(id)
}

/// Conferma che il prelievo è avvenuto
pub async fn confirm_withdrawal(pool: &SqlitePool, id: i64, signature: &str) {
    let _ = sqlx::query("UPDATE withdrawals SET status = 'COMPLETED', tx_signature = ? WHERE id = ?")
        .bind(signature)
        .bind(id)
        .execute(pool)
        .await;
}

/// Marca un prelievo come fallito
pub async fn mark_withdrawal_failed(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE withdrawals SET status = 'FAILED' WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Conta i trade aperti per un utente specifico
pub async fn count_open_trades(pool: &SqlitePool, user_id: &str) -> Result<usize, sqlx::Error> {
    let row = sqlx::query("SELECT COUNT(*) as count FROM trades WHERE user_id = ? AND status = 'OPEN'")
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    
    let count: i32 = row.get("count");
    Ok(count as usize)
}

/// Recupera trade aperti (per il ripristino al riavvio)
pub async fn get_open_trades(pool: &SqlitePool) -> Result<Vec<(i32, String, u64, u64)>, sqlx::Error> {
    let rows = sqlx::query("SELECT id, token_address, amount_in_lamports, highest_price_lamports FROM trades WHERE status = 'OPEN'")
        .fetch_all(pool)
        .await?;
    
    let mut results = Vec::new();
    for row in rows {
        let id: i32 = row.get("id");
        let token: String = row.get("token_address");
        let entry: i64 = row.get("amount_in_lamports");
        let high: i64 = row.get("highest_price_lamports");
        results.push((id, token, entry as u64, high as u64));
    }
    Ok(results)
}

// ═══════════════════════════════════════════════════════════════
// STORICO TRANSAZIONI
// ═══════════════════════════════════════════════════════════════

/// Struttura per trade nello storico
#[derive(Debug, Clone, serde::Serialize)]
pub struct TradeHistory {
    pub id: i32,
    pub token_address: String,
    pub tx_signature: String,
    pub amount_sol: f64,
    pub status: String,
    pub entry_time: String,
    pub exit_time: Option<String>,
    pub profit_loss_sol: f64,
    pub trading_mode: String, // DIP, BREAKOUT, AUTO
}

/// Struttura per prelievo nello storico
#[derive(Debug, Clone, serde::Serialize)]
pub struct WithdrawalHistory {
    pub id: i32,
    pub amount_sol: f64,
    pub destination: String,
    pub status: String,
    pub tx_signature: Option<String>,
    pub created_at: String,
}

/// Recupera tutti i trade di un utente (ultimi 50)
pub async fn get_user_trades(pool: &SqlitePool, user_id: &str) -> Result<Vec<TradeHistory>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, token_address, tx_signature, amount_in_lamports, status, entry_time, exit_time, profit_loss_sol, trading_mode 
         FROM trades WHERE user_id = ? ORDER BY entry_time DESC LIMIT 50"
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    
    let mut results = Vec::new();
    for row in rows {
        results.push(TradeHistory {
            id: row.get("id"),
            token_address: row.get("token_address"),
            tx_signature: row.get("tx_signature"),
            amount_sol: row.get::<i64, _>("amount_in_lamports") as f64 / 1_000_000_000.0,
            status: row.get("status"),
            entry_time: row.get("entry_time"),
            exit_time: row.try_get("exit_time").ok(),
            profit_loss_sol: row.try_get("profit_loss_sol").unwrap_or(0.0),
            trading_mode: row.try_get("trading_mode").unwrap_or_else(|_| "AUTO".to_string()),
        });
    }
    Ok(results)
}

/// Recupera tutti i prelievi di un utente (ultimi 50)
pub async fn get_user_withdrawals(pool: &SqlitePool, user_id: &str) -> Result<Vec<WithdrawalHistory>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, amount_lamports, destination, status, tx_signature, created_at 
         FROM withdrawals WHERE user_id = ? ORDER BY created_at DESC LIMIT 50"
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    
    let mut results = Vec::new();
    for row in rows {
        results.push(WithdrawalHistory {
            id: row.get("id"),
            amount_sol: row.get::<i64, _>("amount_lamports") as f64 / 1_000_000_000.0,
            destination: row.get("destination"),
            status: row.get("status"),
            tx_signature: row.try_get("tx_signature").ok(),
            created_at: row.get("created_at"),
        });
    }
    Ok(results)
}

/// Recupera storico combinato (trade + prelievi) ordinato per data
pub async fn get_all_history(pool: &SqlitePool, user_id: &str) -> Result<(Vec<TradeHistory>, Vec<WithdrawalHistory>), sqlx::Error> {
    let trades = get_user_trades(pool, user_id).await?;
    let withdrawals = get_user_withdrawals(pool, user_id).await?;
    Ok((trades, withdrawals))
}