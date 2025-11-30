use log::{info, debug};
use std::collections::VecDeque;

// ═══════════════════════════════════════════════════════════════════════════════
// ADAPTIVE MULTI-PHASE MOMENTUM STRATEGY (AMMS) - 2025
// Strategia matematica definitiva per trading automatico su Solana
// ═══════════════════════════════════════════════════════════════════════════════

// --- CONFIGURAZIONE INDICATORI ---
const EMA_FAST: usize = 20;          // EMA veloce (trend breve)
const EMA_SLOW: usize = 50;          // EMA lenta (trend medio)
const RSI_PERIOD: usize = 14;        // RSI standard
const ATR_PERIOD: usize = 14;        // Average True Range
const BOLLINGER_PERIOD: usize = 20;  // Bollinger Bands
const BOLLINGER_MULT: f64 = 2.0;     // Deviazione standard x2

// --- SOGLIE STRATEGIA ---
const RSI_OVERSOLD: f64 = 30.0;      // Ipervenduto
const RSI_OVERBOUGHT: f64 = 70.0;    // Ipercomprato
const RSI_MOMENTUM_LOW: f64 = 50.0;  // Momentum minimo per entry
const RSI_MOMENTUM_HIGH: f64 = 70.0; // Momentum massimo per entry
const MIN_VOLUME_USD: f64 = 50_000.0;
const MIN_LIQUIDITY_USD: f64 = 10_000.0;
const MIN_PUMP_5M: f64 = 2.0;        // Minimo +2% in 5 minuti

