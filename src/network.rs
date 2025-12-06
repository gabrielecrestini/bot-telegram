use solana_client::nonblocking::rpc_client::RpcClient as AsyncRpcClient;
use solana_client::rpc_client::RpcClient as BlockingRpcClient;
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::tpu_client::{TpuClient, TpuClientConfig};
use solana_quic_client::{QuicPool, QuicConnectionManager, QuicConfig}; 
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use std::env;
use std::time::Duration;
use log::{info, warn, error};

pub struct NetworkClient {
    // Usiamo questo ASINCRONO per leggere saldo, dati token, ecc. (Veloce)
    pub rpc: Arc<AsyncRpcClient>, 
    // WebSocket per ascoltare aggiornamenti in tempo reale
    pub pubsub: PubsubClient, 
    // Il cannone QUIC (Nota i 3 Generics specificati per placare il compilatore)
    pub tpu: TpuClient<QuicPool, QuicConnectionManager, QuicConfig>, 
}

pub async fn init_clients() -> NetworkClient {
    let rpc_url = env::var("RPC_URL").expect("Manca RPC_URL");
    let ws_url = env::var("WS_URL").expect("Manca WS_URL");

    info!("🔌 Init Network Layer...");

    // 1. Setup Client ASINCRONO (Il motore principale della tua app)
    let async_rpc = Arc::new(AsyncRpcClient::new_with_commitment(
        rpc_url.clone(),
        CommitmentConfig::processed(), 
    ));

    // 2. Setup Client BLOCCANTE (Il "motorino di avviamento" per il TPU)
    // Lo creiamo qui, lo usiamo una volta e poi verrà buttato via dalla memoria automaticamente
    let blocking_rpc = Arc::new(BlockingRpcClient::new_with_commitment(
        rpc_url.clone(),
        CommitmentConfig::processed(),
    ));

    // 3. Setup WebSocket (WSS)
    info!("🎧 Connessione WSS: {}", ws_url);
    let pubsub_client = PubsubClient::new(&ws_url)
        .await
        .expect("❌ Errore critico WSS");

    // 4. Setup QUIC (TPU)
    info!("🔫 Caricamento Cannone QUIC...");
    
    // TpuClient::new è una funzione vecchia maniera (sincrona/bloccante)
    // Ecco perché vuole 'blocking_rpc'.
    let tpu_client = TpuClient::new(
        blocking_rpc, // Passiamo il client bloccante solo qui
        &ws_url,
        TpuClientConfig::default(),
    )
    .expect("❌ Errore creazione TPU"); // Rimuovi .await perché è sincrono!

    // Restituiamo la struttura pronta
    NetworkClient {
        rpc: async_rpc,
        pubsub: pubsub_client,
        tpu: tpu_client,
    }
}

impl NetworkClient {
    /// Metodo helper per ottenere il saldo velocemente usando il client asincrono
    pub async fn get_balance_fast(&self, pubkey: &Pubkey) -> u64 {
        self.rpc.get_balance(pubkey).await.unwrap_or(0)
    }

    /// Invia transazione via TPU (QUIC) per massima velocità - salta la mempool pubblica
    pub fn send_via_tpu(&self, transaction: &solana_sdk::transaction::Transaction) -> bool {
        self.tpu.send_transaction(transaction)
    }

    /// Invia transazione con retry automatico (TPU first, RPC fallback)
    pub async fn send_transaction_fast(&self, transaction: &solana_sdk::transaction::Transaction) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let sig = transaction.signatures[0].to_string();
        
        // Prima prova TPU (più veloce, salta mempool)
        if self.tpu.send_transaction(transaction) {
            info!("⚡ TX inviata via TPU/QUIC: {}", sig);
            return Ok(sig);
        }
        
        // Fallback RPC standard
        match self.rpc.send_transaction(transaction).await {
            Ok(s) => {
                info!("📡 TX inviata via RPC: {}", s);
                Ok(s.to_string())
            }
            Err(e) => Err(format!("Errore invio TX: {}", e).into())
        }
    }
    
    /// Invia VersionedTransaction (Jupiter V6) con retry automatico
    /// Include retry robusto per gestire problemi DNS su AWS
    pub async fn send_versioned_transaction(&self, transaction: &solana_sdk::transaction::VersionedTransaction) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let _sig = transaction.signatures[0].to_string();
        let mut last_error = String::new();
        
        // Retry con backoff esponenziale (gestisce problemi DNS su AWS)
        for attempt in 1..=5 {
            match self.rpc.send_transaction(transaction).await {
                Ok(s) => {
                    info!("📡 Versioned TX inviata via RPC (tentativo {}): {}", attempt, s);
                    return Ok(s.to_string());
                }
                Err(e) => {
                    last_error = e.to_string();
                    
                    // Log specifico per errori DNS/rete
                    if last_error.contains("dns") || last_error.contains("connect") || 
                       last_error.contains("timeout") || last_error.contains("network") {
                        warn!("⚠️ DNS/Network error TX (attempt {}): {}", attempt, last_error);
                    } else {
                        warn!("⚠️ TX send error (attempt {}): {}", attempt, last_error);
                    }
                    
                    // Backoff esponenziale: 300ms, 600ms, 1200ms, 2400ms, 4800ms
                    if attempt < 5 {
                        tokio::time::sleep(Duration::from_millis(300 * (1 << (attempt - 1)))).await;
                    }
                }
            }
        }
        
        error!("❌ Versioned TX fallita dopo 5 tentativi: {}", last_error);
        Err(format!("Errore invio Versioned TX dopo 5 tentativi: {}", last_error).into())
    }

    /// Invia Transaction (legacy) con retry robusto per problemi DNS
    pub async fn send_transaction_with_retry(&self, transaction: &solana_sdk::transaction::Transaction) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let sig = transaction.signatures[0].to_string();
        let mut last_error = String::new();
        
        // Prima prova TPU (veloce, nessun problema DNS solitamente)
        if self.tpu.send_transaction(transaction) {
            info!("⚡ TX inviata via TPU/QUIC: {}", sig);
            return Ok(sig);
        }
        
        // Fallback RPC con retry
        for attempt in 1..=5 {
            match self.rpc.send_transaction(transaction).await {
                Ok(s) => {
                    info!("📡 TX inviata via RPC (tentativo {}): {}", attempt, s);
                    return Ok(s.to_string());
                }
                Err(e) => {
                    last_error = e.to_string();
                    
                    if last_error.contains("dns") || last_error.contains("connect") || 
                       last_error.contains("timeout") || last_error.contains("network") {
                        warn!("⚠️ DNS/Network error (attempt {}): {}", attempt, last_error);
                    }
                    
                    if attempt < 5 {
                        tokio::time::sleep(Duration::from_millis(300 * (1 << (attempt - 1)))).await;
                    }
                }
            }
        }
        
        error!("❌ TX fallita dopo 5 tentativi: {}", last_error);
        Err(format!("Errore invio TX dopo 5 tentativi: {}", last_error).into())
    }
}