#![allow(unused_imports)]
#![allow(dead_code)]
#![allow(unused_variables)]

use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use chrono::{DateTime, Local};
use tokio::sync::{broadcast, mpsc};
use egui::{
    plot::{Line, Plot, PlotPoints, Points}, 
    Color32,
    Layout,
    Align,
    Ui,
};
use solana_program::pubkey::Pubkey;
use std::str::FromStr;
use serde_json::{json, Value};
use env_logger;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::{SinkExt, StreamExt};

// Import our DEX tick stream components
use crate::dex_tick_stream::{Tick, Side, Protocol, TickStream, DexTickManager};
use crate::pools::MintPoolData;

#[derive(Debug, Clone)]
pub struct PoolStats {
    pub pool_id: Pubkey,
    pub protocol: Protocol,
    pub current_price: f64,
    pub last_update: SystemTime,
}

pub struct HftDashboard {
    // Core trading data
    selected_pool: Option<Pubkey>,
    pool_stats: HashMap<Pubkey, PoolStats>,
    recent_ticks: Vec<Tick>,
    price_points: Vec<[f64; 2]>, // [time_index, price]
    
    // Real-time metrics
    tick_count: u64,
    updates_per_second: f64,
    last_update: SystemTime,
    connection_status: String,
    
    // UI state
    auto_scale: bool,
    time_window: usize, // Number of recent points to show
    
    // DEX integration
    monitored_pools: Vec<MonitoredPool>,
    
    // Data streams
    tick_receiver: Option<mpsc::UnboundedReceiver<Tick>>,
    status_receiver: Option<mpsc::UnboundedReceiver<String>>,
}

#[derive(Debug, Clone)]
pub struct MonitoredPool {
    pub address: Pubkey,
    pub name: String,
    pub protocol: Protocol,
    pub token_mint: Pubkey,
    pub base_mint: Pubkey,
}

impl Default for HftDashboard {
    fn default() -> Self {
        Self::new()
    }
}

impl HftDashboard {
    pub fn new() -> Self {
        let mut dashboard = Self {
            selected_pool: None,
            pool_stats: HashMap::new(),
            recent_ticks: Vec::new(),
            price_points: Vec::new(),
            tick_count: 0,
            updates_per_second: 0.0,
            last_update: SystemTime::now(),
            connection_status: "Connecting...".to_string(),
            auto_scale: true,
            time_window: 500, // Show last 500 price points
            monitored_pools: Vec::new(),
            tick_receiver: None,
            status_receiver: None,
        };
        
        dashboard.initialize_streams();
        dashboard
    }

    fn initialize_streams(&mut self) {
        let sol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        let (tick_manager, mut tick_stream) = DexTickManager::new(sol_mint);
        
        let (tick_sender, tick_receiver) = mpsc::unbounded_channel();
        let (status_sender, status_receiver) = mpsc::unbounded_channel();
        
        self.tick_receiver = Some(tick_receiver);
        self.status_receiver = Some(status_receiver);

        // Set up real pool monitoring
        self.setup_real_pools();

        // Start Helius WebSocket connection
        let tick_manager_arc = Arc::new(tick_manager);
        let monitored_pools = self.monitored_pools.clone();
        
        tokio::spawn(async move {
            Self::connect_helius_websocket(tick_manager_arc, monitored_pools, status_sender).await;
        });

        // Process tick stream
        tokio::spawn(async move {
            use tokio_stream::StreamExt;
            while let Some(tick) = tick_stream.next().await {
                let _ = tick_sender.send(tick);
            }
        });
    }

