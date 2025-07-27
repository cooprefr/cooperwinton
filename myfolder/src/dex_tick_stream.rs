use anyhow::Result;
use solana_program::pubkey::Pubkey;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};

// Import the existing DEX modules - FIXED IMPORTS
use crate::dex::{
    meteora::{dlmm_info::DlmmInfo, constants::dlmm_program_id},
    pump::{PumpAmmInfo, pump_program_id},
    raydium::{RaydiumAmmInfo, RaydiumCpAmmInfo, PoolState, raydium_program_id, raydium_cp_program_id, raydium_clmm_program_id},
    whirlpool::{state::Whirlpool, constants::whirlpool_program_id},
};
use crate::pools::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct Tick {
    pub price: f64,        // Price in quote token per base token
    pub size: f64,         // Size in base token
    pub side: Side,        // Buy or Sell
    pub timestamp: u64,    // Unix timestamp in milliseconds
    pub pool_id: Pubkey,   // Pool identifier
    pub protocol: Protocol, // Which DEX protocol
}

#[derive(Debug, Clone, PartialEq)]
pub enum Protocol {
    Meteora,
    Pump,
    RaydiumAmm,
    RaydiumCp,
    RaydiumClmm,
    Whirlpool,
}

#[derive(Debug)]
pub enum PoolData {
    Meteora(DlmmInfo),
    Pump(PumpAmmInfo),
    RaydiumAmm(RaydiumAmmInfo),
    RaydiumCp(RaydiumCpAmmInfo),
    RaydiumClmm(PoolState),
    Whirlpool(Whirlpool),
}

#[derive(Debug)]
pub struct PoolUpdate {
    pub pool_id: Pubkey,
    pub data: PoolData,
    pub timestamp: u64,
}

// Trait for converting pool data to ticks
pub trait TickConverter {
    fn to_tick(&self, pool_id: Pubkey, timestamp: u64, sol_mint: &Pubkey) -> Result<Option<Tick>>;
}

impl TickConverter for DlmmInfo {
    fn to_tick(&self, pool_id: Pubkey, timestamp: u64, sol_mint: &Pubkey) -> Result<Option<Tick>> {
        // Calculate price from active_id (bin price)
        // Meteora DLMM uses bin pricing: price = (1 + bin_step/10000)^active_id
        let bin_step = self.lb_pair.bin_step as f64 / 10000.0;
        let price = (1.0 + bin_step).powf(self.active_id as f64);
        
        // Determine if SOL is token X or Y to set correct price direction
        let (adjusted_price, side) = if sol_mint == &self.token_x_mint {
            // SOL is X, token is Y - price is token/SOL
            (1.0 / price, Side::Sell)
        } else {
            // SOL is Y, token is X - price is SOL/token  
            (price, Side::Buy)
        };

        Ok(Some(Tick {
            price: adjusted_price,
            size: 0.0, // Would need liquidity data to calculate
            side,
            timestamp,
            pool_id,
            protocol: Protocol::Meteora,
        }))
    }
}

impl TickConverter for RaydiumAmmInfo {
    fn to_tick(&self, pool_id: Pubkey, timestamp: u64, sol_mint: &Pubkey) -> Result<Option<Tick>> {
        // For Raydium AMM, we'd need to fetch vault balances to calculate price
        // This is a simplified implementation
        Ok(Some(Tick {
            price: 0.0, // Would calculate from coin_vault/pc_vault balances
            size: 0.0,
            side: Side::Buy,
            timestamp,
            pool_id,
            protocol: Protocol::RaydiumAmm,
        }))
    }
}

impl TickConverter for RaydiumCpAmmInfo {
    fn to_tick(&self, pool_id: Pubkey, timestamp: u64, sol_mint: &Pubkey) -> Result<Option<Tick>> {
        // Similar to AMM, need vault balances for actual price calculation
        Ok(Some(Tick {
            price: 0.0,
            size: 0.0,
            side: Side::Buy,
            timestamp,
            pool_id,
            protocol: Protocol::RaydiumCp,
        }))
    }
}

impl TickConverter for PoolState {
    fn to_tick(&self, pool_id: Pubkey, timestamp: u64, sol_mint: &Pubkey) -> Result<Option<Tick>> {
        // Convert sqrt_price_x64 to actual price
        let sqrt_price = self.sqrt_price_x64 as f64 / (2_f64.powf(64.0));
        let price = sqrt_price * sqrt_price;
        
        let (adjusted_price, side) = if sol_mint == &self.token_mint_0 {
            (1.0 / price, Side::Sell)
        } else {
            (price, Side::Buy)
        };

        Ok(Some(Tick {
            price: adjusted_price,
            size: 0.0, // Would need to calculate from liquidity
            timestamp,
            pool_id,
            protocol: Protocol::RaydiumClmm,
            side,
        }))
    }
}

impl TickConverter for Whirlpool {
    fn to_tick(&self, pool_id: Pubkey, timestamp: u64, sol_mint: &Pubkey) -> Result<Option<Tick>> {
        // Convert sqrt_price to actual price
        let sqrt_price = self.sqrt_price as f64 / (2_f64.powf(64.0));
        let price = sqrt_price * sqrt_price;
        
        let (adjusted_price, side) = if sol_mint == &self.token_mint_a {
            (1.0 / price, Side::Sell)
        } else {
            (price, Side::Buy)
        };

        Ok(Some(Tick {
            price: adjusted_price,
            size: 0.0,
            side,
            timestamp,
            pool_id,
            protocol: Protocol::Whirlpool,
        }))
    }
}

