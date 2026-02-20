use aether_common::types::ProtocolType;
use alloy::primitives::{Address, B256, U256};
use alloy::sol;
use alloy::sol_types::SolEvent;
use tracing::trace;

// Compile-time ABI definitions via alloy sol! macro
sol! {
    // Uniswap V2 / SushiSwap Sync event
    event Sync(uint112 reserve0, uint112 reserve1);

    // Uniswap V2 / SushiSwap Swap event
    event Swap(
        address indexed sender,
        uint256 amount0In,
        uint256 amount1In,
        uint256 amount0Out,
        uint256 amount1Out,
        address indexed to
    );

    // Uniswap V3 Swap event
    event SwapV3(
        address indexed sender,
        address indexed recipient,
        int256 amount0,
        int256 amount1,
        uint160 sqrtPriceX96,
        uint128 liquidity,
        int24 tick
    );

    // Curve TokenExchange event
    event TokenExchange(
        address indexed buyer,
        int128 sold_id,
        uint256 tokens_sold,
        int128 bought_id,
        uint256 tokens_bought
    );

    // Uniswap V2 PairCreated (factory event for discovery)
    event PairCreated(
        address indexed token0,
        address indexed token1,
        address pair,
        uint256 allPairsLength
    );
}

/// Decoded pool update event
#[derive(Debug, Clone)]
pub enum PoolEvent {
    /// Reserve update (UniV2/Sushi Sync)
    ReserveUpdate {
        pool: Address,
        protocol: ProtocolType,
        reserve0: U256,
        reserve1: U256,
    },
    /// V3 state update
    V3Update {
        pool: Address,
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
    },
    /// New pool created
    PoolCreated {
        token0: Address,
        token1: Address,
        pool: Address,
    },
}

/// Event topic signatures for filtering
pub struct EventSignatures;

impl EventSignatures {
    pub fn sync_topic() -> B256 {
        Sync::SIGNATURE_HASH
    }

    pub fn swap_v2_topic() -> B256 {
        Swap::SIGNATURE_HASH
    }

    pub fn swap_v3_topic() -> B256 {
        SwapV3::SIGNATURE_HASH
    }

    pub fn token_exchange_topic() -> B256 {
        TokenExchange::SIGNATURE_HASH
    }

    pub fn pair_created_topic() -> B256 {
        PairCreated::SIGNATURE_HASH
    }
}

/// Decode a raw log into a PoolEvent
/// Returns None if the log doesn't match any known event
pub fn decode_log(
    topics: &[B256],
    data: &[u8],
    source_address: Address,
    protocol_hint: Option<ProtocolType>,
) -> Option<PoolEvent> {
    if topics.is_empty() {
        return None;
    }

    let topic0 = topics[0];

    if topic0 == EventSignatures::sync_topic() {
        decode_sync(data, source_address, protocol_hint)
    } else if topic0 == EventSignatures::swap_v3_topic() {
        decode_swap_v3(topics, data, source_address)
    } else if topic0 == EventSignatures::token_exchange_topic() {
        decode_token_exchange(topics, data, source_address)
    } else if topic0 == EventSignatures::pair_created_topic() {
        decode_pair_created(topics, data)
    } else {
        None
    }
}

fn decode_sync(
    data: &[u8],
    pool: Address,
    protocol_hint: Option<ProtocolType>,
) -> Option<PoolEvent> {
    if data.len() < 64 {
        return None;
    }
    let reserve0 = U256::from_be_slice(&data[0..32]);
    let reserve1 = U256::from_be_slice(&data[32..64]);
    let protocol = protocol_hint.unwrap_or(ProtocolType::UniswapV2);

    trace!(pool = %pool, r0 = %reserve0, r1 = %reserve1, "Sync event decoded");

    Some(PoolEvent::ReserveUpdate {
        pool,
        protocol,
        reserve0,
        reserve1,
    })
}