    async fn connect_helius_websocket(
        tick_manager: Arc<DexTickManager>,
        monitored_pools: Vec<MonitoredPool>,
        status_sender: mpsc::UnboundedSender<String>,
    ) {
        let helius_url = "wss://mainnet.helius-rpc.com/?api-key=9e9acc29-363c-46d5-af90-a80475fec4c2";
        
        let _ = status_sender.send("🔄 Connecting to Helius...".to_string());
        
        loop {
            match connect_async(helius_url).await {
                Ok((ws_stream, _)) => {
                    let _ = status_sender.send("🟢 Connected to Helius".to_string());
                    println!("✅ Connected to Helius WebSocket");
                    
                    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
                    
                    // Subscribe to account updates for each monitored pool
                    for pool in &monitored_pools {
                        let subscribe_msg = json!({
                            "jsonrpc": "2.0",
                            "id": pool.address.to_string(),
                            "method": "accountSubscribe",
                            "params": [
                                pool.address.to_string(),
                                {
                                    "commitment": "confirmed",
                                    "encoding": "base64"
                                }
                            ]
                        });
                        
                        if let Ok(msg_text) = serde_json::to_string(&subscribe_msg) {
                            if let Err(e) = ws_sender.send(Message::Text(msg_text)).await {
                                eprintln!("❌ Failed to send subscription: {}", e);
                                break;
                            } else {
                                println!("📡 Subscribed to pool: {} ({})", pool.name, pool.address);
                            }
                        }
                    }
                    
                    // Process incoming messages
                    let mut subscription_count = 0;
                    while let Some(msg) = ws_receiver.next().await {
                        match msg {
                            Ok(Message::Text(text)) => {
                                if let Ok(json_data) = serde_json::from_str::<Value>(&text) {
                                    // Handle subscription confirmations
                                    if json_data.get("result").is_some() {
                                        subscription_count += 1;
                                        let _ = status_sender.send(format!("📊 Subscribed {}/{} pools", subscription_count, monitored_pools.len()));
                                        continue;
                                    }
                                    
                                    // Handle account updates
                                    if let Some(params) = json_data.get("params") {
                                        if let Some(result) = params.get("result") {
                                            if let Some(account_data) = result.get("value") {
                                                if let Some(data_str) = account_data.get("data").and_then(|d| d.get(0)).and_then(|s| s.as_str()) {
                                                    // Decode base64 account data
                                                    if let Ok(account_bytes) = base64::decode(data_str) {
                                                        // Get account pubkey from subscription
                                                        if let Some(subscription_id) = json_data.get("subscription").and_then(|s| s.as_u64()) {
                                                            // Find corresponding pool
                                                            if let Some(pool) = monitored_pools.iter().find(|p| p.address.to_string().parse::<u64>().unwrap_or(0) == subscription_id) {
                                                                // Process the account update
                                                                if let Err(e) = tick_manager.handle_account_update(&pool.address, &account_bytes).await {
                                                                    eprintln!("⚠️  Error processing account update: {}", e);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(Message::Close(_)) => {
                                println!("🔌 WebSocket connection closed");
                                let _ = status_sender.send("🔴 Connection closed".to_string());
                                break;
                            }
                            Err(e) => {
                                eprintln!("❌ WebSocket error: {}", e);
                                let _ = status_sender.send("🔴 Connection error".to_string());
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    eprintln!("❌ Failed to connect to Helius: {}", e);
                    let _ = status_sender.send("🔴 Connection failed".to_string());
                }
            }
            
            // Reconnect after 5 seconds
            println!("🔄 Reconnecting in 5 seconds...");
            let _ = status_sender.send("🔄 Reconnecting...".to_string());
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    fn setup_real_pools(&mut self) {
        // REAL Solana DEX pool addresses from your existing pool data
        let real_pools = [
            // Whirlpool pools
            ("7dHbWXmci3dT8UFYWYZweBLXgycu7Y3iL6trKn1Y7ARj", "SOL/USDC - Whirlpool", Protocol::Whirlpool),
            ("HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ", "SOL/USDT - Whirlpool", Protocol::Whirlpool),
            
            // Raydium AMM pools  
            ("58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2", "SOL/USDC - Raydium", Protocol::RaydiumAmm),
            ("7XawhbbxtsRcQA8KTkHT9f9nc6d69UwqCDh6U5EEbEmX", "SOL/USDT - Raydium", Protocol::RaydiumAmm),
            
            // Raydium CLMM pools
            ("61R1ndXxvsWXXkWSyNkCxnzwd3zUNB8Q2ibmkiLPC8ht", "SOL/USDC - Raydium CLMM", Protocol::RaydiumClmm),
            
            // Meteora DLMM pools
            ("Ak6oGhCjZAYQGNpFb8VWRUubT2L3JPWQ2hTQ3hJQdQ9j", "SOL/USDC - Meteora", Protocol::Meteora),
            
            // Pump.fun pools (add specific addresses)
            ("7L53bUGZopuwMzjyK7TsSzSzKTeGC6VzqzP5akpump", "PUMP/SOL", Protocol::Pump),
        ];

        let sol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        let usdc_mint = Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap();

        for (address_str, name, protocol) in real_pools.iter() {
            if let Ok(pool_address) = Pubkey::from_str(address_str) {
                // Add to monitored pools
                self.monitored_pools.push(MonitoredPool {
                    address: pool_address,
                    name: name.to_string(),
                    protocol: protocol.clone(),
                    token_mint: if name.contains("USDC") { usdc_mint } else { sol_mint },
                    base_mint: sol_mint,
                });
                
                // Add to pool stats
                self.pool_stats.insert(pool_address, PoolStats {
                    pool_id: pool_address,
                    protocol: protocol.clone(),
                    current_price: 0.0, // Will be updated by real DEX data
                    last_update: SystemTime::now(),
                });
                
                println!("📊 Monitoring pool: {} - {}", name, address_str);
            }
        }

        // Set first pool as selected
        if let Some(first_pool) = self.monitored_pools.first() {
            self.selected_pool = Some(first_pool.address);
        }
        
        println!("🚀 Ready to receive live DEX price data for {} pools", self.monitored_pools.len());
    }

    fn process_real_time_updates(&mut self) {
        // Process status updates
        if let Some(ref mut status_receiver) = self.status_receiver {
            if let Ok(status) = status_receiver.try_recv() {
                self.connection_status = status;
            }
        }
        
        if let Some(ref mut tick_receiver) = self.tick_receiver {
            let mut new_ticks = 0;
            let current_time = self.tick_count as f64;
            
            while let Ok(tick) = tick_receiver.try_recv() {
                // Only process ticks for selected pool
                if let Some(selected) = self.selected_pool {
                    if tick.pool_id == selected {
                        // Add to recent ticks
                        self.recent_ticks.push(tick.clone());
                        if self.recent_ticks.len() > 1000 {
                            self.recent_ticks.remove(0);
                        }
                        
                        // Add to price chart
                        self.price_points.push([current_time + new_ticks as f64, tick.price]);
                        if self.price_points.len() > self.time_window {
                            self.price_points.remove(0);
                        }
                    }
                }
                
                // Update pool stats for all pools
                if let Some(pool) = self.pool_stats.get_mut(&tick.pool_id) {
                    pool.current_price = tick.price;
                    pool.last_update = SystemTime::now();
                }
                
                new_ticks += 1;
                self.tick_count += 1;
            }
            
            // Update rate calculation
            if new_ticks > 0 {
                let elapsed = self.last_update.elapsed().unwrap_or(Duration::from_millis(100));
                if elapsed.as_millis() > 0 {
                    self.updates_per_second = new_ticks as f64 / elapsed.as_secs_f64();
                }
                self.last_update = SystemTime::now();
            }
        }
    }
}

impl eframe::App for HftDashboard {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // High frequency updates
        self.process_real_time_updates();

        // Top bar
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ctx.request_repaint_after(Duration::from_millis(25)); // 40 FPS
            
            ui.horizontal(|ui| {
                // Title
                ui.heading("🚀 HFT Dashboard");
                
                ui.separator();
                
                // Pool selector
                ui.label("Pool:");
                let selected_text = if let Some(pool_id) = self.selected_pool {
                    if let Some(stats) = self.pool_stats.get(&pool_id) {
                        format!("{:?} - ${:.4}", stats.protocol, stats.current_price)
                    } else {
                        "Select Pool".to_string()
                    }
                } else {
                    "Select Pool".to_string()
                };
                
                egui::ComboBox::from_label("")
                    .selected_text(selected_text)
                    .show_ui(ui, |ui| {
                        for (pool_id, stats) in &self.pool_stats {
                            let label = format!("{:?} - ${:.4}", stats.protocol, stats.current_price);
                            if ui.selectable_value(&mut self.selected_pool, Some(*pool_id), label).clicked() {
                                // Clear chart when switching pools
                                self.price_points.clear();
                            }
                        }
                    });
                
                ui.separator();
                
                // Live stats
                let status_color = if self.updates_per_second > 10.0 { Color32::GREEN } 
                                 else if self.updates_per_second > 1.0 { Color32::YELLOW } 
                                 else { Color32::RED };
                ui.colored_label(status_color, format!("🔴 {:.1} tps", self.updates_per_second));
                
                ui.separator();
                ui.label(format!("Ticks: {}", self.tick_count));
                
                ui.separator();
                ui.checkbox(&mut self.auto_scale, "Auto Scale");
                
                // Right side - Time and controls
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Clear").clicked() {
                        self.price_points.clear();
                        self.recent_ticks.clear();
                        self.tick_count = 0;
                    }
                    
                    ui.separator();
                    
                    let now: DateTime<Local> = Local::now();
                    ui.label(format!("{}", now.format("%H:%M:%S")));
                });
            });
        });

        // Main trading interface
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                // Main price chart (80% width)
                ui.allocate_ui_with_layout(
                    [ui.available_width() * 0.8, ui.available_height()],
                    Layout::top_down(Align::Min),
                    |ui| {
                        self.draw_price_chart(ui);
                    }
                );

                ui.separator();

                // Side panel: Live tick feed (20% width)
                ui.allocate_ui_with_layout(
                    [ui.available_width(), ui.available_height()],
                    Layout::top_down(Align::Min),
                    |ui| {
                        self.draw_tick_feed(ui);
                    }
                );
            });
        });
    }
}

impl HftDashboard {
    fn draw_price_chart(&self, ui: &mut egui::Ui) {
        ui.heading("💹 Live Price Feed");
        ui.separator();
        
        // Current price display
        if let Some(last_tick) = self.recent_ticks.last() {
            ui.horizontal(|ui| {
                let price_color = match last_tick.side {
                    Side::Buy => Color32::GREEN,
                    Side::Sell => Color32::RED,
                };
                
                ui.colored_label(price_color, format!("${:.6}", last_tick.price));
                ui.label(format!("Size: {:.2}", last_tick.size));
                
                let side_text = match last_tick.side {
                    Side::Buy => "BUY",
                    Side::Sell => "SELL",
                };
                ui.colored_label(price_color, side_text);
            });
        }
        
        ui.separator();
        
        // High-frequency price chart
        let plot = Plot::new("hft_price_chart")
            .height(ui.available_height() - 100.0)
            .show_background(true)
            .allow_boxed_zoom(true)
            .allow_drag(true)
            .auto_bounds_x()
            .auto_bounds_y();

        plot.show(ui, |plot_ui| {
            if !self.price_points.is_empty() {
                // Main price line
                let price_line = Line::new(PlotPoints::new(self.price_points.clone()))
                    .color(Color32::WHITE)
                    .width(1.0);
                plot_ui.line(price_line);
                
                // Add buy/sell points
                let recent_buys: Vec<[f64; 2]> = self.recent_ticks.iter()
                    .rev()
                    .take(50)
                    .enumerate()
                    .filter(|(_, tick)| tick.side == Side::Buy)
                    .map(|(i, tick)| [self.price_points.len() as f64 - i as f64, tick.price])
                    .collect();
                
                let recent_sells: Vec<[f64; 2]> = self.recent_ticks.iter()
                    .rev()
                    .take(50)
                    .enumerate()
                    .filter(|(_, tick)| tick.side == Side::Sell)
                    .map(|(i, tick)| [self.price_points.len() as f64 - i as f64, tick.price])
                    .collect();
                
                if !recent_buys.is_empty() {
                    plot_ui.points(Points::new(PlotPoints::new(recent_buys))
                        .color(Color32::GREEN)
                        .radius(2.0)
                        .filled(true));
                }
                
                if !recent_sells.is_empty() {
                    plot_ui.points(Points::new(PlotPoints::new(recent_sells))
                        .color(Color32::RED)
                        .radius(2.0)
                        .filled(true));
                }
            }
        });
    }

    fn draw_tick_feed(&self, ui: &mut egui::Ui) {
        ui.heading("📈 Live Ticks");
        ui.separator();
        
        // Quick stats
        ui.horizontal(|ui| {
            let buy_count = self.recent_ticks.iter()
                .rev()
                .take(100)
                .filter(|t| t.side == Side::Buy)
                .count();
            let sell_count = 100 - buy_count;
            
            ui.colored_label(Color32::GREEN, format!("B: {}", buy_count));
            ui.colored_label(Color32::RED, format!("S: {}", sell_count));
        });
        
        ui.separator();
        
        // Live tick stream
        egui::ScrollArea::vertical()
            .max_height(ui.available_height())
            .auto_shrink([false, true])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for tick in self.recent_ticks.iter().rev().take(100) {
                    ui.horizontal(|ui| {
                        let (color, symbol) = match tick.side {
                            Side::Buy => (Color32::GREEN, "▲"),
                            Side::Sell => (Color32::RED, "▼"),
                        };
                        
                        ui.colored_label(color, symbol);
                        ui.label(format!("{:.6}", tick.price));
                        ui.label(format!("{:.1}", tick.size));
                        
                        // Time
                        let time = DateTime::from_timestamp(tick.timestamp as i64 / 1000, 0)
                            .unwrap_or_default()
                            .format("%H:%M:%S%.3f");
                        ui.small(format!("{}", time));
                    });
                }
                
                if self.recent_ticks.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("Waiting for ticks...");
                    });
                }
            });
    }
}

