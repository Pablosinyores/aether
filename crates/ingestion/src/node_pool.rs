use aether_common::types::NodeState;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::info;

/// Configuration for a single node provider
#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub name: String,
    pub url: String,
    pub node_type: NodeType,
    pub priority: u32,
    pub max_retries: u32,
    pub health_check_interval: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeType {
    WebSocket,
    Ipc,
    Http,
}

/// State tracking for a single node connection
#[derive(Debug)]
pub struct NodeConnection {
    pub config: NodeConfig,
    pub state: NodeState,
    pub last_health_check: Instant,
    pub consecutive_failures: u32,
    pub last_block_seen: u64,
    pub latency_ms: u64,
}

impl NodeConnection {
    pub fn new(config: NodeConfig) -> Self {
        Self {
            config,
            state: NodeState::Connected,
            last_health_check: Instant::now(),
            consecutive_failures: 0,
            last_block_seen: 0,
            latency_ms: 0,
        }
    }

    /// State machine transitions
    pub fn transition(&mut self, new_state: NodeState) {
        info!(
            node = %self.config.name,
            from = ?self.state,
            to = ?new_state,
            "Node state transition"
        );
        self.state = new_state;
    }

    pub fn record_success(&mut self, latency_ms: u64, block: u64) {
        self.consecutive_failures = 0;
        self.latency_ms = latency_ms;
        self.last_block_seen = block;
        self.last_health_check = Instant::now();
        if self.state != NodeState::Healthy {
            self.transition(NodeState::Healthy);
        }
    }

    pub fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= self.config.max_retries {
            self.transition(NodeState::Failed);
        } else if self.consecutive_failures >= 2 {
            self.transition(NodeState::Degraded);
        }
    }

    pub fn is_healthy(&self) -> bool {
        matches!(self.state, NodeState::Healthy | NodeState::Connected)
    }
}

/// Manages a pool of Ethereum node connections
pub struct NodePool {
    nodes: Vec<Arc<RwLock<NodeConnection>>>,
    min_healthy_nodes: usize,
    reconnect_backoff_base: Duration,
    reconnect_backoff_max: Duration,
}

impl NodePool {
    pub fn new(configs: Vec<NodeConfig>, min_healthy_nodes: usize) -> Self {
        let nodes = configs
            .into_iter()
            .map(|c| Arc::new(RwLock::new(NodeConnection::new(c))))
            .collect();
        Self {
            nodes,
            min_healthy_nodes,
            reconnect_backoff_base: Duration::from_millis(100),
            reconnect_backoff_max: Duration::from_secs(30),
        }
    }

    /// Get the best available node (lowest priority number = highest priority)
    pub async fn best_node(&self) -> Option<Arc<RwLock<NodeConnection>>> {
        let mut best: Option<(u32, Arc<RwLock<NodeConnection>>)> = None;
        for node in &self.nodes {
            let n = node.read().await;
            if n.is_healthy() {
                match &best {
                    None => best = Some((n.config.priority, Arc::clone(node))),
                    Some((p, _)) if n.config.priority < *p => {
                        best = Some((n.config.priority, Arc::clone(node)));
                    }
                    _ => {}
                }
            }
        }
        best.map(|(_, node)| node)
    }

    /// Count of healthy nodes
    pub async fn healthy_count(&self) -> usize {
        let mut count = 0;
        for node in &self.nodes {
            if node.read().await.is_healthy() {
                count += 1;
            }
        }
        count
    }

    /// Check if minimum healthy node requirement is met
    pub async fn is_operational(&self) -> bool {
        self.healthy_count().await >= self.min_healthy_nodes
    }

    /// Get all nodes
    pub fn all_nodes(&self) -> &[Arc<RwLock<NodeConnection>>] {
        &self.nodes
    }

