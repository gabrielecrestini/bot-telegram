use log::{info, warn, debug};
use std::collections::VecDeque;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ═══════════════════════════════════════════════════════════════════════════════
// TRADING ENGINE - AMMS Implementation
// ═══════════════════════════════════════════════════════════════════════════════

// Re-export types from strategy
pub use crate::strategy::{
    MarketData, MarketAnalysis, Position, ExternalData, TradeAction,
    Trend, Signal, WealthLevel,
    analyze_market_full, check_entry_conditions, calculate_investment_amount,
    get_wealth_level, check_position,
};

// ═══════════════════════════════════════════════════════════════════════════════
// POSITION MANAGER - Gestisce posizioni aperte con trailing stop ATR
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug)]
pub struct OpenPosition {
    pub id: i64,
    pub token_address: String,
    pub entry_price: f64,
    pub entry_atr: f64,
    pub amount_sol: f64,
    pub amount_lamports: u64,
    pub highest_price: f64,
    pub current_stop: f64,
    pub target_tp: f64,
    pub trailing_active: bool,
    pub entry_time: i64,
}

impl OpenPosition {
    pub fn new(
        id: i64, 
        token: &str, 
        entry_price: f64, 
        atr: f64, 
        amount_sol: f64, 
        amount_lamports: u64
    ) -> Self {
        let atr_stop_mult = 1.5;
        let atr_tp_mult = 2.0;
        
        Self {
            id,
            token_address: token.to_string(),
            entry_price,
            entry_atr: atr,
            amount_sol,
            amount_lamports,
            highest_price: entry_price,
            current_stop: entry_price - (atr_stop_mult * atr),
            target_tp: entry_price + (atr_tp_mult * atr),
            trailing_active: false,
            entry_time: chrono::Utc::now().timestamp(),
        }
    }
    
    /// Aggiorna la posizione con nuovo prezzo e ATR, ritorna azione da fare
    pub fn update(&mut self, current_price: f64, current_atr: f64) -> PositionAction {
        const ATR_STOP_MULT: f64 = 1.5;
        const TRAILING_TRIGGER_PCT: f64 = 3.0;
        const TRAILING_DROP_PCT: f64 = 3.0;
        
        // Aggiorna massimo
        if current_price > self.highest_price {
            self.highest_price = current_price;
            
            // Attiva trailing dopo +3%
            let gain_pct = self.current_pnl_pct(current_price);
            if gain_pct >= TRAILING_TRIGGER_PCT {
                self.trailing_active = true;
            }
            
            // Trailing Stop: alza lo stop quando il prezzo sale
            if self.trailing_active {
                let new_stop = current_price - (ATR_STOP_MULT * current_atr);
                if new_stop > self.current_stop {
                    self.current_stop = new_stop;
                    debug!("📈 [{}] Trailing stop: ${:.8}", self.token_address[..8].to_string(), self.current_stop);
                }
            }
        }
        
        let pnl_pct = self.current_pnl_pct(current_price);
        
        // Check STOP LOSS
        if current_price <= self.current_stop {
            return PositionAction::Sell {
                reason: format!(
                    "🛑 STOP LOSS ATR | PnL: {:+.1}%",
                    pnl_pct
                ),
                pnl_pct,
            };
        }
        
        // Check DROP dal massimo (3%)
        if self.trailing_active {
            let drop_from_high = ((self.highest_price - current_price) / self.highest_price) * 100.0;
            if drop_from_high >= TRAILING_DROP_PCT {
                return PositionAction::Sell {
                    reason: format!(
                        "📉 TRAILING -{:.1}% | Max: ${:.8} | PnL: {:+.1}%",
                        drop_from_high, self.highest_price, pnl_pct
                    ),
                    pnl_pct,
                };
            }
        }
        
        // Check TAKE PROFIT -> attiva trailing invece di vendere
        if current_price >= self.target_tp && !self.trailing_active {
            self.trailing_active = true;
            info!("🎯 [{}] TP raggiunto +{:.1}%, trailing ON", 
                &self.token_address[..8], pnl_pct);
        }
        
        PositionAction::Hold
    }
    
    pub fn current_pnl_pct(&self, current_price: f64) -> f64 {
        ((current_price - self.entry_price) / self.entry_price) * 100.0
    }
    
    pub fn current_pnl_sol(&self, current_price: f64) -> f64 {
        let pnl_pct = self.current_pnl_pct(current_price);
        self.amount_sol * (pnl_pct / 100.0)
    }
}

#[derive(Debug, Clone)]
pub enum PositionAction {
    Hold,
    Sell { reason: String, pnl_pct: f64 },
}

