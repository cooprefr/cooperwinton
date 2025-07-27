use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};
use serde::{Deserialize, Serialize};

// FIXED IMPORTS - use crate instead of super
use crate::dex_tick_stream::{Tick, TickStream, Side, Protocol};
use solana_program::pubkey::Pubkey;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candle {
    pub timestamp: u64,      // Start time of candle (Unix timestamp)
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,         // Total volume in base token
    pub trades: u32,         // Number of trades
    pub pool_id: Pubkey,
    pub protocol: Protocol,
}

impl Default for Candle {
    fn default() -> Self {
        Self {
            timestamp: 0,
            open: 0.0,
            high: 0.0,
            low: f64::MAX,
            close: 0.0,
            volume: 0.0,
            trades: 0,
            pool_id: Pubkey::default(),
            protocol: Protocol::Meteora,
        }
    }
}

impl Candle {
    pub fn new(timestamp: u64, pool_id: Pubkey, protocol: Protocol) -> Self {
        Self {
            timestamp,
            pool_id,
            protocol,
            low: f64::MAX,
            ..Default::default()
        }
    }

    pub fn update_with_tick(&mut self, tick: &Tick) {
        if self.open == 0.0 {
            self.open = tick.price;
        }
        
        self.high = self.high.max(tick.price);
        self.low = if self.low == f64::MAX { tick.price } else { self.low.min(tick.price) };
        self.close = tick.price;
        self.volume += tick.size;
        self.trades += 1;
    }

    pub fn is_bullish(&self) -> bool {
        self.close > self.open
    }

    pub fn body_size(&self) -> f64 {
        (self.close - self.open).abs()
    }

    pub fn upper_wick(&self) -> f64 {
        self.high - self.open.max(self.close)
    }