// --- STOP LOSS / TAKE PROFIT ---
const ATR_STOP_MULTIPLIER: f64 = 1.5;  // Stop Loss = Entry - 1.5 × ATR
const ATR_TP_MULTIPLIER: f64 = 2.0;    // Take Profit = Entry + 2 × ATR
const TRAILING_TRIGGER_PCT: f64 = 3.0; // Attiva trailing dopo +3%
const TRAILING_DROP_PCT: f64 = 3.0;    // Vendi se scende 3% dal massimo

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
    
    // Buffer per costruzione candela
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

        // Chiudi candela ogni 5 tick
        if self.tick_count >= 5 {
            self.candles.push_back(Candle {
                open: self.current_open,
                high: self.current_high,
                low: self.current_low,
                close: price,
                volume: self.current_vol
            });
            if self.candles.len() > 200 { self.candles.pop_front(); }
            
            // Reset
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

#[derive(Debug, PartialEq)]
pub enum TradeAction {
    Buy { amount_sol: f64, reason: String },
    Sell(String),
    Hold,
    UpdateHigh(u64),
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
    pub stop_loss: f64,
    pub take_profit: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Trend {
    StrongBullish,  // EMA20 >> EMA50
    Bullish,        // EMA20 > EMA50
    Neutral,        // EMA20 ≈ EMA50
    Bearish,        // EMA20 < EMA50
    StrongBearish,  // EMA20 << EMA50
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

/// Calcola EMA (Exponential Moving Average)
/// Formula: EMA = Price × k + EMA_prev × (1 - k), dove k = 2/(N+1)
fn calculate_ema(candles: &VecDeque<Candle>, period: usize) -> Option<f64> {
    if candles.len() < period { return None; }
    
    let k = 2.0 / (period as f64 + 1.0);
    let mut ema = candles.iter().take(period).map(|c| c.close).sum::<f64>() / period as f64;
    
    for candle in candles.iter().skip(period) {
        ema = candle.close * k + ema * (1.0 - k);
    }
    
    Some(ema)
}

/// Calcola RSI (Relative Strength Index)
/// Formula: RSI = 100 - (100 / (1 + RS)), dove RS = AvgGain / AvgLoss
fn calculate_rsi(candles: &VecDeque<Candle>) -> Option<f64> {
    if candles.len() < RSI_PERIOD + 1 { return None; }
    
    let mut gains: f64 = 0.0;
    let mut losses: f64 = 0.0;
    
    let start = candles.len() - RSI_PERIOD - 1;
    for i in start..candles.len() - 1 {
        let diff = candles[i + 1].close - candles[i].close;
        if diff >= 0.0 { 
            gains += diff; 
        } else { 
            losses += diff.abs(); 
        }
    }
    
    let avg_gain = gains / RSI_PERIOD as f64;
    let avg_loss = losses / RSI_PERIOD as f64;
    
    if avg_loss == 0.0 { return Some(100.0); }
    
    let rs = avg_gain / avg_loss;
    Some(100.0 - (100.0 / (1.0 + rs)))
}

/// Calcola ATR (Average True Range) - Misura la volatilità
/// Formula: ATR = SMA(TR), dove TR = max(H-L, |H-C_prev|, |L-C_prev|)
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

/// Calcola Bollinger Bands
/// Formula: Middle = SMA(20), Upper = Middle + 2×StdDev, Lower = Middle - 2×StdDev
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

/// Calcola la variazione percentuale
fn calculate_change_pct(old_price: f64, new_price: f64) -> f64 {
    if old_price == 0.0 { return 0.0; }
    ((new_price - old_price) / old_price) * 100.0
}

// ═══════════════════════════════════════════════════════════════════════════════
// ANALISI COMPLETA DEL MERCATO
// ═══════════════════════════════════════════════════════════════════════════════

/// Esegue analisi tecnica completa
pub fn analyze_market_full(data: &MarketData) -> Option<MarketAnalysis> {
    if data.candles.len() < EMA_SLOW + 10 { return None; }
    
    let ema_20 = calculate_ema(&data.candles, EMA_FAST)?;
    let ema_50 = calculate_ema(&data.candles, EMA_SLOW)?;
    let rsi = calculate_rsi(&data.candles)?;
    let atr = calculate_atr(&data.candles)?;
    let (bb_lower, bb_middle, bb_upper) = calculate_bollinger(&data.candles)?;
    
    let current_price = data.get_last_price();
    
    // Determina trend
    let ema_diff_pct = ((ema_20 - ema_50) / ema_50) * 100.0;
    let trend = if ema_diff_pct > 3.0 {
        Trend::StrongBullish
    } else if ema_diff_pct > 0.5 {
        Trend::Bullish
    } else if ema_diff_pct < -3.0 {
        Trend::StrongBearish
    } else if ema_diff_pct < -0.5 {
        Trend::Bearish
    } else {
        Trend::Neutral
    };
    
    // Determina segnale
    let signal = determine_signal(&trend, rsi, current_price, bb_upper, bb_lower, ema_20);
    
    // Calcola Stop Loss e Take Profit basati su ATR
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
        stop_loss,
        take_profit,
    })
}

/// Determina il segnale di trading
fn determine_signal(trend: &Trend, rsi: f64, price: f64, bb_upper: f64, bb_lower: f64, ema_20: f64) -> Signal {
    // STRONG BUY: Trend bullish + RSI momentum zone + prezzo sopra EMA + breakout BB
    if matches!(trend, Trend::Bullish | Trend::StrongBullish) 
       && rsi >= RSI_MOMENTUM_LOW && rsi <= RSI_MOMENTUM_HIGH
       && price > ema_20
       && price >= bb_upper * 0.98 // Vicino o sopra BB superiore
    {
        return Signal::StrongBuy;
    }
    
    // BUY: Trend bullish + RSI sano
    if matches!(trend, Trend::Bullish | Trend::StrongBullish) 
       && rsi >= 40.0 && rsi <= RSI_MOMENTUM_HIGH
       && price > ema_20
    {
        return Signal::Buy;
    }
    
    // STRONG SELL: Trend bearish + RSI overbought
    if matches!(trend, Trend::Bearish | Trend::StrongBearish)
       && rsi > RSI_OVERBOUGHT
    {
        return Signal::StrongSell;
    }
    
    // SELL: RSI molto alto o prezzo crolla sotto EMA
    if rsi > 80.0 || (price < ema_20 * 0.97 && rsi > 60.0) {
        return Signal::Sell;
    }
    
    Signal::Hold
}

// ═══════════════════════════════════════════════════════════════════════════════
// LOGICA DI ENTRATA (BUY) - AMMS
// ═══════════════════════════════════════════════════════════════════════════════

/// Struttura per dati esterni (da API)
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

/// Verifica se soddisfa le condizioni di entrata AMMS
pub fn check_entry_conditions(analysis: &MarketAnalysis, external: &ExternalData) -> (bool, String) {
    let mut reasons: Vec<&str> = Vec::new();
    let mut score: u8 = 0;
    
    // 1. EMA20 > EMA50 → Trend rialzista (REQUIRED)
    let trend_ok = analysis.ema_20 > analysis.ema_50;
    if trend_ok { 
        score += 20; 
        reasons.push("Trend ↑");
    } else {
        return (false, "❌ Trend ribassista".to_string());
    }
    
    // 2. RSI tra 50 e 70 → Momentum sano (REQUIRED)
    let rsi_ok = analysis.rsi >= RSI_MOMENTUM_LOW && analysis.rsi <= RSI_MOMENTUM_HIGH;
    if rsi_ok { 
        score += 20;
        reasons.push("RSI OK");
    } else if analysis.rsi > RSI_OVERBOUGHT {
        return (false, format!("❌ RSI overbought: {:.0}", analysis.rsi));
    }
    
    // 3. Δ prezzo 5min > 2% → Pump iniziale (IMPORTANT)
    let pump_ok = external.change_5m >= MIN_PUMP_5M;
    if pump_ok { 
        score += 15;
        reasons.push("Pump 5m ↑");
    }
    
    // 4. Volume 24h > 50k USD (REQUIRED)
    let volume_ok = external.volume_24h >= MIN_VOLUME_USD;
    if !volume_ok {
        return (false, format!("❌ Volume basso: ${:.0}", external.volume_24h));
    }
    score += 15;
    reasons.push("Vol OK");
    
    // 5. Liquidità > 10k USD (REQUIRED)
    let liq_ok = external.liquidity_usd >= MIN_LIQUIDITY_USD;
    if !liq_ok {
        return (false, format!("❌ Liquidità bassa: ${:.0}", external.liquidity_usd));
    }
    score += 10;
    reasons.push("Liq OK");
    
    // 6. Breakout Bollinger (BONUS)
    let breakout = external.price >= analysis.bb_upper * 0.98;
    if breakout { 
        score += 20;
        reasons.push("BB Breakout!");
    }
    
    // 7. ATR basso = bassa volatilità = rischio basso (BONUS)
    let low_volatility = analysis.atr < external.price * 0.03; // ATR < 3% del prezzo
    if low_volatility {
        score += 10;
        reasons.push("Low Vol");
    }
    
    // DECISIONE: Score minimo 60 per entrare
    let should_buy = score >= 60 && trend_ok && volume_ok && liq_ok;
    
    let reason = if should_buy {
        format!("✅ AMMS BUY [{}] - {}", score, reasons.join(", "))
    } else {
        format!("⏸️ Score: {} - {}", score, reasons.join(", "))
    };
    
    (should_buy, reason)
}

// ═══════════════════════════════════════════════════════════════════════════════
// STOP LOSS ADATTIVO E TRAILING (ATR-BASED)
// ═══════════════════════════════════════════════════════════════════════════════

/// Stato di una posizione aperta
#[derive(Debug, Clone)]
pub struct Position {
    pub entry_price: f64,
    pub entry_atr: f64,
    pub highest_price: f64,
    pub current_stop: f64,
    pub target_tp: f64,
    pub trailing_active: bool,
}

impl Position {
    pub fn new(entry_price: f64, atr: f64) -> Self {
        let stop_loss = entry_price - (ATR_STOP_MULTIPLIER * atr);
        let take_profit = entry_price + (ATR_TP_MULTIPLIER * atr);
        
        Self {
            entry_price,
            entry_atr: atr,
            highest_price: entry_price,
            current_stop: stop_loss,
            target_tp: take_profit,
            trailing_active: false,
        }
    }
    
    /// Aggiorna la posizione con il nuovo prezzo
    pub fn update(&mut self, current_price: f64, current_atr: f64) -> TradeAction {
        // Aggiorna massimo
        if current_price > self.highest_price {
            self.highest_price = current_price;
            
            // Attiva trailing dopo +3%
            let gain_pct = ((current_price - self.entry_price) / self.entry_price) * 100.0;
            if gain_pct >= TRAILING_TRIGGER_PCT {
                self.trailing_active = true;
            }
            
            // Trailing Stop: alza lo stop quando il prezzo sale
            if self.trailing_active {
                let new_stop = current_price - (ATR_STOP_MULTIPLIER * current_atr);
                if new_stop > self.current_stop {
                    self.current_stop = new_stop;
                    debug!("📈 Trailing stop aggiornato: ${:.6}", self.current_stop);
                }
            }
        }
        
        // Check STOP LOSS
        if current_price <= self.current_stop {
            let pnl_pct = ((current_price - self.entry_price) / self.entry_price) * 100.0;
            return TradeAction::Sell(format!(
                "🛑 STOP LOSS ATR | Entry: ${:.6} | Exit: ${:.6} | PnL: {:+.1}%",
                self.entry_price, current_price, pnl_pct
            ));
        }
        
        // Check DROP dal massimo (3%)
        if self.trailing_active {
            let drop_from_high = ((self.highest_price - current_price) / self.highest_price) * 100.0;
            if drop_from_high >= TRAILING_DROP_PCT {
                let pnl_pct = ((current_price - self.entry_price) / self.entry_price) * 100.0;
                return TradeAction::Sell(format!(
                    "📉 TRAILING STOP -{:.1}% dal max | Entry: ${:.6} | High: ${:.6} | Exit: ${:.6} | PnL: {:+.1}%",
                    drop_from_high, self.entry_price, self.highest_price, current_price, pnl_pct
                ));
            }
        }
        
        // Check TAKE PROFIT (opzionale, il trailing è meglio)
        if current_price >= self.target_tp && !self.trailing_active {
            // Non vendere subito al TP, attiva il trailing per catturare pump maggiori
            self.trailing_active = true;
            info!("🎯 Target TP raggiunto, trailing attivato");
        }
        
        TradeAction::Hold
    }
    
    /// Calcola P&L corrente in percentuale
    pub fn current_pnl_pct(&self, current_price: f64) -> f64 {
        ((current_price - self.entry_price) / self.entry_price) * 100.0
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MONEY MANAGEMENT INTELLIGENTE
// ═══════════════════════════════════════════════════════════════════════════════

const MIN_TRADE_SOL: f64 = 0.015;
const FEE_RESERVE_SOL: f64 = 0.003;

pub enum WealthLevel {
    Micro,      // < 0.03 SOL (~5€)
    Poor,       // 0.03 - 0.08 SOL (5-15€)
    LowMedium,  // 0.08 - 0.27 SOL (15-50€)
    Medium,     // 0.27 - 0.55 SOL (50-100€)
    HighMedium, // 0.55 - 1.1 SOL (100-200€)
    Rich,       // > 1.1 SOL (200€+)
}

pub fn get_wealth_level(balance_sol: f64) -> WealthLevel {
    if balance_sol < 0.03 { WealthLevel::Micro }
    else if balance_sol < 0.08 { WealthLevel::Poor }
    else if balance_sol < 0.27 { WealthLevel::LowMedium }
    else if balance_sol < 0.55 { WealthLevel::Medium }
    else if balance_sol < 1.1 { WealthLevel::HighMedium }
    else { WealthLevel::Rich }
}

/// Calcola importo da investire basato su ATR e wealth level
pub fn calculate_investment_amount(wallet_balance_sol: f64, atr_pct: Option<f64>) -> f64 {
    let safe_balance = (wallet_balance_sol - FEE_RESERVE_SOL).max(0.0);
    
    if safe_balance < MIN_TRADE_SOL {
        return 0.0;
    }
    
    // Risk adjustment basato su ATR
    // ATR alto = più volatilità = investi meno
    let risk_factor = match atr_pct {
        Some(atr) if atr > 5.0 => 0.7,   // Alta volatilità
        Some(atr) if atr > 3.0 => 0.85,  // Media volatilità
        _ => 1.0,                          // Bassa volatilità
    };
    
    let base_investment = match get_wealth_level(wallet_balance_sol) {
        WealthLevel::Micro => safe_balance * 0.95,
        WealthLevel::Poor => safe_balance * 0.88,
        WealthLevel::LowMedium => safe_balance * 0.75,
        WealthLevel::Medium => safe_balance * 0.55,
        WealthLevel::HighMedium => safe_balance * 0.40,
        WealthLevel::Rich => {
            let pct = if wallet_balance_sol < 3.0 { 0.25 } 
                     else if wallet_balance_sol < 5.0 { 0.18 } 
                     else { 0.12 };
            safe_balance * pct
        }
    };
    
    (base_investment * risk_factor).max(MIN_TRADE_SOL).min(safe_balance)
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

/// Analizza mercato e decide azione (versione completa AMMS)
pub fn analyze_market(data: &MarketData, wallet_balance: f64) -> TradeAction {
    let analysis = match analyze_market_full(data) {
        Some(a) => a,
        None => return TradeAction::Hold,
    };
    
    // Per ora usiamo dati fittizi per external (in produzione verranno da API)
    let current_price = data.get_last_price();
    let external = ExternalData {
        price: current_price,
        change_5m: 3.0,  // Placeholder
        change_1h: 5.0,
        change_24h: 10.0,
        volume_24h: 100_000.0,
        liquidity_usd: 50_000.0,
        market_cap: 1_000_000.0,
    };
    
    // Check condizioni di entrata
    let (should_buy, reason) = check_entry_conditions(&analysis, &external);
    
    if should_buy {
        let atr_pct = (analysis.atr / current_price) * 100.0;
        let invest_amount = calculate_investment_amount(wallet_balance, Some(atr_pct));
        
        if invest_amount > 0.0 {
            return TradeAction::Buy {
                amount_sol: invest_amount,
                reason,
            };
        }
    }
    
    // Check segnale di vendita
    if matches!(analysis.signal, Signal::Sell | Signal::StrongSell) {
        return TradeAction::Sell(format!("📉 {} | RSI: {:.0}", 
            if analysis.signal == Signal::StrongSell { "STRONG SELL" } else { "SELL" },
            analysis.rsi
        ));
    }
    
    TradeAction::Hold
}

/// Check posizione esistente con trailing stop ATR
pub fn check_position(current_val: u64, high_val: u64) -> TradeAction {
    let current = current_val as f64;
    let high = high_val as f64;
    
    if current > high { 
        return TradeAction::UpdateHigh(current_val); 
    }

    // Trailing stop dinamico basato sul profitto
    let drop_pct = ((high - current) / high) * 100.0;
    let profit_pct = ((high - current) / current) * 100.0;
    
    // Stop più stretto se in profitto
    let dynamic_stop = if profit_pct > 50.0 {
        2.0  // -2% dal max se +50% profit
    } else if profit_pct > 20.0 {
        3.0  // -3% dal max se +20% profit
    } else if profit_pct > 0.0 {
        5.0  // -5% dal max se in profit
    } else {
        10.0 // -10% se ancora in loss
    };

    if drop_pct >= dynamic_stop {
        return TradeAction::Sell(format!("🛑 Trailing Stop -{:.1}% (soglia: {:.1}%)", drop_pct, dynamic_stop));
    }
    
    TradeAction::Hold
}

// ═══════════════════════════════════════════════════════════════════════════════
// UTILITY FUNCTIONS
// ═══════════════════════════════════════════════════════════════════════════════

pub fn calculate_user_set_investment(wallet_balance_sol: f64, user_amount_sol: f64) -> f64 {
    let safe_balance = (wallet_balance_sol - FEE_RESERVE_SOL).max(0.0);
    let max_allowed = safe_balance * 0.95;
    user_amount_sol.min(max_allowed).max(0.0)
}

pub fn calculate_dynamic_stop(entry_price: f64, current_price: f64, highest_price: f64) -> f64 {
    let profit_from_entry = ((current_price - entry_price) / entry_price) * 100.0;
    
    if profit_from_entry > 100.0 { 5.0 }
    else if profit_from_entry > 50.0 { 8.0 }
    else if profit_from_entry > 20.0 { 12.0 }
    else if profit_from_entry > 0.0 { 15.0 }
    else { 25.0 }
}