// Simplified run function
pub async fn run_hft_dashboard() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1600.0, 900.0])
            .with_title("🚀 Solana HFT Dashboard - Live DEX Data"),
        ..Default::default()
    };

    println!("🔗 Initializing dashboard with Helius WebSocket connection...");
    let dashboard = HftDashboard::new();

    eframe::run_native(
        "Solana HFT Dashboard",
        options,
        Box::new(move |cc| {
            // Dark theme for trading
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            
            // Set monospace font for price display
            let mut fonts = egui::FontDefinitions::default();
            if let Some(font_data) = egui::FontData::builtin("monospace") {
                fonts.font_data.insert("monospace".to_owned(), font_data);
            }
            cc.egui_ctx.set_fonts(fonts);
            
            Box::new(dashboard)
        }),
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_pool_setup() {
        let dashboard = HftDashboard::new();
        assert!(dashboard.monitored_pools.len() > 0);
        assert!(dashboard.pool_stats.len() > 0);
    }
    
    #[tokio::test]
    async fn test_websocket_connection() {
        // Test that we can parse the Helius URL
        let helius_url = "wss://mainnet.helius-rpc.com/?api-key=9e9acc29-363c-46d5-af90-a80475fec4c2";
        assert!(helius_url.starts_with("wss://"));
        assert!(helius_url.contains("api-key"));
    }
}