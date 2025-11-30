use solana_client::nonblocking::rpc_client::RpcClient as AsyncRpcClient;
use solana_client::rpc_client::RpcClient as BlockingRpcClient; // <--- Ci serve solo per l'inizializzazione
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::tpu_client::{TpuClient, TpuClientConfig};
// Importiamo i tipi necessari per definire i Generics del TPU
use solana_quic_client::{QuicPool, QuicConnectionManager, QuicConfig}; 
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use std::env;
use log::info;

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
}