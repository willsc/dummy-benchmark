//! Wire formats carried in ring-buffer slots.
//!
//! Layouts are `#[repr(C)]` and fixed-size so they can be `memcpy`'d in and out
//! of a shared-memory slot without any serialisation cost.

use std::fmt;

pub const SYMBOL_LEN: usize = 8;

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Side {
    Buy = 1,
    Sell = 2,
}

impl Side {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            1 => Some(Side::Buy),
            2 => Some(Side::Sell),
            _ => None,
        }
    }
}

/// Top-of-book quote tick.
///
/// Fixed prices are encoded as i64 with a multiplier of 1e6
/// (so 100.50 USD becomes 100_500_000).
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct MarketTick {
    pub seq_num: u64,
    pub exch_ts_ns: u64,
    pub recv_ts_ns: u64,
    pub symbol: [u8; SYMBOL_LEN],
    pub bid_px: i64,
    pub ask_px: i64,
    pub bid_sz: u32,
    pub ask_sz: u32,
    pub _pad: u32,
}

impl MarketTick {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    pub fn symbol_str(&self) -> &str {
        let end = self.symbol.iter().position(|&b| b == 0).unwrap_or(SYMBOL_LEN);
        std::str::from_utf8(&self.symbol[..end]).unwrap_or("?")
    }
}

impl fmt::Display for MarketTick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} bid={}@{} ask={}@{} seq={}",
            self.symbol_str(),
            self.bid_sz,
            self.bid_px as f64 / 1e6,
            self.ask_sz,
            self.ask_px as f64 / 1e6,
            self.seq_num,
        )
    }
}

/// Order emitted by the trading engine back to the feedhandler.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct OrderMsg {
    pub order_id: u64,
    pub submit_ts_ns: u64,
    pub symbol: [u8; SYMBOL_LEN],
    pub px: i64,
    pub qty: u32,
    pub side: u8,
    pub _pad: [u8; 3],
}

impl OrderMsg {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    pub fn symbol_str(&self) -> &str {
        let end = self.symbol.iter().position(|&b| b == 0).unwrap_or(SYMBOL_LEN);
        std::str::from_utf8(&self.symbol[..end]).unwrap_or("?")
    }
}

impl fmt::Display for OrderMsg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let side = match Side::from_u8(self.side) {
            Some(Side::Buy) => "BUY",
            Some(Side::Sell) => "SELL",
            None => "?",
        };
        write!(
            f,
            "{} {} {}@{} id={}",
            side,
            self.symbol_str(),
            self.qty,
            self.px as f64 / 1e6,
            self.order_id,
        )
    }
}

// Sanity: both fit comfortably in a ring-buffer payload.
const _: () = assert!(MarketTick::SIZE <= crate::ring::SLOT_PAYLOAD);
const _: () = assert!(OrderMsg::SIZE <= crate::ring::SLOT_PAYLOAD);