    pub fn lower_wick(&self) -> f64 {
        self.open.min(self.close) - self.low
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extrema {
    pub timestamp: u64,
    pub price: f64,
    pub is_maximum: bool,    // true for local maxima, false for minima
    pub strength: f64,       // How significant this extrema is (0.0 to 1.0)
    pub candle_index: usize, // Index in the sliding window
}

#[derive(Debug, Clone)]
pub struct TechnicalIndicators {
    pub sma_20: Option<f64>,     // 20-period Simple Moving Average
    pub ema_12: Option<f64>,     // 12-period Exponential Moving Average
    pub ema_26: Option<f64>,     // 26-period Exponential Moving Average
    pub macd: Option<f64>,       // MACD line (EMA12 - EMA26)
    pub rsi: Option<f64>,        // 14-period RSI
    pub bollinger_upper: Option<f64>,
    pub bollinger_lower: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct CandleUpdate {
    pub candle: Candle,
    pub extrema: Vec<Extrema>,
    pub indicators: TechnicalIndicators,
    pub is_new_candle: bool,
}

pub struct OhlcAggregator {
    timeframe_seconds: u64,
    max_candles: usize,
    candles: VecDeque<Candle>,
    current_candle: Option<Candle>,
    current_candle_start: u64,
    sender: broadcast::Sender<CandleUpdate>,
    
    // Technical analysis state
    rsi_gains: VecDeque<f64>,
    rsi_losses: VecDeque<f64>,
    ema_12_prev: Option<f64>,
    ema_26_prev: Option<f64>,
}

impl OhlcAggregator {
    pub fn new(timeframe_seconds: u64, max_candles: usize) -> (Self, broadcast::Receiver<CandleUpdate>) {
        let (sender, receiver) = broadcast::channel(1000);
        
        (Self {
            timeframe_seconds,
            max_candles,
            candles: VecDeque::with_capacity(max_candles),
            current_candle: None,
            current_candle_start: 0,
            sender,
            rsi_gains: VecDeque::with_capacity(14),
            rsi_losses: VecDeque::with_capacity(14),
            ema_12_prev: None,
            ema_26_prev: None,
        }, receiver)
    }

    pub fn process_tick(&mut self, tick: Tick) -> Result<()> {
        let candle_timestamp = self.get_candle_timestamp(tick.timestamp);
        let mut is_new_candle = false;

        // Check if we need to start a new candle
        if self.current_candle.is_none() || candle_timestamp > self.current_candle_start {
            if let Some(completed_candle) = self.current_candle.take() {
                self.add_completed_candle(completed_candle);
            }
            
            self.current_candle = Some(Candle::new(candle_timestamp, tick.pool_id, tick.protocol.clone()));
            self.current_candle_start = candle_timestamp;
            is_new_candle = true;
        }

        // Update current candle with tick
        if let Some(ref mut candle) = self.current_candle {
            candle.update_with_tick(&tick);
            
            // Calculate extrema and indicators
            let extrema = self.detect_extrema();
            let indicators = self.calculate_indicators();
            
            let update = CandleUpdate {
                candle: candle.clone(),
                extrema,
                indicators,
                is_new_candle,
            };

            // Send update to subscribers
            let _ = self.sender.send(update);
        }

        Ok(())
    }

    fn get_candle_timestamp(&self, tick_timestamp: u64) -> u64 {
        (tick_timestamp / (self.timeframe_seconds * 1000)) * (self.timeframe_seconds * 1000)
    }

    fn add_completed_candle(&mut self, candle: Candle) {
        self.candles.push_back(candle);
        
        if self.candles.len() > self.max_candles {
            self.candles.pop_front();
        }
    }

    fn detect_extrema(&self) -> Vec<Extrema> {
        let mut extrema = Vec::new();
        
        if self.candles.len() < 5 {
            return extrema; // Need at least 5 candles for extrema detection
        }

        let window_size = 5; // Look at 5 candles around each point
        let candles_vec: Vec<&Candle> = self.candles.iter().collect();

        for i in window_size..candles_vec.len().saturating_sub(window_size) {
            let current = candles_vec[i];
            let mut is_local_max = true;
            let mut is_local_min = true;

            // Check if current candle's high is a local maximum
            for j in i.saturating_sub(window_size)..=i + window_size {
                if j != i && candles_vec[j].high >= current.high {
                    is_local_max = false;
                }
                if j != i && candles_vec[j].low <= current.low {
                    is_local_min = false;
                }
            }

            // Calculate strength based on price difference
            if is_local_max {
                let strength = self.calculate_extrema_strength(i, true, &candles_vec);
                extrema.push(Extrema {
                    timestamp: current.timestamp,
                    price: current.high,
                    is_maximum: true,
                    strength,
                    candle_index: i,
                });
            }

            if is_local_min {
                let strength = self.calculate_extrema_strength(i, false, &candles_vec);
                extrema.push(Extrema {
                    timestamp: current.timestamp,
                    price: current.low,
                    is_maximum: false,
                    strength,
                    candle_index: i,
                });
            }
        }

        extrema
    }

    fn calculate_extrema_strength(&self, index: usize, is_max: bool, candles: &[&Candle]) -> f64 {
        let current_price = if is_max { candles[index].high } else { candles[index].low };
        let window = 10.min(candles.len() / 2);
        
        let start = index.saturating_sub(window);
        let end = (index + window).min(candles.len());
        
        let mut max_diff = 0.0;
        for i in start..end {
            if i != index {
                let compare_price = if is_max { candles[i].high } else { candles[i].low };
                let diff = (current_price - compare_price).abs();
                max_diff = max_diff.max(diff);
            }
        }

        // Normalize strength to 0-1 range
        (max_diff / current_price).min(1.0)
    }

    fn calculate_indicators(&mut self) -> TechnicalIndicators {
        let closes: Vec<f64> = self.candles.iter().map(|c| c.close).collect();
        
        TechnicalIndicators {
            sma_20: self.calculate_sma(&closes, 20),
            ema_12: self.calculate_ema(&closes, 12, &mut self.ema_12_prev),
            ema_26: self.calculate_ema(&closes, 26, &mut self.ema_26_prev),
            macd: self.calculate_macd(),
            rsi: self.calculate_rsi(&closes),
            bollinger_upper: None, // Simplified for now
            bollinger_lower: None,
        }
    }

    fn calculate_sma(&self, prices: &[f64], period: usize) -> Option<f64> {
        if prices.len() < period {
            return None;
        }
        
        let sum: f64 = prices.iter().rev().take(period).sum();
        Some(sum / period as f64)
    }

    fn calculate_ema(&self, prices: &[f64], period: usize, prev_ema: &mut Option<f64>) -> Option<f64> {
        if prices.is_empty() {
            return None;
        }

        let multiplier = 2.0 / (period as f64 + 1.0);
        let current_price = *prices.last().unwrap();

        match prev_ema {
            Some(prev) => {
                let ema = (current_price * multiplier) + (*prev * (1.0 - multiplier));
                *prev_ema = Some(ema);
                Some(ema)
            }
            None => {
                if prices.len() >= period {
                    let sma = self.calculate_sma(prices, period).unwrap();
                    *prev_ema = Some(sma);
                    Some(sma)
                } else {
                    None
                }
            }
        }
    }

    fn calculate_macd(&self) -> Option<f64> {
        match (self.ema_12_prev, self.ema_26_prev) {
            (Some(ema12), Some(ema26)) => Some(ema12 - ema26),
            _ => None,
        }
    }

    fn calculate_rsi(&mut self, prices: &[f64]) -> Option<f64> {
        if prices.len() < 2 {
            return None;
        }

        // Calculate price change
        let current_change = prices[prices.len() - 1] - prices[prices.len() - 2];
        
        if current_change > 0.0 {
            self.rsi_gains.push_back(current_change);
            self.rsi_losses.push_back(0.0);
        } else {
            self.rsi_gains.push_back(0.0);
            self.rsi_losses.push_back(-current_change);
        }

        // Keep only last 14 periods
        if self.rsi_gains.len() > 14 {
            self.rsi_gains.pop_front();
            self.rsi_losses.pop_front();
        }

        if self.rsi_gains.len() < 14 {
            return None;
        }

        let avg_gain: f64 = self.rsi_gains.iter().sum::<f64>() / 14.0;
        let avg_loss: f64 = self.rsi_losses.iter().sum::<f64>() / 14.0;

        if avg_loss == 0.0 {
            return Some(100.0);
        }

        let rs = avg_gain / avg_loss;
        Some(100.0 - (100.0 / (1.0 + rs)))
    }

    pub fn get_recent_candles(&self, count: usize) -> Vec<&Candle> {
        self.candles.iter().rev().take(count).collect()
    }
}

// GUI Components using egui
use eframe::egui;
use egui_plot::{Bar, BarChart, Plot, PlotPoints, Line};

#[derive(Default)]
pub struct CandlestickGui {
    candles: Vec<Candle>,
    extrema: Vec<Extrema>,
    indicators: TechnicalIndicators,
    show_volume: bool,
    show_extrema: bool,
    show_indicators: bool,
    timeframe: String,
    last_update: SystemTime,
}

impl eframe::App for CandlestickGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Real-time Candlestick Chart");
            
            // Controls
            ui.horizontal(|ui| {
                ui.label("Timeframe:");
                egui::ComboBox::from_label("")
                    .selected_text(&self.timeframe)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.timeframe, "1s".to_owned(), "1 Second");
                        ui.selectable_value(&mut self.timeframe, "5s".to_owned(), "5 Seconds");
                        ui.selectable_value(&mut self.timeframe, "15s".to_owned(), "15 Seconds");
                        ui.selectable_value(&mut self.timeframe, "1m".to_owned(), "1 Minute");
                    });
                
                ui.checkbox(&mut self.show_volume, "Volume");
                ui.checkbox(&mut self.show_extrema, "Extrema");
                ui.checkbox(&mut self.show_indicators, "Indicators");
                
                ui.label(format!("Last Update: {:?}", 
                    self.last_update.elapsed().unwrap_or(Duration::ZERO)));
            });

            ui.separator();

            // Main chart
            let plot = Plot::new("candlestick_plot")
                .height(400.0)
                .show_background(false)
                .allow_boxed_zoom(true)
                .allow_drag(true);

            plot.show(ui, |plot_ui| {
                // Draw candlesticks
                self.draw_candlesticks(plot_ui);
                
                // Draw extrema points
                if self.show_extrema {
                    self.draw_extrema(plot_ui);
                }
                
                // Draw technical indicators
                if self.show_indicators {
                    self.draw_indicators(plot_ui);
                }
            });

            // Volume chart
            if self.show_volume {
                ui.separator();
                let volume_plot = Plot::new("volume_plot")
                    .height(150.0)
                    .show_background(false);
                
                volume_plot.show(ui, |plot_ui| {
                    self.draw_volume(plot_ui);
                });
            }

            // Statistics panel
            ui.separator();
            self.draw_statistics(ui);
        });

        // Request continuous updates
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

