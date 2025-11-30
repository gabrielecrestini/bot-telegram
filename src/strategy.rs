use log::{info, debug};
use std::collections::VecDeque;
use serde::Deserialize;

// --- CONFIGURAZIONE INDICATORI ---
const RSI_PERIOD: usize = 14;
const BOLLINGER_PERIOD: usize = 20;
const BOLLINGER_MULT: f64 = 2.0;
const ATR_PERIOD: usize = 14;
const VOLUME_MA_PERIOD: usize = 10; // Media mobile del volume

// Struttura Candela Completa
#[derive(Clone, Copy, Debug)]
pub struct Candle {
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64, // Aggiunto Volume
}

pub struct MarketData {
    pub candles: VecDeque<Candle>,
    pub symbol: String,
    
    // Buffer Tick
    current_high: f64,
    current_low: f64,
    current_vol: f64, // Accumulatore volume tick
    last_price: f64,
    tick_count: u32,
}

impl MarketData {
    pub fn new(symbol: &str) -> Self {
        Self { 
            candles: VecDeque::new(), 
            symbol: symbol.to_string(), 
            current_high: 0.0,
            current_low: f64::MAX,
            current_vol: 0.0,
            last_price: 0.0,
            tick_count: 0,
        }
    }

    // Aggiunge un prezzo e un volume (se disponibile, altrimenti stima 1.0)
    pub fn add_tick(&mut self, price: f64, volume: f64) {
        if self.tick_count == 0 {
            self.current_high = price;
            self.current_low = price;
        } else {
            if price > self.current_high { self.current_high = price; }
            if price < self.current_low { self.current_low = price; }
        }
        self.last_price = price;
        self.current_vol += volume;
        self.tick_count += 1;

        // Chiudi candela ogni 5 tick (Simulazione HFT)
        if self.tick_count >= 5 {
            self.candles.push_back(Candle {
                high: self.current_high,
                low: self.current_low,
                close: price,
                volume: self.current_vol
            });
            if self.candles.len() > 100 { self.candles.pop_front(); }
            
            // Reset
            self.tick_count = 0;
            self.current_high = 0.0;
            self.current_low = f64::MAX;
            self.current_vol = 0.0;
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum TradeAction {
    Buy { amount_sol: f64, reason: String },
    Sell(String),
    Hold,
    UpdateHigh(u64)
}

// --- 1. MATEMATICA FINANZIARIA ---

fn calculate_rsi(candles: &VecDeque<Candle>) -> Option<f64> {
    if candles.len() < RSI_PERIOD + 1 { return None; }
    let mut gains = 0.0; let mut losses = 0.0;
    for i in (candles.len() - RSI_PERIOD)..candles.len() {
        let diff = candles[i].close - candles[i - 1].close;
        if diff >= 0.0 { gains += diff; } else { losses += diff.abs(); }
    }
    if losses == 0.0 { return Some(100.0); }
    let rs = (gains / RSI_PERIOD as f64) / (losses / RSI_PERIOD as f64);
    Some(100.0 - (100.0 / (1.0 + rs)))
}

fn calculate_bollinger(candles: &VecDeque<Candle>) -> Option<(f64, f64)> { 
    if candles.len() < BOLLINGER_PERIOD { return None; }
    let sum: f64 = candles.iter().rev().take(BOLLINGER_PERIOD).map(|c| c.close).sum();
    let ma = sum / BOLLINGER_PERIOD as f64;
    let variance = candles.iter().rev().take(BOLLINGER_PERIOD)
        .map(|c| (ma - c.close).powi(2)).sum::<f64>() / BOLLINGER_PERIOD as f64;
    let std_dev = variance.sqrt();
    Some((ma - std_dev * BOLLINGER_MULT, ma + std_dev * BOLLINGER_MULT))
}

// --- 2. VOLUME ANALYSIS (Whale Detector) ---
// Ritorna true se il volume attuale è molto superiore alla media (Smart Money in entrata)
fn check_volume_spike(candles: &VecDeque<Candle>) -> bool {
    if candles.len() < VOLUME_MA_PERIOD + 1 { return false; }
    
    let current_vol = candles.back().unwrap().volume;
    let sum_vol: f64 = candles.iter().rev().skip(1).take(VOLUME_MA_PERIOD).map(|c| c.volume).sum();
    let avg_vol = sum_vol / VOLUME_MA_PERIOD as f64;

    // Se il volume è doppio rispetto alla media, c'è interesse forte
    current_vol > (avg_vol * 2.0)
}

// --- 3. MONEY MANAGEMENT INTELLIGENTE ---

// Costanti per la strategia
const MIN_TRADE_SOL: f64 = 0.015;      // Minimo ~3€ per coprire fee + avere senso
const FEE_RESERVE_SOL: f64 = 0.003;    // Riserva per fee vendita
const SOL_PRICE_EUR: f64 = 180.0;      // Prezzo indicativo per calcoli

/// Determina il livello di ricchezza con soglie più granulari
/// Questo determina la strategia di investimento
pub enum WealthLevel {
    Micro,      // < 0.03 SOL (~5€) - Troppo poco, un solo trade
    Poor,       // 0.03 - 0.08 SOL (5-15€) - YOLO, investi quasi tutto
    LowMedium,  // 0.08 - 0.27 SOL (15-50€) - Investi tanto, poco margine
    Medium,     // 0.27 - 0.55 SOL (50-100€) - Bilanciato
    HighMedium, // 0.55 - 1.1 SOL (100-200€) - Più conservativo
    Rich,       // > 1.1 SOL (200€+) - Diversifica
}

pub fn get_wealth_level(balance_sol: f64) -> WealthLevel {
    if balance_sol < 0.03 { WealthLevel::Micro }
    else if balance_sol < 0.08 { WealthLevel::Poor }
    else if balance_sol < 0.27 { WealthLevel::LowMedium }
    else if balance_sol < 0.55 { WealthLevel::Medium }
    else if balance_sol < 1.1 { WealthLevel::HighMedium }
    else { WealthLevel::Rich }
}

/// Calcola importo da investire - INTELLIGENTE
/// 
/// Logica: Se hai poco, DEVI investire tanto per avere chance di profitto.
/// Le fee Solana + DEX sono ~0.5-1%, quindi con 3€ perdi già 0.03€ in fee.
/// Per guadagnare qualcosa di significativo serve almeno 0.015 SOL.
///
/// Il bot NON usa stop loss fissi - decide autonomamente quando vendere.
pub fn calculate_investment_amount(wallet_balance_sol: f64) -> f64 {
    let safe_balance = (wallet_balance_sol - FEE_RESERVE_SOL).max(0.0);
    
    // Se non hai abbastanza nemmeno per un trade minimo, non fare nulla
    if safe_balance < MIN_TRADE_SOL {
        return 0.0;
    }
    
    let investment = match get_wealth_level(wallet_balance_sol) {
        WealthLevel::Micro => {
            // Micro (<5€): Tutto quello che hai, un solo tentativo
            safe_balance * 0.95
        },
        WealthLevel::Poor => {
            // Povero (5-15€): YOLO mode - investi 85-90%
            // Con 10€, investi 8.5€. Un +20% = 1.7€ di guadagno
            safe_balance * 0.88
        },
        WealthLevel::LowMedium => {
            // Medio-basso (15-50€): Investi 70-80%
            // Con 30€ (0.16 SOL), investi ~24€
            // Abbastanza per 1-2 trade significativi
            safe_balance * 0.75
        },
        WealthLevel::Medium => {
            // Medio (50-100€): Investi 50-60%
            // Con 75€ (0.4 SOL), investi ~40€
            // Puoi permetterti 2-3 trade
            safe_balance * 0.55
        },
        WealthLevel::HighMedium => {
            // Medio-alto (100-200€): Investi 35-45%
            // Con 150€ (0.8 SOL), investi ~60€
            // Diversificazione leggera
            safe_balance * 0.40
        },
        WealthLevel::Rich => {
            // Ricco (>200€): Investi 15-25%
            // Con 500€ (2.7 SOL), investi ~100€
            // Diversifica su più trade
            let pct = if wallet_balance_sol < 3.0 { 0.25 } else if wallet_balance_sol < 5.0 { 0.18 } else { 0.12 };
            safe_balance * pct
        }
    };
    
    // Assicura che l'investimento sia almeno il minimo
    // Ma non più del saldo disponibile
    investment.max(MIN_TRADE_SOL).min(safe_balance)
}

/// Ritorna la percentuale del saldo che verrà investita (per UI)
pub fn get_investment_percentage(wallet_balance_sol: f64) -> u8 {
    match get_wealth_level(wallet_balance_sol) {
        WealthLevel::Micro => 95,
        WealthLevel::Poor => 88,
        WealthLevel::LowMedium => 75,
        WealthLevel::Medium => 55,
        WealthLevel::HighMedium => 40,
        WealthLevel::Rich => if wallet_balance_sol < 3.0 { 25 } else if wallet_balance_sol < 5.0 { 18 } else { 12 },
    }
}

/// Calcola investimento per importo specifico impostato dall'utente
pub fn calculate_user_set_investment(wallet_balance_sol: f64, user_amount_sol: f64) -> f64 {
    let safe_balance = (wallet_balance_sol - 0.005).max(0.0);
    
    // Non permettere di investire più del 95% del saldo disponibile
    let max_allowed = safe_balance * 0.95;
    
    user_amount_sol.min(max_allowed).max(0.0)
}

/// Il bot determina automaticamente quando vendere (NO stop loss fisso)
/// Ritorna la percentuale di stop dinamica basata sul profitto raggiunto
pub fn calculate_dynamic_stop(entry_price: f64, current_price: f64, highest_price: f64) -> f64 {
    let profit_from_entry = ((current_price - entry_price) / entry_price) * 100.0;
    let drop_from_high = ((highest_price - current_price) / highest_price) * 100.0;
    
    // Se siamo in profitto, trailing stop più stretto
    if profit_from_entry > 100.0 {
        // +100% profit -> stop a -5% dal massimo
        5.0
    } else if profit_from_entry > 50.0 {
        // +50% profit -> stop a -8% dal massimo
        8.0
    } else if profit_from_entry > 20.0 {
        // +20% profit -> stop a -12% dal massimo
        12.0
    } else if profit_from_entry > 0.0 {
        // In leggero profitto -> stop a -15%
        15.0
    } else {
        // In perdita -> stop a -25% (lascia spazio per recupero)
        25.0
    }
}

// --- 4. ENGINE DECISIONALE (Volume + Prezzo) ---

pub fn analyze_market(data: &MarketData, wallet_balance: f64) -> TradeAction {
    if data.candles.len() < BOLLINGER_PERIOD { return TradeAction::Hold; }
    
    let current_close = data.candles.back().unwrap().close;
    let rsi = calculate_rsi(&data.candles);
    let bb = calculate_bollinger(&data.candles);
    let volume_spike = check_volume_spike(&data.candles);

    if rsi.is_none() || bb.is_none() { return TradeAction::Hold; }
    
    let rsi_val = rsi.unwrap();
    let (lower_band, upper_band) = bb.unwrap();

    // VENDITA
    if rsi_val > 75.0 || current_close > upper_band {
        return TradeAction::Sell(format!("Overbought: RSI {:.1}", rsi_val));
    }

    // ACQUISTO (Setup Whale)
    // 1. Prezzo basso (Sconto BB o RSI < 40)
    // 2. VOLUME ALTO (Qualcuno sta comprando pesantemente il dip!)
    
    let is_cheap = current_close <= lower_band * 1.02 || rsi_val < 40.0;

    if is_cheap && volume_spike {
        let invest_amount = calculate_investment_amount(wallet_balance);
        if invest_amount > 0.001 {
            return TradeAction::Buy {
                amount_sol: invest_amount,
                reason: format!("WHALE ALERT: Volume Spike + Prezzo Basso (RSI {:.1})", rsi_val)
            };
        }
    }

    TradeAction::Hold
}

// --- 5. TRAILING STOP ---
pub fn check_position(current_val: u64, high_val: u64) -> TradeAction {
    if current_val > high_val { return TradeAction::UpdateHigh(current_val); }

    let drop_pct = (high_val.saturating_sub(current_val) as f64 / high_val as f64) * 100.0;
    let dynamic_stop = if high_val > (current_val * 12 / 10) { 3.0 } else { 10.0 };

    if drop_pct >= dynamic_stop {
        return TradeAction::Sell(format!("Smart Stop: -{:.1}%", drop_pct));
    }
    
    TradeAction::Hold
}