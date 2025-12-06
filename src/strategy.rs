use log::{info, debug};
use std::collections::VecDeque;

// ═══════════════════════════════════════════════════════════════════════════════
// ADAPTIVE MULTI-PHASE MOMENTUM STRATEGY (AMMS) - 2025
// DUE MODALITÀ: DIP (compra basso) + BREAKOUT (cavalca l'onda)
// ═══════════════════════════════════════════════════════════════════════════════

// --- CONFIGURAZIONE INDICATORI ---
const EMA_FAST: usize = 20;
const EMA_SLOW: usize = 50;
const RSI_PERIOD: usize = 14;
const ATR_PERIOD: usize = 14;
const BOLLINGER_PERIOD: usize = 20;
const BOLLINGER_MULT: f64 = 2.0;

// --- SOGLIE DIP MODE (Compra lo sconto) ---
const RSI_OVERSOLD: f64 = 35.0;          // Più permissivo (era 30)
const RSI_DIP_MAX: f64 = 50.0;           // RSI sotto 50 per DIP (era 45)

// --- SOGLIE BREAKOUT MODE (Cavalca l'onda) ---
const RSI_BREAKOUT_MIN: f64 = 55.0;      // RSI sopra 55 per BREAKOUT
const RSI_BREAKOUT_MAX: f64 = 75.0;      // Non comprare se overbought
const BREAKOUT_CHANGE_24H: f64 = 8.0;    // Minimo +8% in 24h (era 10)
const BREAKOUT_CHANGE_1H: f64 = 2.0;     // Minimo +2% in 1h (era 3)
const VOLUME_SPIKE_MULT: f64 = 1.3;      // Volume 1.3x sopra media (era 1.5)

// --- SOGLIE COMUNI ---
const MIN_VOLUME_USD: f64 = 25_000.0;    // $25k minimo (era $50k)
const MIN_LIQUIDITY_USD: f64 = 5_000.0;  // $5k minimo (era $10k)

// --- STOP LOSS / TAKE PROFIT ---
const ATR_STOP_MULTIPLIER: f64 = 1.5;
const ATR_TP_MULTIPLIER: f64 = 2.0;
const TRAILING_TRIGGER_PCT: f64 = 3.0;
const TRAILING_DROP_PCT: f64 = 3.0;

// ═══════════════════════════════════════════════════════════════════════════════
// STRUTTURE DATI
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug)]
pub struct Candle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Clone, Debug)]
pub struct MarketData {
    pub candles: VecDeque<Candle>,
    pub symbol: String,
    current_open: f64,
    current_high: f64,
    current_low: f64,
    current_vol: f64,
    last_price: f64,
    tick_count: u32,
}

impl MarketData {
    pub fn new(symbol: &str) -> Self {
        Self { 
            candles: VecDeque::new(), 
            symbol: symbol.to_string(), 
            current_open: 0.0,
            current_high: 0.0,
            current_low: f64::MAX,
            current_vol: 0.0,
            last_price: 0.0,
            tick_count: 0,
        }
    }

    pub fn add_tick(&mut self, price: f64, volume: f64) {
        if self.tick_count == 0 {
            self.current_open = price;
            self.current_high = price;
            self.current_low = price;
        } else {
            if price > self.current_high { self.current_high = price; }
            if price < self.current_low { self.current_low = price; }
        }
        self.last_price = price;
        self.current_vol += volume;
        self.tick_count += 1;

        if self.tick_count >= 5 {
            self.candles.push_back(Candle {
                open: self.current_open,
                high: self.current_high,
                low: self.current_low,
                close: price,
                volume: self.current_vol
            });
            if self.candles.len() > 200 { self.candles.pop_front(); }
            
            self.tick_count = 0;
            self.current_open = 0.0;
            self.current_high = 0.0;
            self.current_low = f64::MAX;
            self.current_vol = 0.0;
        }
    }
    