    /// Calculate exponential backoff delay for reconnection
    pub fn backoff_delay(&self, attempt: u32) -> Duration {
        let delay = self.reconnect_backoff_base * 2u32.saturating_pow(attempt);
        std::cmp::min(delay, self.reconnect_backoff_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(name: &str, priority: u32) -> NodeConfig {
        NodeConfig {
            name: name.to_string(),
            url: format!("ws://localhost:854{priority}"),
            node_type: NodeType::WebSocket,
            priority,
            max_retries: 3,
            health_check_interval: Duration::from_secs(10),
        }
    }

    // ── NodeConnection state transition tests ──

    #[test]
    fn test_new_connection_starts_connected() {
        let conn = NodeConnection::new(make_config("node1", 1));
        assert_eq!(conn.state, NodeState::Connected);
        assert_eq!(conn.consecutive_failures, 0);
        assert_eq!(conn.last_block_seen, 0);
        assert_eq!(conn.latency_ms, 0);
    }

    #[test]
    fn test_transition_updates_state() {
        let mut conn = NodeConnection::new(make_config("node1", 1));
        assert_eq!(conn.state, NodeState::Connected);

        conn.transition(NodeState::Healthy);
        assert_eq!(conn.state, NodeState::Healthy);

        conn.transition(NodeState::Degraded);
        assert_eq!(conn.state, NodeState::Degraded);

        conn.transition(NodeState::Reconnecting);
        assert_eq!(conn.state, NodeState::Reconnecting);

        conn.transition(NodeState::Failed);
        assert_eq!(conn.state, NodeState::Failed);
    }

    #[test]
    fn test_record_success_transitions_to_healthy() {
        let mut conn = NodeConnection::new(make_config("node1", 1));
        conn.state = NodeState::Degraded;
        conn.consecutive_failures = 2;

        conn.record_success(15, 18_000_000);

        assert_eq!(conn.state, NodeState::Healthy);
        assert_eq!(conn.consecutive_failures, 0);
        assert_eq!(conn.latency_ms, 15);
        assert_eq!(conn.last_block_seen, 18_000_000);
    }

    #[test]
    fn test_record_success_stays_healthy_if_already_healthy() {
        let mut conn = NodeConnection::new(make_config("node1", 1));
        conn.state = NodeState::Healthy;

        conn.record_success(10, 18_000_001);

        assert_eq!(conn.state, NodeState::Healthy);
        assert_eq!(conn.latency_ms, 10);
        assert_eq!(conn.last_block_seen, 18_000_001);
    }

    #[test]
    fn test_single_failure_does_not_degrade() {
        let mut conn = NodeConnection::new(make_config("node1", 1));
        conn.state = NodeState::Healthy;

        conn.record_failure();

        // 1 failure, max_retries=3 => stays Healthy (no transition triggered)
        assert_eq!(conn.consecutive_failures, 1);
        assert_eq!(conn.state, NodeState::Healthy);
    }

    #[test]
    fn test_two_failures_degrades() {
        let mut conn = NodeConnection::new(make_config("node1", 1));
        conn.state = NodeState::Healthy;

        conn.record_failure();
        conn.record_failure();

        assert_eq!(conn.consecutive_failures, 2);
        assert_eq!(conn.state, NodeState::Degraded);
    }

    #[test]
    fn test_max_retries_failures_causes_failed() {
        let mut conn = NodeConnection::new(make_config("node1", 1));
        conn.state = NodeState::Healthy;

        conn.record_failure(); // 1
        conn.record_failure(); // 2 -> Degraded
        conn.record_failure(); // 3 -> Failed (max_retries = 3)

        assert_eq!(conn.consecutive_failures, 3);
        assert_eq!(conn.state, NodeState::Failed);
    }

    #[test]
    fn test_is_healthy_connected() {
        let conn = NodeConnection::new(make_config("node1", 1));
        assert!(conn.is_healthy());
    }

    #[test]
    fn test_is_healthy_healthy_state() {
        let mut conn = NodeConnection::new(make_config("node1", 1));
        conn.state = NodeState::Healthy;
        assert!(conn.is_healthy());
    }

    #[test]
    fn test_is_not_healthy_degraded() {
        let mut conn = NodeConnection::new(make_config("node1", 1));
        conn.state = NodeState::Degraded;
        assert!(!conn.is_healthy());
    }

    #[test]
    fn test_is_not_healthy_failed() {
        let mut conn = NodeConnection::new(make_config("node1", 1));
        conn.state = NodeState::Failed;
        assert!(!conn.is_healthy());
    }

    #[test]
    fn test_is_not_healthy_reconnecting() {
        let mut conn = NodeConnection::new(make_config("node1", 1));
        conn.state = NodeState::Reconnecting;
        assert!(!conn.is_healthy());
    }

    #[test]
    fn test_record_success_resets_failures_after_degradation() {
        let mut conn = NodeConnection::new(make_config("node1", 1));
        conn.record_failure();
        conn.record_failure();
        assert_eq!(conn.state, NodeState::Degraded);
        assert_eq!(conn.consecutive_failures, 2);

        conn.record_success(5, 100);
        assert_eq!(conn.state, NodeState::Healthy);
        assert_eq!(conn.consecutive_failures, 0);
    }

    #[test]
    fn test_node_type_variants() {
        assert_eq!(NodeType::WebSocket, NodeType::WebSocket);
        assert_eq!(NodeType::Ipc, NodeType::Ipc);
        assert_ne!(NodeType::WebSocket, NodeType::Ipc);
    }

    #[test]
    fn test_http_node_type() {
        let config = NodeConfig {
            name: "http-node".to_string(),
            url: "http://localhost:8545".to_string(),
            node_type: NodeType::Http,
            priority: 3,
            max_retries: 3,
            health_check_interval: Duration::from_secs(10),
        };
        assert_eq!(config.node_type, NodeType::Http);
        assert_ne!(NodeType::Http, NodeType::WebSocket);
        assert_ne!(NodeType::Http, NodeType::Ipc);
    }

    // ── NodePool tests ──

    #[tokio::test]
    async fn test_pool_creation_with_configs() {
        let configs = vec![
            make_config("alchemy", 1),
            make_config("quicknode", 2),
            make_config("local-reth", 0),
        ];
        let pool = NodePool::new(configs, 2);

        assert_eq!(pool.all_nodes().len(), 3);
    }

    #[tokio::test]
    async fn test_healthy_count_all_connected() {
        let configs = vec![
            make_config("node1", 1),
            make_config("node2", 2),
            make_config("node3", 3),
        ];
        let pool = NodePool::new(configs, 2);

        // All nodes start as Connected, which counts as healthy
        assert_eq!(pool.healthy_count().await, 3);
    }

    #[tokio::test]
    async fn test_healthy_count_with_failed_nodes() {
        let configs = vec![
            make_config("node1", 1),
            make_config("node2", 2),
        ];
        let pool = NodePool::new(configs, 1);

        // Fail the second node
        {
            let mut n = pool.all_nodes()[1].write().await;
            n.transition(NodeState::Failed);
        }

        assert_eq!(pool.healthy_count().await, 1);
    }

    #[tokio::test]
    async fn test_is_operational_when_enough_healthy() {
        let configs = vec![
            make_config("node1", 1),
            make_config("node2", 2),
            make_config("node3", 3),
        ];
        let pool = NodePool::new(configs, 2);

        assert!(pool.is_operational().await);
    }

    #[tokio::test]
    async fn test_is_not_operational_when_too_few_healthy() {
        let configs = vec![
            make_config("node1", 1),
            make_config("node2", 2),
        ];
        let pool = NodePool::new(configs, 2);

        // Fail one node
        {
            let mut n = pool.all_nodes()[0].write().await;
            n.transition(NodeState::Failed);
        }

        assert!(!pool.is_operational().await);
    }

    #[tokio::test]
    async fn test_best_node_returns_lowest_priority() {
        let configs = vec![
            make_config("alchemy", 2),
            make_config("quicknode", 3),
            make_config("local-reth", 1),
        ];
        let pool = NodePool::new(configs, 1);

        let best = pool.best_node().await.expect("should have a best node");
        let best_read = best.read().await;
        assert_eq!(best_read.config.name, "local-reth");
        assert_eq!(best_read.config.priority, 1);
    }

    #[tokio::test]
    async fn test_best_node_skips_unhealthy() {
        let configs = vec![
            make_config("primary", 1),
            make_config("secondary", 2),
        ];
        let pool = NodePool::new(configs, 1);

        // Fail the primary node
        {
            let mut n = pool.all_nodes()[0].write().await;
            n.transition(NodeState::Failed);
        }

        let best = pool.best_node().await.expect("should have a best node");
        let best_read = best.read().await;
        assert_eq!(best_read.config.name, "secondary");
    }

    #[tokio::test]
    async fn test_best_node_returns_none_when_all_failed() {
        let configs = vec![
            make_config("node1", 1),
            make_config("node2", 2),
        ];
        let pool = NodePool::new(configs, 1);

        for node in pool.all_nodes() {
            let mut n = node.write().await;
            n.transition(NodeState::Failed);
        }

        assert!(pool.best_node().await.is_none());
    }

    #[tokio::test]
    async fn test_best_node_empty_pool() {
        let pool = NodePool::new(vec![], 0);
        assert!(pool.best_node().await.is_none());
    }

    // ── Backoff calculation tests ──

    #[test]
    fn test_backoff_delay_attempt_0() {
        let pool = NodePool::new(vec![], 0);
        // 100ms * 2^0 = 100ms
        assert_eq!(pool.backoff_delay(0), Duration::from_millis(100));
    }

    #[test]
    fn test_backoff_delay_attempt_1() {
        let pool = NodePool::new(vec![], 0);
        // 100ms * 2^1 = 200ms
        assert_eq!(pool.backoff_delay(1), Duration::from_millis(200));
    }

    #[test]
    fn test_backoff_delay_attempt_2() {
        let pool = NodePool::new(vec![], 0);
        // 100ms * 2^2 = 400ms
        assert_eq!(pool.backoff_delay(2), Duration::from_millis(400));
    }

    #[test]
    fn test_backoff_delay_attempt_5() {
        let pool = NodePool::new(vec![], 0);
        // 100ms * 2^5 = 3200ms
        assert_eq!(pool.backoff_delay(5), Duration::from_millis(3200));
    }

    #[test]
    fn test_backoff_delay_capped_at_max() {
        let pool = NodePool::new(vec![], 0);
        // 100ms * 2^10 = 102400ms, but max is 30000ms
        assert_eq!(pool.backoff_delay(10), Duration::from_secs(30));
    }

    #[test]
    fn test_backoff_delay_very_large_attempt() {
        let pool = NodePool::new(vec![], 0);
        // Should be capped, not overflow
        assert_eq!(pool.backoff_delay(100), Duration::from_secs(30));
    }

    // ── NodeConfig tests ──

    #[test]
    fn test_node_config_clone() {
        let config = make_config("test", 1);
        let cloned = config.clone();
        assert_eq!(cloned.name, "test");
        assert_eq!(cloned.priority, 1);
        assert_eq!(cloned.node_type, NodeType::WebSocket);
    }

    #[test]
    fn test_ipc_node_config() {
        let config = NodeConfig {
            name: "local-reth".to_string(),
            url: "/tmp/reth.ipc".to_string(),
            node_type: NodeType::Ipc,
            priority: 0,
            max_retries: 5,
            health_check_interval: Duration::from_secs(5),
        };
        assert_eq!(config.node_type, NodeType::Ipc);
        assert_eq!(config.max_retries, 5);
    }
}
