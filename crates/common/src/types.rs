// Minimal stubs for workspace compilation - full implementation in Session 2

use serde::{Deserialize, Serialize};

/// Protocol type enum matching on-chain constants
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ProtocolType {
    UniswapV2 = 1,
    UniswapV3 = 2,
    SushiSwap = 3,
    Curve = 4,
    BalancerV2 = 5,
    BancorV3 = 6,
}