fn decode_swap_v3(topics: &[B256], data: &[u8], pool: Address) -> Option<PoolEvent> {
    if data.len() < 160 {
        return None;
    }
    // amount0: int256 (bytes 0-32)
    // amount1: int256 (bytes 32-64)
    // sqrtPriceX96: uint160 (bytes 64-96)
    // liquidity: uint128 (bytes 96-128)
    // tick: int24 (bytes 128-160)
    let sqrt_price_x96 = U256::from_be_slice(&data[64..96]);
    let liquidity_u256 = U256::from_be_slice(&data[96..128]);
    let liquidity = liquidity_u256.to::<u128>();

    // tick is int24, stored in last 3 bytes of the 32-byte word
    let tick_bytes = &data[128..160];
    let tick_i256 =
        i32::from_be_bytes([tick_bytes[28], tick_bytes[29], tick_bytes[30], tick_bytes[31]]);
    // Sign extend from 24 bits
    let tick = if tick_i256 & 0x800000 != 0 {
        tick_i256 | !0xFFFFFF_i32
    } else {
        tick_i256 & 0xFFFFFF
    };

    let _ = topics; // topics contain indexed sender and recipient

    trace!(
        pool = %pool,
        sqrt_price = %sqrt_price_x96,
        liq = %liquidity,
        tick = %tick,
        "V3 Swap decoded"
    );

    Some(PoolEvent::V3Update {
        pool,
        sqrt_price_x96,
        liquidity,
        tick,
    })
}

fn decode_token_exchange(
    topics: &[B256],
    _data: &[u8],
    pool: Address,
) -> Option<PoolEvent> {
    // Curve events update reserves; we'd need to query on-chain for new balances
    // For now, emit a generic reserve update that triggers a state refresh
    let _ = topics;
    Some(PoolEvent::ReserveUpdate {
        pool,
        protocol: ProtocolType::Curve,
        reserve0: U256::ZERO, // Will be refreshed from on-chain
        reserve1: U256::ZERO,
    })
}

