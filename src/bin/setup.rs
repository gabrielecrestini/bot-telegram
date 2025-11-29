use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, Executor};
use std::str::FromStr;
use tokio;

#[tokio::main]
async fn main() {
    // URL diretto (assicurati corrisponda al tuo .env o incollalo qui)
    let db_url = "sqlite://god_sniper.db"; 

    println!("🛠️  RESET DATABASE IN CORSO...");

    let connection_options = SqliteConnectOptions::from_str(db_url)
        .unwrap()
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);

    let pool = SqlitePoolOptions::new()
        .connect_with(connection_options)
        .await
        .unwrap();

    // CREAZIONE TABELLE (Query Grezza non controllata dal compilatore)
    let schema = r#"
    CREATE TABLE IF NOT EXISTS users (
        tg_id TEXT PRIMARY KEY,
        pubkey TEXT NOT NULL,
        private_key_enc TEXT NOT NULL,
        is_active INTEGER DEFAULT 0,
        bot_started_at DATETIME,
        created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
        settings TEXT
    );

    CREATE TABLE IF NOT EXISTS trades (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id TEXT NOT NULL,
        token_address TEXT NOT NULL,
        tx_signature TEXT NOT NULL,
        amount_in_lamports INTEGER NOT NULL,
        status TEXT DEFAULT 'OPEN',
        entry_time DATETIME DEFAULT CURRENT_TIMESTAMP,
        exit_time DATETIME,
        profit_loss_sol REAL DEFAULT 0.0
    );
    "#;

    pool.execute(schema).await.unwrap();
    println!("✅ Database rigenerato con successo! Ora puoi compilare.");
}