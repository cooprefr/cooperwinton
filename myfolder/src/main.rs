#![allow(unused_imports)]
#![allow(dead_code)] 
#![allow(unused_variables)]

use cetipoo::app::dashboard::run_hft_dashboard;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();
    
    println!("🚀 Starting High Frequency Trading Dashboard");
    println!("🔌 Connecting to Helius WebSocket...");
    
    // Run the HFT dashboard with real Helius integration
    run_hft_dashboard().await?;
    
    Ok(())
}