impl TickConverter for PumpAmmInfo {
    fn to_tick(&self, pool_id: Pubkey, timestamp: u64, sol_mint: &Pubkey) -> Result<Option<Tick>> {
        // Pump.fun uses a bonding curve, would need additional data to calculate price
        Ok(Some(Tick {
            price: 0.0,
            size: 0.0,
            side: Side::Buy,
            timestamp,
            pool_id,
            protocol: Protocol::Pump,
        }))
    }
}

// Unified tick stream
pub struct TickStream {
    receiver: mpsc::UnboundedReceiver<Tick>,
}

impl TickStream {
    pub fn new() -> (TickStreamSender, Self) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (TickStreamSender { sender }, Self { receiver })
    }
}

impl Stream for TickStream {
    type Item = Tick;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

pub struct TickStreamSender {
    sender: mpsc::UnboundedSender<Tick>,
}

impl TickStreamSender {
    pub fn send_tick(&self, tick: Tick) -> Result<()> {
        self.sender.send(tick).map_err(|_| anyhow::anyhow!("Failed to send tick"))?;
        Ok(())
    }

    pub fn process_pool_update(&self, update: PoolUpdate, sol_mint: &Pubkey) -> Result<()> {
        let tick = match &update.data {
            PoolData::Meteora(info) => info.to_tick(update.pool_id, update.timestamp, sol_mint)?,
            PoolData::Pump(info) => info.to_tick(update.pool_id, update.timestamp, sol_mint)?,
            PoolData::RaydiumAmm(info) => info.to_tick(update.pool_id, update.timestamp, sol_mint)?,
            PoolData::RaydiumCp(info) => info.to_tick(update.pool_id, update.timestamp, sol_mint)?,
            PoolData::RaydiumClmm(info) => info.to_tick(update.pool_id, update.timestamp, sol_mint)?,
            PoolData::Whirlpool(info) => info.to_tick(update.pool_id, update.timestamp, sol_mint)?,
        };

        if let Some(tick) = tick {
            self.send_tick(tick)?;
        }

        Ok(())
    }
}

// Manager for handling multiple pools
pub struct DexTickManager {
    tick_sender: TickStreamSender,
    sol_mint: Pubkey,
}

impl DexTickManager {
    pub fn new(sol_mint: Pubkey) -> (Self, TickStream) {
        let (sender, stream) = TickStream::new();
        (Self {
            tick_sender: sender,
            sol_mint,
        }, stream)
    }

    pub async fn handle_account_update(&self, account_key: &Pubkey, data: &[u8]) -> Result<()> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis() as u64;

        // Try to parse as different pool types based on data structure
        if let Ok(dlmm_info) = DlmmInfo::load_checked(data) {
            let update = PoolUpdate {
                pool_id: *account_key,
                data: PoolData::Meteora(dlmm_info),
                timestamp,
            };
            self.tick_sender.process_pool_update(update, &self.sol_mint)?;
        } else if let Ok(pump_info) = PumpAmmInfo::load_checked(data) {
            let update = PoolUpdate {
                pool_id: *account_key,
                data: PoolData::Pump(pump_info),
                timestamp,
            };
            self.tick_sender.process_pool_update(update, &self.sol_mint)?;
        } else if let Ok(raydium_info) = RaydiumAmmInfo::load_checked(data) {
            let update = PoolUpdate {
                pool_id: *account_key,
                data: PoolData::RaydiumAmm(raydium_info),
                timestamp,
            };
            self.tick_sender.process_pool_update(update, &self.sol_mint)?;
        } else if let Ok(raydium_cp_info) = RaydiumCpAmmInfo::load_checked(data) {
            let update = PoolUpdate {
                pool_id: *account_key,
                data: PoolData::RaydiumCp(raydium_cp_info),
                timestamp,
            };
            self.tick_sender.process_pool_update(update, &self.sol_mint)?;
        } else if let Ok(clmm_info) = PoolState::load_checked(data) {
            let update = PoolUpdate {
                pool_id: *account_key,
                data: PoolData::RaydiumClmm(clmm_info),
                timestamp,
            };
            self.tick_sender.process_pool_update(update, &self.sol_mint)?;
        } else if let Ok(whirlpool_info) = Whirlpool::try_deserialize(data) {
            let update = PoolUpdate {
                pool_id: *account_key,
                data: PoolData::Whirlpool(whirlpool_info),
                timestamp,
            };
            self.tick_sender.process_pool_update(update, &self.sol_mint)?;
        }

        Ok(())
    }

    // Batch processing for multiple pool updates
    pub async fn handle_batch_updates(&self, updates: Vec<(Pubkey, Vec<u8>)>) -> Result<()> {
        for (account_key, data) in updates {
            self.handle_account_update(&account_key, &data).await?;
        }
        Ok(())
    }
}

// Example usage with MintPoolData
impl MintPoolData {
    pub async fn start_tick_stream(&self, sol_mint: Pubkey) -> Result<TickStream> {
        let (manager, stream) = DexTickManager::new(sol_mint);
        
        // In a real implementation, you would:
        // 1. Subscribe to account updates for all pools in self
        // 2. Forward those updates to manager.handle_account_update()
        // 3. Return the stream for consumption
        
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[tokio::test]
    async fn test_tick_stream() {
        let sol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        let (manager, mut stream) = DexTickManager::new(sol_mint);
        
        // Simulate some pool data updates
        tokio::spawn(async move {
            // In real usage, this would come from Solana account subscriptions
            let mock_data = vec![0u8; 1000]; // Mock account data
            let pool_id = Pubkey::new_unique();
            
            if let Err(e) = manager.handle_account_update(&pool_id, &mock_data).await {
                eprintln!("Error handling update: {}", e);
            }
        });

        // Consume ticks from the stream
        if let Some(tick) = stream.next().await {
            println!("Received tick: {:?}", tick);
        }
    }
}