    pub fn get_last_price(&self) -> f64 {
        self.candles.back().map(|c| c.close).unwrap_or(self.last_price)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum TradeAction {
    Buy { amount_sol: f64, reason: String },
    Sell(String),
    Hold,
    UpdateHigh(u64),
}

/// Modalità di trading
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TradingMode {
    Dip,       // Compra lo sconto (RSI basso, prezzo basso)
    Breakout,  // Cavalca l'onda (momentum forte, già in salita)
    None,      // Nessun segnale valido
}

/// Risultato analisi completa
#[derive(Debug, Clone)]
pub struct MarketAnalysis {
    pub ema_20: f64,
    pub ema_50: f64,
    pub rsi: f64,
    pub atr: f64,
    pub bb_upper: f64,
    pub bb_lower: f64,
    pub bb_middle: f64,
    pub trend: Trend,
    pub signal: Signal,
    pub mode: TradingMode,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub volume_ratio: f64,  // Rapporto volume attuale / media
}

#[derive(Debug, Clone, PartialEq)]
pub enum Trend {
    StrongBullish,
    Bullish,
    Neutral,
    Bearish,
    StrongBearish,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    StrongBuy,
    Buy,
    Hold,
    Sell,
    StrongSell,
}

// ═══════════════════════════════════════════════════════════════════════════════
// INDICATORI MATEMATICI
// ═══════════════════════════════════════════════════════════════════════════════

fn calculate_ema(candles: &VecDeque<Candle>, period: usize) -> Option<f64> {
    if candles.len() < period { return None; }
    
    let k = 2.0 / (period as f64 + 1.0);
    let mut ema = candles.iter().take(period).map(|c| c.close).sum::<f64>() / period as f64;
    
    for candle in candles.iter().skip(period) {
        ema = candle.close * k + ema * (1.0 - k);
    }
    
    Some(ema)
}

fn calculate_rsi(candles: &VecDeque<Candle>) -> Option<f64> {
    if candles.len() < RSI_PERIOD + 1 { return None; }
    
    let mut gains: f64 = 0.0;
    let mut losses: f64 = 0.0;
    
    let start = candles.len() - RSI_PERIOD - 1;
    for i in start..candles.len() - 1 {
        let diff = candles[i + 1].close - candles[i].close;
        if diff >= 0.0 { gains += diff; } 
        else { losses += diff.abs(); }
    }
    
    let avg_gain = gains / RSI_PERIOD as f64;
    let avg_loss = losses / RSI_PERIOD as f64;
    
    if avg_loss == 0.0 { return Some(100.0); }
    
    let rs = avg_gain / avg_loss;
    Some(100.0 - (100.0 / (1.0 + rs)))
}

fn calculate_atr(candles: &VecDeque<Candle>) -> Option<f64> {
    if candles.len() < ATR_PERIOD + 1 { return None; }
    
    let mut tr_sum: f64 = 0.0;
    let start = candles.len() - ATR_PERIOD;
    
    for i in start..candles.len() {
        let high = candles[i].high;
        let low = candles[i].low;
        let prev_close = if i > 0 { candles[i - 1].close } else { candles[i].open };
        
        let tr = (high - low)
            .max((high - prev_close).abs())
            .max((low - prev_close).abs());
        
        tr_sum += tr;
    }
    
    Some(tr_sum / ATR_PERIOD as f64)
}

fn calculate_bollinger(candles: &VecDeque<Candle>) -> Option<(f64, f64, f64)> {
    if candles.len() < BOLLINGER_PERIOD { return None; }
    
    let prices: Vec<f64> = candles.iter()
        .rev()
        .take(BOLLINGER_PERIOD)
        .map(|c| c.close)
        .collect();
    
    let sum: f64 = prices.iter().sum();
    let middle = sum / BOLLINGER_PERIOD as f64;
    
    let variance: f64 = prices.iter()
        .map(|p| (p - middle).powi(2))
        .sum::<f64>() / BOLLINGER_PERIOD as f64;
    
    let std_dev = variance.sqrt();
    
    let upper = middle + BOLLINGER_MULT * std_dev;
    let lower = middle - BOLLINGER_MULT * std_dev;
    
    Some((lower, middle, upper))
}

/// Calcola il rapporto del volume attuale rispetto alla media
fn calculate_volume_ratio(candles: &VecDeque<Candle>) -> f64 {
    if candles.len() < 20 { return 1.0; }
    
    let avg_volume: f64 = candles.iter()
        .rev()
        .skip(1) // Escludi l'ultimo
        .take(20)
        .map(|c| c.volume)
        .sum::<f64>() / 20.0;
    
    let current_volume = candles.back().map(|c| c.volume).unwrap_or(0.0);
    
    if avg_volume > 0.0 {
        current_volume / avg_volume
    } else {
        1.0
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// ANALISI COMPLETA DEL MERCATO
// ═══════════════════════════════════════════════════════════════════════════════

pub fn analyze_market_full(data: &MarketData) -> Option<MarketAnalysis> {
    if data.candles.len() < EMA_SLOW + 10 { return None; }
    
    let ema_20 = calculate_ema(&data.candles, EMA_FAST)?;
    let ema_50 = calculate_ema(&data.candles, EMA_SLOW)?;
    let rsi = calculate_rsi(&data.candles)?;
    let atr = calculate_atr(&data.candles)?;
    let (bb_lower, bb_middle, bb_upper) = calculate_bollinger(&data.candles)?;
    let volume_ratio = calculate_volume_ratio(&data.candles);
    
    let current_price = data.get_last_price();
    
    // Determina trend
    let ema_diff_pct = ((ema_20 - ema_50) / ema_50) * 100.0;
    let trend = if ema_diff_pct > 3.0 { Trend::StrongBullish }
        else if ema_diff_pct > 0.5 { Trend::Bullish }
        else if ema_diff_pct < -3.0 { Trend::StrongBearish }
        else if ema_diff_pct < -0.5 { Trend::Bearish }
        else { Trend::Neutral };
    
    // Determina modalità e segnale
    let (mode, signal) = determine_mode_and_signal(
        &trend, rsi, current_price, bb_upper, bb_lower, bb_middle, ema_20, volume_ratio
    );
    
    let stop_loss = current_price - (ATR_STOP_MULTIPLIER * atr);
    let take_profit = current_price + (ATR_TP_MULTIPLIER * atr);
    
    Some(MarketAnalysis {
        ema_20,
        ema_50,
        rsi,
        atr,
        bb_upper,
        bb_lower,
        bb_middle,
        trend,
        signal,
        mode,
        stop_loss,
        take_profit,
        volume_ratio,
    })
}

/// Determina modalità (DIP o BREAKOUT) e segnale
fn determine_mode_and_signal(
    trend: &Trend, 
    rsi: f64, 
    price: f64, 
    bb_upper: f64, 
    bb_lower: f64,
    bb_middle: f64,
    ema_20: f64, 
    volume_ratio: f64
) -> (TradingMode, Signal) {
    
    // ═══════════════════════════════════════════════════════════════
    // MODALITÀ DIP: Compra quando il prezzo è basso (sconto)
    // Condizioni: RSI basso + prezzo vicino a BB inferiore + trend non bearish
    // ═══════════════════════════════════════════════════════════════
    if rsi < RSI_DIP_MAX && price <= bb_lower * 1.02 {
        if rsi < RSI_OVERSOLD && !matches!(trend, Trend::StrongBearish) {
            return (TradingMode::Dip, Signal::StrongBuy);
        }
        if rsi < RSI_DIP_MAX && matches!(trend, Trend::Neutral | Trend::Bullish | Trend::StrongBullish) {
            return (TradingMode::Dip, Signal::Buy);
        }
    }
    
    // ═══════════════════════════════════════════════════════════════
    // MODALITÀ BREAKOUT: Cavalca l'onda quando il momentum è forte
    // Condizioni: 
    //   - RSI tra 55-75 (momentum forte ma non overbought)
    //   - Prezzo sopra EMA20 e BB middle
    //   - Trend bullish
    //   - Volume sopra la media (volume spike)
    // ═══════════════════════════════════════════════════════════════
    if rsi >= RSI_BREAKOUT_MIN && rsi <= RSI_BREAKOUT_MAX {
        if matches!(trend, Trend::Bullish | Trend::StrongBullish) {
            if price > ema_20 && price > bb_middle {
                // STRONG BREAKOUT: Volume spike + prezzo rompe BB superiore
                if volume_ratio >= VOLUME_SPIKE_MULT && price >= bb_upper * 0.98 {
                    return (TradingMode::Breakout, Signal::StrongBuy);
                }
                // BREAKOUT normale: Volume ok + trend forte
                if volume_ratio >= 1.2 {
                    return (TradingMode::Breakout, Signal::Buy);
                }
            }
        }
    }
    
    // ═══════════════════════════════════════════════════════════════
    // SEGNALI DI VENDITA
    // ═══════════════════════════════════════════════════════════════
    if rsi > 80.0 {
        return (TradingMode::None, Signal::StrongSell);
    }
    
    if matches!(trend, Trend::Bearish | Trend::StrongBearish) && rsi > 60.0 {
        return (TradingMode::None, Signal::Sell);
    }
    
    (TradingMode::None, Signal::Hold)
}

// ═══════════════════════════════════════════════════════════════════════════════
// LOGICA DI ENTRATA CON DUE MODALITÀ
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct ExternalData {
    pub price: f64,
    pub change_5m: f64,
    pub change_1h: f64,
    pub change_24h: f64,
    pub volume_24h: f64,
    pub liquidity_usd: f64,
    pub market_cap: f64,
}

/// Verifica se soddisfa le condizioni di entrata (DIP o BREAKOUT)
pub fn check_entry_conditions(analysis: &MarketAnalysis, external: &ExternalData) -> (bool, String) {
    let mut reasons: Vec<String> = Vec::new();
    let mut score: u8 = 0;
    
    // Requisiti base per entrambe le modalità
    if external.volume_24h < MIN_VOLUME_USD {
        return (false, format!("❌ Volume basso: ${:.0}", external.volume_24h));
    }
    if external.liquidity_usd < MIN_LIQUIDITY_USD {
        return (false, format!("❌ Liquidità bassa: ${:.0}", external.liquidity_usd));
    }
    
    score += 10; // Base per requisiti soddisfatti
    
    match analysis.mode {
        // ═══════════════════════════════════════════════════════════════
        // MODALITÀ DIP: Compra lo sconto
        // ═══════════════════════════════════════════════════════════════
        TradingMode::Dip => {
            reasons.push("🔵 DIP".to_string());
            
            // RSI basso = sconto
            if analysis.rsi < RSI_OVERSOLD {
                score += 25;
                reasons.push("RSI Oversold".to_string());
            } else if analysis.rsi < RSI_DIP_MAX {
                score += 15;
                reasons.push("RSI Basso".to_string());
            }
            
            // Prezzo vicino a BB inferiore
            if external.price <= analysis.bb_lower * 1.02 {
                score += 20;
                reasons.push("BB Lower".to_string());
            }
            
            // Trend non deve essere fortemente bearish
            if matches!(analysis.trend, Trend::Bullish | Trend::StrongBullish) {
                score += 15;
                reasons.push("Trend ↑".to_string());
            } else if matches!(analysis.trend, Trend::Neutral) {
                score += 10;
            }
            
            // Volume ok
            if analysis.volume_ratio >= 1.0 {
                score += 10;
                reasons.push("Vol OK".to_string());
            }
            
            // ATR basso = meno rischio
            let atr_pct = (analysis.atr / external.price) * 100.0;
            if atr_pct < 3.0 {
                score += 10;
                reasons.push("Low Risk".to_string());
            }
        },
        
        // ═══════════════════════════════════════════════════════════════
        // MODALITÀ BREAKOUT: Cavalca l'onda
        // ═══════════════════════════════════════════════════════════════
        TradingMode::Breakout => {
            reasons.push("🚀 BREAKOUT".to_string());
            
            // Token già in salita (+10% 24h minimo)
            if external.change_24h >= BREAKOUT_CHANGE_24H {
                score += 20;
                reasons.push(format!("24h +{:.0}%", external.change_24h));
            } else if external.change_24h >= 5.0 {
                score += 10;
            }
            
            // Momentum forte in 1h
            if external.change_1h >= BREAKOUT_CHANGE_1H {
                score += 20;
                reasons.push("1h Pump".to_string());
            } else if external.change_1h >= 1.0 {
                score += 10;
            }
            
            // Volume spike (1.5x+ sopra media)
            if analysis.volume_ratio >= VOLUME_SPIKE_MULT {
                score += 25;
                reasons.push("Vol Spike!".to_string());
            } else if analysis.volume_ratio >= 1.2 {
                score += 15;
                reasons.push("Vol ↑".to_string());
            }
            
            // RSI nel range momentum (55-75)
            if analysis.rsi >= RSI_BREAKOUT_MIN && analysis.rsi <= 70.0 {
                score += 15;
                reasons.push("RSI Momentum".to_string());
            }
            
            // Trend bullish confermato
            if matches!(analysis.trend, Trend::StrongBullish) {
                score += 15;
                reasons.push("Strong Trend".to_string());
            } else if matches!(analysis.trend, Trend::Bullish) {
                score += 10;
            }
            
            // Prezzo sopra BB middle
            if external.price > analysis.bb_middle {
                score += 10;
            }
        },
        
        TradingMode::None => {
            return (false, "⏸️ Nessun segnale valido".to_string());
        }
    }
    
    // Score minimo per entrare
    let min_score = match analysis.mode {
        TradingMode::Dip => 55,      // DIP richiede meno conferme
        TradingMode::Breakout => 65, // BREAKOUT richiede più conferme (più rischioso)
        TradingMode::None => 100,
    };
    
    let should_buy = score >= min_score;
    
    let mode_emoji = match analysis.mode {
        TradingMode::Dip => "🔵",
        TradingMode::Breakout => "🚀",
        TradingMode::None => "⏸️",
    };
    
    let reason = if should_buy {
        format!("{} {} BUY [{}] - {}", mode_emoji, 
            if analysis.mode == TradingMode::Dip { "DIP" } else { "BREAKOUT" },
            score, reasons.join(" | "))
    } else {
        format!("⏸️ Score: {} ({}) - {}", score, min_score, reasons.join(" | "))
    };
    
    (should_buy, reason)
}

// ═══════════════════════════════════════════════════════════════════════════════
// STOP LOSS ADATTIVO E TRAILING
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct Position {
    pub entry_price: f64,
    pub entry_atr: f64,
    pub highest_price: f64,
    pub current_stop: f64,
    pub target_tp: f64,
    pub trailing_active: bool,
    pub mode: TradingMode,
}

impl Position {
    pub fn new(entry_price: f64, atr: f64, mode: TradingMode) -> Self {
        // BREAKOUT usa stop più largo (momentum può oscillare)
        let stop_mult = if mode == TradingMode::Breakout { 2.0 } else { 1.5 };
        let tp_mult = if mode == TradingMode::Breakout { 3.0 } else { 2.0 };
        
        Self {
            entry_price,
            entry_atr: atr,
            highest_price: entry_price,
            current_stop: entry_price - (stop_mult * atr),
            target_tp: entry_price + (tp_mult * atr),
            trailing_active: false,
            mode,
        }
    }
    
    pub fn update(&mut self, current_price: f64, current_atr: f64) -> TradeAction {
        let stop_mult = if self.mode == TradingMode::Breakout { 2.0 } else { 1.5 };
        
        if current_price > self.highest_price {
            self.highest_price = current_price;
            
            let gain_pct = ((current_price - self.entry_price) / self.entry_price) * 100.0;
            if gain_pct >= TRAILING_TRIGGER_PCT {
                self.trailing_active = true;
            }
            
            if self.trailing_active {
                let new_stop = current_price - (stop_mult * current_atr);
                if new_stop > self.current_stop {
                    self.current_stop = new_stop;
                    debug!("📈 Trailing stop: ${:.8}", self.current_stop);
                }
            }
        }
        
        let pnl_pct = ((current_price - self.entry_price) / self.entry_price) * 100.0;
        
        if current_price <= self.current_stop {
            return TradeAction::Sell(format!(
                "🛑 STOP {} | PnL: {:+.1}%",
                if self.mode == TradingMode::Breakout { "BREAKOUT" } else { "DIP" },
                pnl_pct
            ));
        }
        
        if self.trailing_active {
            let drop_from_high = ((self.highest_price - current_price) / self.highest_price) * 100.0;
            let max_drop = if self.mode == TradingMode::Breakout { 4.0 } else { 3.0 };
            
            if drop_from_high >= max_drop {
                return TradeAction::Sell(format!(
                    "📉 TRAILING -{:.1}% | Max: ${:.8} | PnL: {:+.1}%",
                    drop_from_high, self.highest_price, pnl_pct
                ));
            }
        }
        
        if current_price >= self.target_tp && !self.trailing_active {
            self.trailing_active = true;
            info!("🎯 TP raggiunto, trailing ON");
        }
        
        TradeAction::Hold
    }
    
    pub fn current_pnl_pct(&self, current_price: f64) -> f64 {
        ((current_price - self.entry_price) / self.entry_price) * 100.0
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MONEY MANAGEMENT - With Single Trade Mode for Low Balance
// ═══════════════════════════════════════════════════════════════════════════════

const MIN_TRADE_SOL: f64 = 0.015;
const FEE_RESERVE_SOL: f64 = 0.003;

/// Threshold below which single trade mode is activated (in SOL)
const SINGLE_TRADE_THRESHOLD: f64 = 0.1; // ~€18 at $200/SOL

/// Maximum open positions allowed in single trade mode
const SINGLE_TRADE_MAX_POSITIONS: usize = 1;

/// Normal mode maximum positions
const NORMAL_MAX_POSITIONS: usize = 5;

pub enum WealthLevel {
    Micro,
    Poor,
    LowMedium,
    Medium,
    HighMedium,
    Rich,
}

/// Configuration for trading mode based on balance
#[derive(Debug, Clone)]
pub struct TradingConfig {
    pub single_trade_mode: bool,
    pub max_positions: usize,
    pub investment_pct: f64,
    pub min_trade_sol: f64,
    pub risk_level: RiskProfile,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RiskProfile {
    UltraAggressive, // Low balance - all-in single trade
    Aggressive,      // Low-medium balance
    Balanced,        // Medium balance
    Conservative,    // High balance
}

impl TradingConfig {
    pub fn from_balance(balance_sol: f64) -> Self {
        if balance_sol < SINGLE_TRADE_THRESHOLD {
            // SINGLE TRADE MODE: Low balance = one trade at a time, maximum risk
            TradingConfig {
                single_trade_mode: true,
                max_positions: SINGLE_TRADE_MAX_POSITIONS,
                investment_pct: 0.95, // 95% of balance
                min_trade_sol: MIN_TRADE_SOL,
                risk_level: RiskProfile::UltraAggressive,
            }
        } else if balance_sol < 0.3 {
            TradingConfig {
                single_trade_mode: false,
                max_positions: 2,
                investment_pct: 0.80,
                min_trade_sol: MIN_TRADE_SOL,
                risk_level: RiskProfile::Aggressive,
            }
        } else if balance_sol < 1.0 {
            TradingConfig {
                single_trade_mode: false,
                max_positions: 3,
                investment_pct: 0.55,
                min_trade_sol: MIN_TRADE_SOL,
                risk_level: RiskProfile::Balanced,
            }
        } else {
            TradingConfig {
                single_trade_mode: false,
                max_positions: NORMAL_MAX_POSITIONS,
                investment_pct: 0.25,
                min_trade_sol: 0.02,
                risk_level: RiskProfile::Conservative,
            }
        }
    }
    
    /// Check if we can open a new position given current open positions
    pub fn can_open_position(&self, current_positions: usize) -> bool {
        current_positions < self.max_positions
    }
    
    /// Calculate investment for this config
    pub fn calculate_investment(&self, balance_sol: f64, atr_pct: Option<f64>) -> f64 {
        let safe_balance = (balance_sol - FEE_RESERVE_SOL).max(0.0);
        
        if safe_balance < self.min_trade_sol {
            return 0.0;
        }
        
        // Risk adjustment based on ATR
        let risk_factor = match atr_pct {
            Some(atr) if atr > 5.0 => 0.7,
            Some(atr) if atr > 3.0 => 0.85,
            _ => 1.0,
        };
        
        let base = safe_balance * self.investment_pct * risk_factor;
        
        // In single trade mode, ensure we use almost everything
        if self.single_trade_mode {
            return base.max(self.min_trade_sol).min(safe_balance * 0.98);
        }
        
        base.max(self.min_trade_sol).min(safe_balance)
    }
}

/// Check if should auto-liquidate (balance too low with open positions)
pub fn should_auto_liquidate(balance_sol: f64, open_positions: usize) -> bool {
    // Auto-liquidate if balance is very low AND we have positions
    // This helps prevent getting stuck with tiny positions
    let critical_threshold = 0.02; // ~€3.60
    
    if balance_sol < critical_threshold && open_positions > 0 {
        info!("⚠️ Auto-liquidation triggered: balance {:.4} SOL < threshold {:.4}", 
            balance_sol, critical_threshold);
        return true;
    }
    
    // Also liquidate if we can't afford fees to trade anymore
    if balance_sol < FEE_RESERVE_SOL * 3.0 && open_positions > 0 {
        info!("⚠️ Auto-liquidation: insufficient fee reserve");
        return true;
    }
    
    false
}

pub fn get_wealth_level(balance_sol: f64) -> WealthLevel {
    if balance_sol < 0.03 { WealthLevel::Micro }
    else if balance_sol < 0.08 { WealthLevel::Poor }
    else if balance_sol < 0.27 { WealthLevel::LowMedium }
    else if balance_sol < 0.55 { WealthLevel::Medium }
    else if balance_sol < 1.1 { WealthLevel::HighMedium }
    else { WealthLevel::Rich }
}

/// Calcola importo da investire (considera anche la modalità)
/// Now uses TradingConfig for better single-trade mode support
pub fn calculate_investment_amount(wallet_balance_sol: f64, atr_pct: Option<f64>) -> f64 {
    let config = TradingConfig::from_balance(wallet_balance_sol);
    config.calculate_investment(wallet_balance_sol, atr_pct)
}

/// Calcola importo da investire with position count check
/// Returns 0 if max positions reached
pub fn calculate_investment_amount_with_positions(
    wallet_balance_sol: f64, 
    atr_pct: Option<f64>,
    current_positions: usize
) -> f64 {
    let config = TradingConfig::from_balance(wallet_balance_sol);
    
    // In single trade mode, only allow 1 position
    if !config.can_open_position(current_positions) {
        info!("⏸️ Max positions reached ({}/{}), waiting for trade to close", 
            current_positions, config.max_positions);
        return 0.0;
    }
    
    config.calculate_investment(wallet_balance_sol, atr_pct)
}

/// Calcola importo per BREAKOUT (leggermente più cauto)
pub fn calculate_breakout_investment(wallet_balance_sol: f64, atr_pct: Option<f64>) -> f64 {
    let config = TradingConfig::from_balance(wallet_balance_sol);
    
    // BREAKOUT è più rischioso, usa 80% dell'importo normale
    // But in single trade mode, go all-in
    if config.single_trade_mode {
        config.calculate_investment(wallet_balance_sol, atr_pct) * 0.90
    } else {
        config.calculate_investment(wallet_balance_sol, atr_pct) * 0.80
    }
}

/// Get trading configuration for a given balance
pub fn get_trading_config(balance_sol: f64) -> TradingConfig {
    TradingConfig::from_balance(balance_sol)
}

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

// ═══════════════════════════════════════════════════════════════════════════════
// ENGINE DECISIONALE PRINCIPALE
// ═══════════════════════════════════════════════════════════════════════════════

pub fn analyze_market(data: &MarketData, wallet_balance: f64) -> TradeAction {
    let analysis = match analyze_market_full(data) {
        Some(a) => a,
        None => return TradeAction::Hold,
    };
    
    let current_price = data.get_last_price();
    let external = ExternalData {
        price: current_price,
        change_5m: 3.0,
        change_1h: 5.0,
        change_24h: 10.0,
        volume_24h: 100_000.0,
        liquidity_usd: 50_000.0,
        market_cap: 1_000_000.0,
    };
    
    let (should_buy, reason) = check_entry_conditions(&analysis, &external);
    
    if should_buy {
        let atr_pct = (analysis.atr / current_price) * 100.0;
        let invest_amount = match analysis.mode {
            TradingMode::Breakout => calculate_breakout_investment(wallet_balance, Some(atr_pct)),
            _ => calculate_investment_amount(wallet_balance, Some(atr_pct)),
        };
        
        if invest_amount > 0.0 {
            return TradeAction::Buy {
                amount_sol: invest_amount,
                reason,
            };
        }
    }
    
    if matches!(analysis.signal, Signal::Sell | Signal::StrongSell) {
        return TradeAction::Sell(format!("📉 {} | RSI: {:.0}", 
            if analysis.signal == Signal::StrongSell { "STRONG SELL" } else { "SELL" },
            analysis.rsi
        ));
    }
    
    TradeAction::Hold
}

pub fn check_position(current_val: u64, high_val: u64) -> TradeAction {
    let current = current_val as f64;
    let high = high_val as f64;
    
    if current > high { 
        return TradeAction::UpdateHigh(current_val); 
    }

    let drop_pct = ((high - current) / high) * 100.0;
    let profit_pct = ((high - current) / current) * 100.0;
    
    let dynamic_stop = if profit_pct > 50.0 { 2.0 }
        else if profit_pct > 20.0 { 3.0 }
        else if profit_pct > 0.0 { 5.0 }
        else { 10.0 };

    if drop_pct >= dynamic_stop {
        return TradeAction::Sell(format!("🛑 Trailing Stop -{:.1}%", drop_pct));
    }
    
    TradeAction::Hold
}

pub fn calculate_user_set_investment(wallet_balance_sol: f64, user_amount_sol: f64) -> f64 {
    let safe_balance = (wallet_balance_sol - FEE_RESERVE_SOL).max(0.0);
    let max_allowed = safe_balance * 0.95;
    user_amount_sol.min(max_allowed).max(0.0)
}

pub fn calculate_dynamic_stop(entry_price: f64, current_price: f64, _highest_price: f64) -> f64 {
    let profit_from_entry = ((current_price - entry_price) / entry_price) * 100.0;
    
    if profit_from_entry > 100.0 { 5.0 }
    else if profit_from_entry > 50.0 { 8.0 }
    else if profit_from_entry > 20.0 { 12.0 }
    else if profit_from_entry > 0.0 { 15.0 }
    else { 25.0 }
}
