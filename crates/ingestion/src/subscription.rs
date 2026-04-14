use crate::event_decoder::PoolEvent;
use tokio::sync::broadcast;
use tracing::warn;

/// Channel capacity for event dispatch
const CHANNEL_CAPACITY: usize = 10_000;

/// Event channels for broadcasting decoded events to consumers
pub struct EventChannels {
    pub pool_updates_tx: broadcast::Sender<PoolEvent>,
    pub new_block_tx: broadcast::Sender<NewBlockEvent>,
    pub pending_tx_tx: broadcast::Sender<PendingTxEvent>,
}

/// New block notification
#[derive(Debug, Clone)]
pub struct NewBlockEvent {
    pub block_number: u64,
    pub timestamp: u64,
    pub base_fee: u128,
    pub gas_limit: u64,
}

/// Pending transaction notification
#[derive(Debug, Clone)]
pub struct PendingTxEvent {
    pub tx_hash: alloy::primitives::B256,
    pub from: alloy::primitives::Address,
    pub to: Option<alloy::primitives::Address>,
    pub value: alloy::primitives::U256,
    pub input: Vec<u8>,
    pub gas_price: u128,
}

impl EventChannels {
    pub fn new() -> Self {
        let (pool_updates_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let (new_block_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let (pending_tx_tx, _) = broadcast::channel(CHANNEL_CAPACITY);

        Self {
            pool_updates_tx,
            new_block_tx,
            pending_tx_tx,
        }
    }

    /// Subscribe to pool update events
    pub fn subscribe_pool_updates(&self) -> broadcast::Receiver<PoolEvent> {
        self.pool_updates_tx.subscribe()
    }

    /// Subscribe to new block events
    pub fn subscribe_new_blocks(&self) -> broadcast::Receiver<NewBlockEvent> {
        self.new_block_tx.subscribe()
    }

    /// Subscribe to pending transaction events
    pub fn subscribe_pending_txs(&self) -> broadcast::Receiver<PendingTxEvent> {
        self.pending_tx_tx.subscribe()
    }

    /// Dispatch a pool update event
    pub fn dispatch_pool_update(&self, event: PoolEvent) {
        match self.pool_updates_tx.send(event) {
            Ok(_n) => {}
            Err(_) => warn!("No pool update subscribers"),
        }
    }

    /// Dispatch a new block event
    pub fn dispatch_new_block(&self, event: NewBlockEvent) {
        match self.new_block_tx.send(event) {
            Ok(_) => {}
            Err(_) => warn!("No new block subscribers"),
        }
    }

    /// Dispatch a pending tx event
    pub fn dispatch_pending_tx(&self, event: PendingTxEvent) {
        match self.pending_tx_tx.send(event) {
            Ok(_) => {}
            Err(_) => warn!("No pending tx subscribers"),
        }
    }

    /// Get current subscriber counts
    pub fn subscriber_counts(&self) -> (usize, usize, usize) {
        (
            self.pool_updates_tx.receiver_count(),
            self.new_block_tx.receiver_count(),
            self.pending_tx_tx.receiver_count(),
        )
    }
}

impl Default for EventChannels {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256, U256};

    // ── Channel creation tests ──

    #[test]
    fn test_event_channels_new() {
        let channels = EventChannels::new();
        let (pool, block, tx) = channels.subscriber_counts();
        assert_eq!(pool, 0);
        assert_eq!(block, 0);
        assert_eq!(tx, 0);
    }

    #[test]
    fn test_event_channels_default() {
        let channels = EventChannels::default();
        let (pool, block, tx) = channels.subscriber_counts();
        assert_eq!(pool, 0);
        assert_eq!(block, 0);
        assert_eq!(tx, 0);
    }

    // ── Subscribe/dispatch pool updates ──

    #[tokio::test]
    async fn test_subscribe_and_dispatch_pool_update() {
        let channels = EventChannels::new();
        let mut rx = channels.subscribe_pool_updates();

        let event = PoolEvent::ReserveUpdate {
            pool: Address::ZERO,
            protocol: aether_common::types::ProtocolType::UniswapV2,
            reserve0: U256::from(1000u64),
            reserve1: U256::from(2000u64),
        };

        channels.dispatch_pool_update(event);

        let received = rx.recv().await.expect("should receive pool update");
        let PoolEvent::ReserveUpdate {
            reserve0, reserve1, ..
        } = received
        else {
            panic!("dispatch returned unexpected variant");
        };
        assert_eq!(reserve0, U256::from(1000u64));
        assert_eq!(reserve1, U256::from(2000u64));
    }

    // ── Subscribe/dispatch new block ──

    #[tokio::test]
    async fn test_subscribe_and_dispatch_new_block() {
        let channels = EventChannels::new();
        let mut rx = channels.subscribe_new_blocks();

        let event = NewBlockEvent {
            block_number: 18_000_000,
            timestamp: 1_700_000_000,
            base_fee: 30_000_000_000, // 30 gwei
            gas_limit: 30_000_000,
        };

        channels.dispatch_new_block(event);

        let received = rx.recv().await.expect("should receive new block");
        assert_eq!(received.block_number, 18_000_000);
        assert_eq!(received.timestamp, 1_700_000_000);
        assert_eq!(received.base_fee, 30_000_000_000);
        assert_eq!(received.gas_limit, 30_000_000);
    }

    // ── Subscribe/dispatch pending tx ──

    #[tokio::test]
    async fn test_subscribe_and_dispatch_pending_tx() {
        let channels = EventChannels::new();
        let mut rx = channels.subscribe_pending_txs();

        let event = PendingTxEvent {
            tx_hash: B256::ZERO,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            value: U256::from(1_000_000_000_000_000_000u64),
            input: vec![0xaa, 0xbb],
            gas_price: 50_000_000_000,
        };

        channels.dispatch_pending_tx(event);

        let received = rx.recv().await.expect("should receive pending tx");
        assert_eq!(received.value, U256::from(1_000_000_000_000_000_000u64));
        assert_eq!(received.input, vec![0xaa, 0xbb]);
        assert_eq!(received.gas_price, 50_000_000_000);
    }

    // ── Multiple subscribers ──

    #[tokio::test]
    async fn test_multiple_pool_update_subscribers() {
        let channels = EventChannels::new();
        let mut rx1 = channels.subscribe_pool_updates();
        let mut rx2 = channels.subscribe_pool_updates();
        let mut rx3 = channels.subscribe_pool_updates();

        assert_eq!(channels.subscriber_counts().0, 3);

        let event = PoolEvent::V3Update {
            pool: Address::ZERO,
            sqrt_price_x96: U256::from(999u64),
            liquidity: 12345,
            tick: -50,
        };

        channels.dispatch_pool_update(event);

        // All three should receive the event
        let r1 = rx1.recv().await.expect("rx1 should receive");
        let r2 = rx2.recv().await.expect("rx2 should receive");
        let r3 = rx3.recv().await.expect("rx3 should receive");

        for received in [r1, r2, r3] {
            let PoolEvent::V3Update { tick, liquidity, .. } = received else {
                panic!("dispatch returned unexpected variant");
            };
            assert_eq!(tick, -50);
            assert_eq!(liquidity, 12345);
        }
    }

    #[tokio::test]
    async fn test_multiple_block_subscribers() {
        let channels = EventChannels::new();
        let mut rx1 = channels.subscribe_new_blocks();
        let mut rx2 = channels.subscribe_new_blocks();

        assert_eq!(channels.subscriber_counts().1, 2);

        channels.dispatch_new_block(NewBlockEvent {
            block_number: 42,
            timestamp: 100,
            base_fee: 10,
            gas_limit: 30_000_000,
        });

        let r1 = rx1.recv().await.unwrap();
        let r2 = rx2.recv().await.unwrap();
        assert_eq!(r1.block_number, 42);
        assert_eq!(r2.block_number, 42);
    }

    // ── Subscriber count tracking ──

    #[test]
    fn test_subscriber_count_tracks_subscriptions() {
        let channels = EventChannels::new();

        assert_eq!(channels.subscriber_counts(), (0, 0, 0));

        let _rx1 = channels.subscribe_pool_updates();
        assert_eq!(channels.subscriber_counts(), (1, 0, 0));

        let _rx2 = channels.subscribe_new_blocks();
        assert_eq!(channels.subscriber_counts(), (1, 1, 0));

        let _rx3 = channels.subscribe_pending_txs();
        assert_eq!(channels.subscriber_counts(), (1, 1, 1));

        let _rx4 = channels.subscribe_pool_updates();
        assert_eq!(channels.subscriber_counts(), (2, 1, 1));
    }

    #[test]
    fn test_subscriber_count_decreases_on_drop() {
        let channels = EventChannels::new();

        let rx1 = channels.subscribe_pool_updates();
        let rx2 = channels.subscribe_pool_updates();
        assert_eq!(channels.subscriber_counts().0, 2);

        drop(rx1);
        assert_eq!(channels.subscriber_counts().0, 1);

        drop(rx2);
        assert_eq!(channels.subscriber_counts().0, 0);
    }

    // ── Dispatch without subscribers ──

    #[test]
    fn test_dispatch_pool_update_no_subscribers_does_not_panic() {
        let channels = EventChannels::new();
        channels.dispatch_pool_update(PoolEvent::ReserveUpdate {
            pool: Address::ZERO,
            protocol: aether_common::types::ProtocolType::UniswapV2,
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
        });
        // Should not panic
    }

    #[test]
    fn test_dispatch_new_block_no_subscribers_does_not_panic() {
        let channels = EventChannels::new();
        channels.dispatch_new_block(NewBlockEvent {
            block_number: 1,
            timestamp: 0,
            base_fee: 0,
            gas_limit: 0,
        });
        // Should not panic
    }

    #[test]
    fn test_dispatch_pending_tx_no_subscribers_does_not_panic() {
        let channels = EventChannels::new();
        channels.dispatch_pending_tx(PendingTxEvent {
            tx_hash: B256::ZERO,
            from: Address::ZERO,
            to: None,
            value: U256::ZERO,
            input: vec![],
            gas_price: 0,
        });
        // Should not panic
    }

    // ── Event struct tests ──

    #[test]
    fn test_new_block_event_clone() {
        let event = NewBlockEvent {
            block_number: 18_000_000,
            timestamp: 1_700_000_000,
            base_fee: 30_000_000_000,
            gas_limit: 30_000_000,
        };
        let cloned = event.clone();
        assert_eq!(cloned.block_number, event.block_number);
        assert_eq!(cloned.timestamp, event.timestamp);
        assert_eq!(cloned.base_fee, event.base_fee);
        assert_eq!(cloned.gas_limit, event.gas_limit);
    }

    #[test]
    fn test_pending_tx_event_clone() {
        let event = PendingTxEvent {
            tx_hash: B256::ZERO,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            value: U256::from(1u64),
            input: vec![0x01, 0x02],
            gas_price: 100,
        };
        let cloned = event.clone();
        assert_eq!(cloned.tx_hash, event.tx_hash);
        assert_eq!(cloned.input, event.input);
        assert_eq!(cloned.gas_price, event.gas_price);
    }

    #[test]
    fn test_pending_tx_event_to_is_optional() {
        let event = PendingTxEvent {
            tx_hash: B256::ZERO,
            from: Address::ZERO,
            to: None,
            value: U256::ZERO,
            input: vec![],
            gas_price: 0,
        };
        assert!(event.to.is_none());
    }
}
