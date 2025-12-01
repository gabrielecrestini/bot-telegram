use log::{info, debug};
use std::collections::VecDeque;
use std::collections::HashMap;

// ═══════════════════════════════════════════════════════════════════════════════
// TRADING ENGINE - AMMS Implementation (DIP + BREAKOUT)
// ═══════════════════════════════════════════════════════════════════════════════

// Re-export types from strategy
pub use crate::strategy::{
    MarketData, MarketAnalysis, Position, ExternalData, TradeAction,
    Trend, Signal, WealthLevel, TradingMode,
    analyze_market_full, check_entry_conditions, calculate_investment_amount,
    calculate_breakout_investment, get_wealth_level, check_position,
};

// ═══════════════════════════════════════════════════════════════════════════════
// POSITION MANAGER - Gestisce posizioni aperte con trailing stop
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
    pub mode: TradingMode,
}

impl OpenPosition {
    pub fn new(
        id: i64, 
        token: &str, 
        entry_price: f64, 
        atr: f64, 
        amount_sol: f64, 
        amount_lamports: u64,
        mode: TradingMode,
    ) -> Self {
        // BREAKOUT usa stop più largo
        let stop_mult = if mode == TradingMode::Breakout { 2.0 } else { 1.5 };
        let tp_mult = if mode == TradingMode::Breakout { 3.0 } else { 2.0 };
        
        Self {
            id,
            token_address: token.to_string(),
            entry_price,
            entry_atr: atr,
            amount_sol,
            amount_lamports,
            highest_price: entry_price,
            current_stop: entry_price - (stop_mult * atr),
            target_tp: entry_price + (tp_mult * atr),
            trailing_active: false,
            entry_time: chrono::Utc::now().timestamp(),
            mode,
        }
    }
    
