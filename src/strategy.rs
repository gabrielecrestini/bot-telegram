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

// --- 3. MONEY MANAGEMENT ---
pub fn calculate_investment_amount(wallet_balance_sol: f64) -> f64 {
    let safe_balance = (wallet_balance_sol - 0.02).max(0.0); 
    if safe_balance < 1.0 { return safe_balance * 0.90; } 
    if safe_balance < 10.0 { return safe_balance * 0.40; } 
    let amount = safe_balance * 0.10; 
    if amount > 5.0 { 5.0 } else { amount }
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