impl CandlestickGui {
    pub fn new() -> Self {
        Self {
            timeframe: "1s".to_owned(),
            last_update: SystemTime::now(),
            ..Default::default()
        }
    }

    pub fn update_data(&mut self, update: CandleUpdate) {
        if update.is_new_candle {
            self.candles.push(update.candle);
            // Keep only last 100 candles for performance
            if self.candles.len() > 100 {
                self.candles.remove(0);
            }
        } else if let Some(last_candle) = self.candles.last_mut() {
            *last_candle = update.candle;
        }
        
        self.extrema = update.extrema;
        self.indicators = update.indicators;
        self.last_update = SystemTime::now();
    }

    fn draw_candlesticks(&self, plot_ui: &mut egui_plot::PlotUi) {
        for (i, candle) in self.candles.iter().enumerate() {
            let x = i as f64;
            let color = if candle.is_bullish() { 
                egui::Color32::GREEN 
            } else { 
                egui::Color32::RED 
            };

            // Draw candle body
            let body_bars = vec![
                Bar::new(x, candle.body_size())
                    .width(0.8)
                    .fill(color)
                    .base_offset(candle.open.min(candle.close))
            ];
            
            plot_ui.bar_chart(BarChart::new(body_bars));

            // Draw wicks
            let wick_points = PlotPoints::new(vec![
                [x, candle.low],
                [x, candle.high],
            ]);
            plot_ui.line(Line::new(wick_points).color(color).width(1.0));
        }
    }