// ═══════════════════════════════════════════════════════════════════════════════
// TRADE ANALYZER - Analisi completa per decisione trading
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct TradeAnalysis {
    pub should_buy: bool,
    pub confidence: u8,        // 0-100
    pub reason: String,
    pub suggested_amount: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub atr: f64,
    pub risk_level: RiskLevel,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RiskLevel {
    VeryLow,   // ATR < 2%
    Low,       // ATR 2-3%
    Medium,    // ATR 3-5%
    High,      // ATR 5-8%
    VeryHigh,  // ATR > 8%
}

impl RiskLevel {
    pub fn from_atr_pct(atr_pct: f64) -> Self {
        if atr_pct < 2.0 { RiskLevel::VeryLow }
        else if atr_pct < 3.0 { RiskLevel::Low }
        else if atr_pct < 5.0 { RiskLevel::Medium }
        else if atr_pct < 8.0 { RiskLevel::High }
        else { RiskLevel::VeryHigh }
    }
    
    pub fn adjustment_factor(&self) -> f64 {
        match self {
            RiskLevel::VeryLow => 1.1,
            RiskLevel::Low => 1.0,
            RiskLevel::Medium => 0.85,
            RiskLevel::High => 0.65,
            RiskLevel::VeryHigh => 0.45,
        }
    }
}

/// Analizza un token per decidere se comprare
pub fn analyze_token_for_trade(
    analysis: &MarketAnalysis,
    external: &ExternalData,
    wallet_balance_sol: f64,
) -> TradeAnalysis {
    let (should_buy, reason) = check_entry_conditions(analysis, external);
    
    // Calcola confidence basato sui segnali
    let mut confidence: u8 = 0;
    
    // Trend
    match analysis.trend {
        Trend::StrongBullish => confidence += 25,
        Trend::Bullish => confidence += 18,
        Trend::Neutral => confidence += 8,
        _ => {}
    }
    
    // RSI
    if analysis.rsi >= 50.0 && analysis.rsi <= 65.0 {
        confidence += 20;
    } else if analysis.rsi >= 45.0 && analysis.rsi <= 70.0 {
        confidence += 12;
    }
    
    // Volume
    if external.volume_24h > 200_000.0 {
        confidence += 15;
    } else if external.volume_24h > 50_000.0 {
        confidence += 10;
    }
    
    // Liquidità
    if external.liquidity_usd > 100_000.0 {
        confidence += 15;
    } else if external.liquidity_usd > 25_000.0 {
        confidence += 8;
    }
    
    // Momentum 5m
    if external.change_5m > 5.0 {
        confidence += 15;
    } else if external.change_5m > 2.0 {
        confidence += 10;
    }
    
    // Breakout Bollinger
    if external.price >= analysis.bb_upper * 0.98 {
        confidence += 10;
    }
    
    confidence = confidence.min(100);
    
    // Risk level
    let atr_pct = (analysis.atr / external.price) * 100.0;
    let risk_level = RiskLevel::from_atr_pct(atr_pct);
    
    // Calculate suggested amount with risk adjustment
    let base_amount = calculate_investment_amount(wallet_balance_sol, Some(atr_pct));
    let adjusted_amount = base_amount * risk_level.adjustment_factor();
    
    TradeAnalysis {
        should_buy,
        confidence,
        reason,
        suggested_amount: adjusted_amount.max(0.0),
        stop_loss: analysis.stop_loss,
        take_profit: analysis.take_profit,
        atr: analysis.atr,
        risk_level,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PORTFOLIO STATS - Statistiche performance
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Default)]
pub struct PortfolioStats {
    pub total_trades: u32,
    pub winning_trades: u32,
    pub losing_trades: u32,
    pub total_pnl_sol: f64,
    pub best_trade_pct: f64,
    pub worst_trade_pct: f64,
    pub avg_hold_time_mins: f64,
    pub current_streak: i32,  // + per wins, - per losses
}

impl PortfolioStats {
    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 { return 0.0; }
        (self.winning_trades as f64 / self.total_trades as f64) * 100.0
    }
    
    pub fn record_trade(&mut self, pnl_pct: f64, pnl_sol: f64, hold_time_mins: f64) {
        self.total_trades += 1;
        self.total_pnl_sol += pnl_sol;
        
        if pnl_pct > 0.0 {
            self.winning_trades += 1;
            if self.current_streak > 0 {
                self.current_streak += 1;
            } else {
                self.current_streak = 1;
            }
        } else {
            self.losing_trades += 1;
            if self.current_streak < 0 {
                self.current_streak -= 1;
            } else {
                self.current_streak = -1;
            }
        }
        
        if pnl_pct > self.best_trade_pct {
            self.best_trade_pct = pnl_pct;
        }
        if pnl_pct < self.worst_trade_pct {
            self.worst_trade_pct = pnl_pct;
        }
        
        // Update average hold time
        let total_time = self.avg_hold_time_mins * (self.total_trades - 1) as f64;
        self.avg_hold_time_mins = (total_time + hold_time_mins) / self.total_trades as f64;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MARKET DATA AGGREGATOR - Aggrega dati da multiple fonti
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
pub struct MarketDataAggregator {
    pub data_map: HashMap<String, MarketData>,
    pub last_update: i64,
}

impl MarketDataAggregator {
    pub fn new() -> Self {
        Self {
            data_map: HashMap::new(),
            last_update: 0,
        }
    }
    
    pub fn update_price(&mut self, token: &str, price: f64, volume: f64) {
        let data = self.data_map
            .entry(token.to_string())
            .or_insert_with(|| MarketData::new(token));
        
        data.add_tick(price, volume);
        self.last_update = chrono::Utc::now().timestamp();
    }
    
    pub fn get_analysis(&self, token: &str) -> Option<MarketAnalysis> {
        self.data_map.get(token).and_then(|d| analyze_market_full(d))
    }
    
    pub fn get_last_price(&self, token: &str) -> Option<f64> {
        self.data_map.get(token).map(|d| d.get_last_price())
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// REINVESTMENT CALCULATOR
// ═══════════════════════════════════════════════════════════════════════════════

/// Calcola quanto reinvestire dopo una vendita
pub fn calculate_reinvestment(
    new_balance_sol: f64,
    last_pnl_pct: f64,
    stats: &PortfolioStats,
) -> (f64, String) {
    let wealth = get_wealth_level(new_balance_sol);
    
    // Aggiusta percentuale basata su win streak
    let streak_factor = match stats.current_streak {
        s if s >= 3 => 1.15,   // 3+ wins: +15% confidence
        s if s >= 1 => 1.05,   // winning: +5%
        0 => 1.0,
        s if s <= -3 => 0.70,  // 3+ losses: -30% (cautela)
        _ => 0.85,             // losing: -15%
    };
    
    // Aggiusta basato su ultimo trade
    let momentum_factor = if last_pnl_pct > 20.0 {
        1.1  // Grande win, capitalizza
    } else if last_pnl_pct > 0.0 {
        1.0
    } else if last_pnl_pct > -10.0 {
        0.9  // Piccola loss, cautela
    } else {
        0.75 // Grande loss, molto cauto
    };
    
    let base = calculate_investment_amount(new_balance_sol, None);
    let adjusted = base * streak_factor * momentum_factor;
    
    let strategy_desc = match wealth {
        WealthLevel::Micro | WealthLevel::Poor => {
            format!("🔥 FULL-RISK: {:.1}% balance (streak: {})", 
                (adjusted / new_balance_sol) * 100.0, 
                if stats.current_streak > 0 { format!("+{}", stats.current_streak) } else { stats.current_streak.to_string() }
            )
        },
        WealthLevel::LowMedium | WealthLevel::Medium => {
            format!("⚖️ MEDIUM-RISK: {:.1}% balance", (adjusted / new_balance_sol) * 100.0)
        },
        WealthLevel::HighMedium | WealthLevel::Rich => {
            format!("🛡️ DIVERSIFIED: {:.1}% balance (max protection)", (adjusted / new_balance_sol) * 100.0)
        }
    };
    
    (adjusted.max(0.015), strategy_desc) // Min 0.015 SOL
}

// ═══════════════════════════════════════════════════════════════════════════════
// LEGACY COMPATIBILITY
// ═══════════════════════════════════════════════════════════════════════════════

// Mantieni compatibilità con vecchio codice
pub fn calculate_rsi(prices: &VecDeque<f64>) -> Option<f64> {
    if prices.len() < 15 { return None; }
    
    let mut gains = 0.0;
    let mut losses = 0.0;
    
    for i in (prices.len() - 14)..prices.len() {
        let diff = prices[i] - prices[i - 1];
        if diff >= 0.0 { gains += diff; } 
        else { losses += diff.abs(); }
    }
    
    if losses == 0.0 { return Some(100.0); }
    
    let rs = (gains / 14.0) / (losses / 14.0);
    Some(100.0 - (100.0 / (1.0 + rs)))
}

pub fn calculate_sma(prices: &VecDeque<f64>) -> Option<f64> {
    if prices.len() < 20 { return None; }
    let sum: f64 = prices.iter().rev().take(20).sum();
    Some(sum / 20.0)
}