fn decode_pair_created(topics: &[B256], data: &[u8]) -> Option<PoolEvent> {
    if topics.len() < 3 || data.len() < 64 {
        return None;
    }
    let token0 = Address::from_slice(&topics[1].as_slice()[12..]);
    let token1 = Address::from_slice(&topics[2].as_slice()[12..]);
    let pool = Address::from_slice(&data[12..32]);

    Some(PoolEvent::PoolCreated {
        token0,
        token1,
        pool,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    /// Build a 32-byte big-endian representation of a U256
    fn u256_to_be_bytes(val: U256) -> [u8; 32] {
        val.to_be_bytes::<32>()
    }

    // ── EventSignatures tests ──

    #[test]
    fn test_event_signatures_are_nonzero_and_distinct() {
        let sigs = [
            EventSignatures::sync_topic(),
            EventSignatures::swap_v2_topic(),
            EventSignatures::swap_v3_topic(),
            EventSignatures::token_exchange_topic(),
            EventSignatures::pair_created_topic(),
        ];
        for sig in &sigs {
            assert_ne!(*sig, B256::ZERO, "Signature should not be zero");
        }
        // All must be distinct
        for i in 0..sigs.len() {
            for j in (i + 1)..sigs.len() {
                assert_ne!(sigs[i], sigs[j], "Signatures at {i} and {j} must differ");
            }
        }
    }

    // ── Sync event decode tests ──

    #[test]
    fn test_decode_sync_event() {
        let pool_addr = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");
        let reserve0 = U256::from(1_000_000_000_000_000_000u64); // 1e18
        let reserve1 = U256::from(2_000_000_000u64); // 2e9

        let mut data = Vec::new();
        data.extend_from_slice(&u256_to_be_bytes(reserve0));
        data.extend_from_slice(&u256_to_be_bytes(reserve1));

        let topics = vec![EventSignatures::sync_topic()];

        let event = decode_log(&topics, &data, pool_addr, None);
        assert!(event.is_some());

        match event.unwrap() {
            PoolEvent::ReserveUpdate {
                pool,
                protocol,
                reserve0: r0,
                reserve1: r1,
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(protocol, ProtocolType::UniswapV2);
                assert_eq!(r0, reserve0);
                assert_eq!(r1, reserve1);
            }
            other => panic!("Expected ReserveUpdate, got {:?}", other),
        }
    }

    #[test]
    fn test_decode_sync_event_with_sushi_hint() {
        let pool_addr = Address::ZERO;
        let reserve0 = U256::from(500u64);
        let reserve1 = U256::from(1000u64);

        let mut data = Vec::new();
        data.extend_from_slice(&u256_to_be_bytes(reserve0));
        data.extend_from_slice(&u256_to_be_bytes(reserve1));

        let topics = vec![EventSignatures::sync_topic()];

        let event = decode_log(&topics, &data, pool_addr, Some(ProtocolType::SushiSwap));
        match event.unwrap() {
            PoolEvent::ReserveUpdate { protocol, .. } => {
                assert_eq!(protocol, ProtocolType::SushiSwap);
            }
            other => panic!("Expected ReserveUpdate, got {:?}", other),
        }
    }

    #[test]
    fn test_decode_sync_event_too_short_data() {
        let topics = vec![EventSignatures::sync_topic()];
        // Only 32 bytes instead of 64
        let data = vec![0u8; 32];
        let event = decode_log(&topics, &data, Address::ZERO, None);
        assert!(event.is_none());
    }

    // ── V3 Swap event decode tests ──

    #[test]
    fn test_decode_v3_swap_event() {
        let pool_addr = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");

        // Build 160 bytes of data:
        // amount0 (int256): 32 bytes
        // amount1 (int256): 32 bytes
        // sqrtPriceX96 (uint160): 32 bytes
        // liquidity (uint128): 32 bytes
        // tick (int24): 32 bytes
        let mut data = Vec::new();

        // amount0 = 1e18 (positive)
        data.extend_from_slice(&u256_to_be_bytes(U256::from(
            1_000_000_000_000_000_000u64,
        )));
        // amount1 = -2000e6 (negative, as two's complement)
        let neg_amount1 = U256::MAX - U256::from(2_000_000_000u64) + U256::from(1u64);
        data.extend_from_slice(&u256_to_be_bytes(neg_amount1));
        // sqrtPriceX96
        let sqrt_price = U256::from(1_234_567_890_123_456_789u64);
        data.extend_from_slice(&u256_to_be_bytes(sqrt_price));
        // liquidity = 5_000_000
        let liquidity = U256::from(5_000_000u64);
        data.extend_from_slice(&u256_to_be_bytes(liquidity));
        // tick = 200 (positive i24)
        let tick_val = U256::from(200u64);
        data.extend_from_slice(&u256_to_be_bytes(tick_val));

        let topics = vec![
            EventSignatures::swap_v3_topic(),
            B256::ZERO, // sender (indexed)
            B256::ZERO, // recipient (indexed)
        ];

        let event = decode_log(&topics, &data, pool_addr, None);
        assert!(event.is_some());

        match event.unwrap() {
            PoolEvent::V3Update {
                pool,
                sqrt_price_x96,
                liquidity: liq,
                tick,
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(sqrt_price_x96, sqrt_price);
                assert_eq!(liq, 5_000_000u128);
                assert_eq!(tick, 200);
            }
            other => panic!("Expected V3Update, got {:?}", other),
        }
    }

    #[test]
    fn test_decode_v3_swap_negative_tick() {
        let pool_addr = Address::ZERO;

        let mut data = Vec::new();
        // amount0, amount1
        data.extend_from_slice(&[0u8; 64]);
        // sqrtPriceX96
        data.extend_from_slice(&u256_to_be_bytes(U256::from(999u64)));
        // liquidity
        data.extend_from_slice(&u256_to_be_bytes(U256::from(100u64)));
        // tick = -100 as int24 in 32 bytes (two's complement)
        // -100 in 24-bit = 0xFFFF9C, in 32-byte word last 4 bytes: 0xFFFFFF9C
        let neg_tick = U256::MAX - U256::from(99u64); // two's complement for -100
        data.extend_from_slice(&u256_to_be_bytes(neg_tick));

        let topics = vec![
            EventSignatures::swap_v3_topic(),
            B256::ZERO,
            B256::ZERO,
        ];

        let event = decode_log(&topics, &data, pool_addr, None);
        match event.unwrap() {
            PoolEvent::V3Update { tick, .. } => {
                assert_eq!(tick, -100);
            }
            other => panic!("Expected V3Update, got {:?}", other),
        }
    }

    #[test]
    fn test_decode_v3_swap_too_short_data() {
        let topics = vec![
            EventSignatures::swap_v3_topic(),
            B256::ZERO,
            B256::ZERO,
        ];
        // Only 128 bytes instead of 160
        let data = vec![0u8; 128];
        let event = decode_log(&topics, &data, Address::ZERO, None);
        assert!(event.is_none());
    }

    // ── TokenExchange (Curve) decode tests ──

    #[test]
    fn test_decode_token_exchange_event() {
        let pool_addr = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
        let topics = vec![
            EventSignatures::token_exchange_topic(),
            B256::ZERO, // buyer (indexed)
        ];
        let data = vec![0u8; 128]; // sold_id, tokens_sold, bought_id, tokens_bought

        let event = decode_log(&topics, &data, pool_addr, None);
        assert!(event.is_some());

        match event.unwrap() {
            PoolEvent::ReserveUpdate {
                pool,
                protocol,
                reserve0,
                reserve1,
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(protocol, ProtocolType::Curve);
                // Zeroes because Curve needs on-chain refresh
                assert_eq!(reserve0, U256::ZERO);
                assert_eq!(reserve1, U256::ZERO);
            }
            other => panic!("Expected ReserveUpdate, got {:?}", other),
        }
    }

    // ── PairCreated decode tests ──

    #[test]
    fn test_decode_pair_created_event() {
        let token0 = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
        let token1 = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC
        let pair = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");

        // topic[1] = token0, left-padded in 32 bytes
        let mut topic1 = [0u8; 32];
        topic1[12..32].copy_from_slice(token0.as_slice());

        // topic[2] = token1, left-padded in 32 bytes
        let mut topic2 = [0u8; 32];
        topic2[12..32].copy_from_slice(token1.as_slice());

        // data: pair address (left-padded in 32 bytes) + allPairsLength (32 bytes)
        let mut data = vec![0u8; 64];
        data[12..32].copy_from_slice(pair.as_slice());
        // allPairsLength = 1000
        let len_bytes = u256_to_be_bytes(U256::from(1000u64));
        data[32..64].copy_from_slice(&len_bytes);

        let topics = vec![
            EventSignatures::pair_created_topic(),
            B256::from(topic1),
            B256::from(topic2),
        ];

        let event = decode_log(&topics, &data, Address::ZERO, None);
        assert!(event.is_some());

        match event.unwrap() {
            PoolEvent::PoolCreated {
                token0: t0,
                token1: t1,
                pool: p,
            } => {
                assert_eq!(t0, token0);
                assert_eq!(t1, token1);
                assert_eq!(p, pair);
            }
            other => panic!("Expected PoolCreated, got {:?}", other),
        }
    }

    #[test]
    fn test_decode_pair_created_insufficient_topics() {
        let topics = vec![
            EventSignatures::pair_created_topic(),
            B256::ZERO,
            // Missing third topic
        ];
        let data = vec![0u8; 64];
        let event = decode_log(&topics, &data, Address::ZERO, None);
        assert!(event.is_none());
    }

    #[test]
    fn test_decode_pair_created_insufficient_data() {
        let topics = vec![
            EventSignatures::pair_created_topic(),
            B256::ZERO,
            B256::ZERO,
        ];
        // Only 32 bytes instead of 64
        let data = vec![0u8; 32];
        let event = decode_log(&topics, &data, Address::ZERO, None);
        assert!(event.is_none());
    }

    // ── Unknown event tests ──

    #[test]
    fn test_decode_unknown_event_returns_none() {
        let unknown_topic = B256::from([0xABu8; 32]);
        let topics = vec![unknown_topic];
        let data = vec![0u8; 64];

        let event = decode_log(&topics, &data, Address::ZERO, None);
        assert!(event.is_none());
    }

    #[test]
    fn test_decode_empty_topics_returns_none() {
        let topics: Vec<B256> = vec![];
        let data = vec![0u8; 64];

        let event = decode_log(&topics, &data, Address::ZERO, None);
        assert!(event.is_none());
    }
}