    fn draw_extrema(&self, plot_ui: &mut egui_plot::PlotUi) {
        for extrema in &self.extrema {
            let color = if extrema.is_maximum { 
                egui::Color32::YELLOW 
            } else { 
                egui::Color32::BLUE 
            };
            
            let points = PlotPoints::new(vec![[extrema.candle_index as f64, extrema.price]]);
            plot_ui.points(egui_plot::Points::new(points).color(color).radius(4.0));
        }
    }

    fn draw_indicators(&self, plot_ui: &mut egui_plot::PlotUi) {
        // Draw SMA line
        if let Some(sma) = self.indicators.sma_20 {
            let sma_points: Vec<[f64; 2]> = (0..self.candles.len())
                .map(|i| [i as f64, sma])
                .collect();
            plot_ui.line(Line::new(PlotPoints::new(sma_points))
                .color(egui::Color32::BLUE)
                .width(2.0));
        }

        // Draw EMA lines
        if let Some(ema12) = self.indicators.ema_12 {
            let ema_points: Vec<[f64; 2]> = (0..self.candles.len())
                .map(|i| [i as f64, ema12])
                .collect();
            plot_ui.line(Line::new(PlotPoints::new(ema_points))
                .color(egui::Color32::ORANGE)
                .width(1.5));
        }
    }