    pub fn update(&mut self, current_price: f64, current_atr: f64) -> PositionAction {
        let stop_mult = if self.mode == TradingMode::Breakout { 2.0 } else { 1.5 };
        let trailing_trigger = if self.mode == TradingMode::Breakout { 5.0 } else { 3.0 };
        let trailing_drop = if self.mode == TradingMode::Breakout { 4.0 } else { 3.0 };
        
        if current_price > self.highest_price {
            self.highest_price = current_price;
            
            let gain_pct = self.current_pnl_pct(current_price);
            if gain_pct >= trailing_trigger {
                self.trailing_active = true;
            }
            
            if self.trailing_active {
                let new_stop = current_price - (stop_mult * current_atr);
                if new_stop > self.current_stop {
                    self.current_stop = new_stop;
                    debug!("📈 [{}] Trailing: ${:.8}", &self.token_address[..8], self.current_stop);
                }
            }
        }
        
        let pnl_pct = self.current_pnl_pct(current_price);
        
        // Check STOP LOSS
        if current_price <= self.current_stop {
            let mode_str = match self.mode {
                TradingMode::Dip => "DIP",
                TradingMode::Breakout => "BREAKOUT",
                TradingMode::None => "TRADE",
            };
            return PositionAction::Sell {
                reason: format!("🛑 STOP {} | PnL: {:+.1}%", mode_str, pnl_pct),
                pnl_pct,
            };
        }
        
        // Check TRAILING DROP
        if self.trailing_active {
            let drop_from_high = ((self.highest_price - current_price) / self.highest_price) * 100.0;
            if drop_from_high >= trailing_drop {
                return PositionAction::Sell {
                    reason: format!(
                        "📉 TRAILING -{:.1}% | Max: ${:.8} | PnL: {:+.1}%",
                        drop_from_high, self.highest_price, pnl_pct
                    ),
                    pnl_pct,
                };
            }
        }
        
        // Attiva trailing al TP
        if current_price >= self.target_tp && !self.trailing_active {
            self.trailing_active = true;
            let mode_str = if self.mode == TradingMode::Breakout { "BREAKOUT" } else { "DIP" };
            info!("🎯 [{}] {} TP raggiunto +{:.1}%, trailing ON", 
                &self.token_address[..8], mode_str, pnl_pct);
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
// TRADE ANALYZER
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct TradeAnalysis {
    pub should_buy: bool,
    pub confidence: u8,
    pub reason: String,
    pub suggested_amount: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub atr: f64,
    pub risk_level: RiskLevel,
    pub mode: TradingMode,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RiskLevel {
    VeryLow,
    Low,
    Medium,
    High,
    VeryHigh,
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

pub fn analyze_token_for_trade(
    analysis: &MarketAnalysis,
    external: &ExternalData,
    wallet_balance_sol: f64,
) -> TradeAnalysis {
    let (should_buy, reason) = check_entry_conditions(analysis, external);
    
    let mut confidence: u8 = 0;
    
    // Mode-based confidence
    match analysis.mode {
        TradingMode::Breakout => {
            confidence += 10; // Base per breakout
            if external.change_24h > 15.0 { confidence += 15; }
            if external.change_1h > 5.0 { confidence += 15; }
            if analysis.volume_ratio > 2.0 { confidence += 20; }
        },
        TradingMode::Dip => {
            confidence += 15; // Base per dip
            if analysis.rsi < 30.0 { confidence += 20; }
            if external.price < analysis.bb_lower { confidence += 15; }
        },
        TradingMode::None => {}
    }
    
    // Trend
    match analysis.trend {
        Trend::StrongBullish => confidence += 20,
        Trend::Bullish => confidence += 15,
        _ => {}
    }
    
    // Volume
    if external.volume_24h > 200_000.0 { confidence += 10; }
    if external.liquidity_usd > 100_000.0 { confidence += 10; }
    
    confidence = confidence.min(100);
    
    let atr_pct = (analysis.atr / external.price) * 100.0;
    let risk_level = RiskLevel::from_atr_pct(atr_pct);
    
    let base_amount = match analysis.mode {
        TradingMode::Breakout => calculate_breakout_investment(wallet_balance_sol, Some(atr_pct)),
        _ => calculate_investment_amount(wallet_balance_sol, Some(atr_pct)),
    };
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
        mode: analysis.mode,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PORTFOLIO STATS
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
    pub current_streak: i32,
    pub dip_trades: u32,
    pub breakout_trades: u32,
    pub dip_wins: u32,
    pub breakout_wins: u32,
}

impl PortfolioStats {
    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 { return 0.0; }
        (self.winning_trades as f64 / self.total_trades as f64) * 100.0
    }
    
    pub fn record_trade(&mut self, pnl_pct: f64, pnl_sol: f64, hold_time_mins: f64, mode: TradingMode) {
        self.total_trades += 1;
        self.total_pnl_sol += pnl_sol;
        
        match mode {
            TradingMode::Dip => self.dip_trades += 1,
            TradingMode::Breakout => self.breakout_trades += 1,
            _ => {}
        }
        
        if pnl_pct > 0.0 {
            self.winning_trades += 1;
            self.current_streak = if self.current_streak > 0 { self.current_streak + 1 } else { 1 };
            
            match mode {
                TradingMode::Dip => self.dip_wins += 1,
                TradingMode::Breakout => self.breakout_wins += 1,
                _ => {}
            }
        } else {
            self.losing_trades += 1;
            self.current_streak = if self.current_streak < 0 { self.current_streak - 1 } else { -1 };
        }
        
        if pnl_pct > self.best_trade_pct { self.best_trade_pct = pnl_pct; }
        if pnl_pct < self.worst_trade_pct { self.worst_trade_pct = pnl_pct; }
        
        let total_time = self.avg_hold_time_mins * (self.total_trades - 1) as f64;
        self.avg_hold_time_mins = (total_time + hold_time_mins) / self.total_trades as f64;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MARKET DATA AGGREGATOR
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

pub fn calculate_reinvestment(
    new_balance_sol: f64,
    last_pnl_pct: f64,
    stats: &PortfolioStats,
) -> (f64, String) {
    let wealth = get_wealth_level(new_balance_sol);
    
    let streak_factor = match stats.current_streak {
        s if s >= 3 => 1.15,
        s if s >= 1 => 1.05,
        0 => 1.0,
        s if s <= -3 => 0.70,
        _ => 0.85,
    };
    
    let momentum_factor = if last_pnl_pct > 20.0 { 1.1 }
        else if last_pnl_pct > 0.0 { 1.0 }
        else if last_pnl_pct > -10.0 { 0.9 }
        else { 0.75 };
    
    let base = calculate_investment_amount(new_balance_sol, None);
    let adjusted = base * streak_factor * momentum_factor;
    
    let strategy_desc = match wealth {
        WealthLevel::Micro | WealthLevel::Poor => {
            format!("🔥 FULL-RISK: {:.1}% (streak: {})", 
                (adjusted / new_balance_sol) * 100.0, 
                if stats.current_streak > 0 { format!("+{}", stats.current_streak) } 
                else { stats.current_streak.to_string() }
            )
        },
        WealthLevel::LowMedium | WealthLevel::Medium => {
            format!("⚖️ BALANCED: {:.1}%", (adjusted / new_balance_sol) * 100.0)
        },
        WealthLevel::HighMedium | WealthLevel::Rich => {
            format!("🛡️ SAFE: {:.1}%", (adjusted / new_balance_sol) * 100.0)
        }
    };
    
    (adjusted.max(0.015), strategy_desc)
}

// ═══════════════════════════════════════════════════════════════════════════════
// LEGACY COMPATIBILITY
// ═══════════════════════════════════════════════════════════════════════════════

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
