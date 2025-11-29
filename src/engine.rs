use log::{info, warn};
use std::collections::VecDeque;

// --- CONFIGURAZIONE STRATEGIA ---
const RSI_PERIOD: usize = 14;       // Periodo standard per RSI
const RSI_OVERSOLD: f64 = 30.0;     // Sotto 30 = Prezzo ottimo (COMPRA)
const RSI_OVERBOUGHT: f64 = 70.0;   // Sopra 70 = Prezzo alto (VENDI)
const SMA_PERIOD: usize = 20;       // Media mobile a 20 periodi (Trend)

// Struttura che mantiene lo storico dei prezzi in memoria RAM
pub struct MarketData {
    pub prices: VecDeque<f64>, // Coda dei prezzi (Ultimi N prezzi)
    pub symbol: String,
}

impl MarketData {
    pub fn new(symbol: &str) -> Self {
        Self {
            prices: VecDeque::new(),
            symbol: symbol.to_string(),
        }
    }

    /// Aggiunge un nuovo prezzo (Tick) arrivato dal WebSocket
    pub fn add_price(&mut self, price: f64) {
        self.prices.push_back(price);
        // Manteniamo solo gli ultimi 100 prezzi per risparmiare memoria
        if self.prices.len() > 100 {
            self.prices.pop_front();
        }
    }
}

// Risultato dell'analisi
#[derive(Debug, PartialEq)]
pub enum Signal {
    Buy(String),   // "Compra perché RSI basso e Trend Salita"
    Sell(String),  // "Vendi perché RSI alto"
    Hold,          // "Non fare nulla"
}

// --- 1. CALCOLO RSI (Relative Strength Index) ---
// Formula Matematica: 100 - (100 / (1 + RS))
fn calculate_rsi(prices: &VecDeque<f64>) -> Option<f64> {
    if prices.len() < RSI_PERIOD + 1 { return None; }

    let mut gains = 0.0;
    let mut losses = 0.0;

    // Calcoliamo le differenze sugli ultimi 14 periodi
    for i in (prices.len() - RSI_PERIOD)..prices.len() {
        let diff = prices[i] - prices[i - 1];
        if diff >= 0.0 {
            gains += diff;
        } else {
            losses += diff.abs();
        }
    }

    if losses == 0.0 { return Some(100.0); } // Solo salita

    let avg_gain = gains / RSI_PERIOD as f64;
    let avg_loss = losses / RSI_PERIOD as f64;
    let rs = avg_gain / avg_loss;

    Some(100.0 - (100.0 / (1.0 + rs)))
}

// --- 2. CALCOLO SMA (Simple Moving Average) ---
// Formula: Somma ultimi N prezzi / N
fn calculate_sma(prices: &VecDeque<f64>) -> Option<f64> {
    if prices.len() < SMA_PERIOD { return None; }

    let sum: f64 = prices.iter().rev().take(SMA_PERIOD).sum();
    Some(sum / SMA_PERIOD as f64)
}

// --- 3. MOTORE DECISIONALE (IL CERVELLO) ---
pub fn analyze_market(data: &MarketData) -> Signal {
    let current_price = match data.prices.back() {
        Some(p) => *p,
        None => return Signal::Hold,
    };

    // Calcoliamo gli indicatori
    let rsi = calculate_rsi(&data.prices);
    let sma = calculate_sma(&data.prices);

    // Se non abbiamo abbastanza dati storici, aspettiamo
    if rsi.is_none() || sma.is_none() {
        return Signal::Hold;
    }

    let rsi_val = rsi.unwrap();
    let sma_val = sma.unwrap();

    // --- LOGICA DI TRADING ---

    // CASO A: VENDITA (Take Profit)
    // Se l'RSI è alle stelle (>70) siamo in "Ipercomprato". Probabile crollo imminente.
    if rsi_val > RSI_OVERBOUGHT {
        return Signal::Sell(format!("RSI Alto ({:.2}). Ipercomprato!", rsi_val));
    }

    // CASO B: ACQUISTO (Buy the Dip)
    // Compriamo SOLO SE:
    // 1. RSI è basso (<30) -> La gente ha venduto per panico (Discount)
    // 2. Il prezzo è SOPRA la media mobile -> Il trend generale è ancora rialzista (Bull Market)
    if rsi_val < RSI_OVERSOLD && current_price > sma_val {
        return Signal::Buy(format!("Dip Finder! RSI Basso ({:.2}) in Trend Rialzista", rsi_val));
    }

    // CASO C: PANIC SELL (Stop Loss Tecnico)
    // Se il prezzo crolla sotto la media mobile velocemente
    if current_price < sma_val * 0.95 { // 5% sotto la media
        return Signal::Sell("Rottura del Trend (Prezzo < SMA). Uscire!".to_string());
    }

    Signal::Hold
}