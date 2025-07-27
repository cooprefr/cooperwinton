pub mod app;
pub mod dex_tick_stream;
pub mod ohlc_aggregator;
pub mod pools;
pub mod constants;
pub mod dex;

// Re-export main types for easier access
pub use app::dashboard::DexDashboard;
pub use dex_tick_stream::{Tick, Side, Protocol, TickStream, DexTickManager};
pub use ohlc_aggregator::{Candle, CandleUpdate, OhlcAggregator};