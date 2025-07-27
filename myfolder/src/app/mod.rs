pub mod app;
pub mod dashboard;

// Re-export for easier access
pub use app::App;
pub use dashboard::{DexDashboard, run_dex_dashboard};