    fn draw_volume(&self, plot_ui: &mut egui_plot::PlotUi) {
        let volume_bars: Vec<Bar> = self.candles.iter().enumerate()
            .map(|(i, candle)| {
                let color = if candle.is_bullish() { 
                    egui::Color32::GREEN 
                } else { 
                    egui::Color32::RED 
                };
                Bar::new(i as f64, candle.volume).width(0.8).fill(color)
            })
            .collect();
        
        plot_ui.bar_chart(BarChart::new(volume_bars));
    }

    fn draw_statistics(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(last_candle) = self.candles.last() {
                ui.label(format!("OHLC: {:.6} | {:.6} | {:.6} | {:.6}", 
                    last_candle.open, last_candle.high, last_candle.low, last_candle.close));
                ui.separator();
                ui.label(format!("Volume: {:.2}", last_candle.volume));
                ui.separator();
                ui.label(format!("Trades: {}", last_candle.trades));
            }
            
            if let Some(rsi) = self.indicators.rsi {
                ui.separator();
                ui.label(format!("RSI: {:.2}", rsi));
            }
            
            if let Some(macd) = self.indicators.macd {
                ui.separator();
                ui.label(format!("MACD: {:.6}", macd));
            }
        });
    }
}

// Integration function to connect everything
pub async fn run_realtime_chart(
    mut tick_stream: TickStream,
    timeframe_seconds: u64,
) -> Result<()> {
    let (mut aggregator, mut candle_receiver) = OhlcAggregator::new(timeframe_seconds, 200);
    
    // Spawn tick processing task
    tokio::spawn(async move {
        while let Some(tick) = tick_stream.next().await {
            if let Err(e) = aggregator.process_tick(tick) {
                eprintln!("Error processing tick: {}", e);
            }
        }
    });

    // Setup GUI
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };

    let mut gui = CandlestickGui::new();
    
    // Spawn candle update task
    let gui_handle = std::sync::Arc::new(std::sync::Mutex::new(gui));
    let gui_clone = gui_handle.clone();
    
    tokio::spawn(async move {
        while let Ok(update) = candle_receiver.recv().await {
            if let Ok(mut gui) = gui_clone.lock() {
                gui.update_data(update);
            }
        }
    });

    // Run GUI
    eframe::run_native(
        "Real-time DEX Candlestick Chart",
        options,
        Box::new(|_cc| Box::new(CandlestickGui::new())),
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[tokio::test]
    async fn test_ohlc_aggregator() {
        let (mut aggregator, mut receiver) = OhlcAggregator::new(1, 100); // 1-second candles
        
        // Create mock ticks
        let pool_id = Pubkey::new_unique();
        let base_time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        
        let ticks = vec![
            Tick {
                price: 100.0,
                size: 10.0,
                side: Side::Buy,
                timestamp: base_time,
                pool_id,
                protocol: Protocol::Meteora,
            },
            Tick {
                price: 105.0,
                size: 5.0,
                side: Side::Buy,
                timestamp: base_time + 500,
                pool_id,
                protocol: Protocol::Meteora,
            },
            Tick {
                price: 98.0,
                size: 8.0,
                side: Side::Sell,
                timestamp: base_time + 1500, // New candle
                pool_id,
                protocol: Protocol::Meteora,
            },
        ];

        for tick in ticks {
            aggregator.process_tick(tick).unwrap();
        }

        // Should receive 2 updates (one for each tick in first candle, one for new candle)
        let update1 = receiver.recv().await.unwrap();
        assert!(!update1.is_new_candle);
        assert_eq!(update1.candle.open, 100.0);
        assert_eq!(update1.candle.close, 105.0);

        let update2 = receiver.recv().await.unwrap();
        assert!(update2.is_new_candle);
        assert_eq!(update2.candle.open, 98.0);
    }
}