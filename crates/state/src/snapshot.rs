use crate::price_graph::PriceGraph;
use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Immutable snapshot of the price graph state at a particular block.
///
/// Snapshots are produced by the writer (state updater) and consumed by
/// readers (detector threads) through the [`SnapshotManager`]. Because
/// they are wrapped in `Arc`, readers get zero-copy access.
#[derive(Debug, Clone)]
pub struct GraphSnapshot {
    /// The price graph at this point in time.
    pub graph: PriceGraph,
    /// Ethereum block number this snapshot corresponds to.
    pub block_number: u64,
    /// Wall-clock timestamp in nanoseconds when the snapshot was created.
    pub timestamp_ns: i64,
    /// Monotonically increasing snapshot version.
    pub version: u64,
}

impl GraphSnapshot {
    pub fn new(graph: PriceGraph, block_number: u64, timestamp_ns: i64, version: u64) -> Self {
        Self {
            graph,
            block_number,
            timestamp_ns,
            version,
        }
    }
}

/// MVCC snapshot manager using `ArcSwap` for lock-free publish/load.
///
/// Writers atomically swap in new snapshot versions; readers obtain
/// zero-copy immutable references via `Guard<Arc<GraphSnapshot>>`.
/// This is the core concurrency primitive that allows the detection
/// engine to read a consistent graph while the state updater writes
/// the next version.
pub struct SnapshotManager {
    /// The current published snapshot.
    current: Arc<ArcSwap<GraphSnapshot>>,
    /// Monotonically increasing version counter.
    version_counter: AtomicU64,
}

impl SnapshotManager {
    /// Create a new snapshot manager with an initial (empty) graph.
    pub fn new(initial_graph: PriceGraph) -> Self {
        let snapshot = GraphSnapshot::new(initial_graph, 0, 0, 0);
        Self {
            current: Arc::new(ArcSwap::from_pointee(snapshot)),
            version_counter: AtomicU64::new(0),
        }
    }

    /// Get a zero-copy reference to the current snapshot.
    ///
    /// The returned `Guard` keeps the snapshot alive for the duration
    /// of the borrow. This is extremely cheap (no atomic increment on
    /// the fast path in many cases).
    #[inline]
    pub fn load(&self) -> arc_swap::Guard<Arc<GraphSnapshot>> {
        self.current.load()
    }

    /// Get a full `Arc<GraphSnapshot>` clone (increments the refcount).
    ///
    /// Use this when you need to hold onto the snapshot for longer than
    /// a single scope, e.g. passing it to a spawned task.
    #[inline]
    pub fn load_full(&self) -> Arc<GraphSnapshot> {
        self.current.load_full()
    }

    /// Atomically publish a new snapshot, replacing the current one.
    ///
    /// Existing readers holding a `Guard` or `Arc` to the old snapshot
    /// are unaffected -- the old data is kept alive until all references
    /// are dropped.
    pub fn publish(&self, graph: PriceGraph, block_number: u64, timestamp_ns: i64) {
        let version = self.version_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let snapshot = Arc::new(GraphSnapshot::new(graph, block_number, timestamp_ns, version));
        self.current.store(snapshot);
    }

    /// Get the current version number (number of publishes so far).
    #[inline]
    pub fn version(&self) -> u64 {
        self.version_counter.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_graph(n: usize) -> PriceGraph {
        PriceGraph::new(n)
    }

    #[test]
    fn test_snapshot_creation() {
        let snap = GraphSnapshot::new(make_graph(5), 100, 12345, 1);
        assert_eq!(snap.block_number, 100);
        assert_eq!(snap.timestamp_ns, 12345);
        assert_eq!(snap.version, 1);
        assert_eq!(snap.graph.num_vertices(), 5);
    }

    #[test]
    fn test_manager_initial_state() {
        let mgr = SnapshotManager::new(make_graph(10));
        assert_eq!(mgr.version(), 0);

        let snap = mgr.load();
        assert_eq!(snap.block_number, 0);
        assert_eq!(snap.version, 0);
        assert_eq!(snap.graph.num_vertices(), 10);
    }

    #[test]
    fn test_publish_increments_version() {
        let mgr = SnapshotManager::new(make_graph(5));
        assert_eq!(mgr.version(), 0);

        mgr.publish(make_graph(5), 100, 1000);
        assert_eq!(mgr.version(), 1);

        mgr.publish(make_graph(5), 101, 2000);
        assert_eq!(mgr.version(), 2);

        mgr.publish(make_graph(5), 102, 3000);
        assert_eq!(mgr.version(), 3);
    }

    #[test]
    fn test_publish_updates_snapshot() {
        let mgr = SnapshotManager::new(make_graph(5));

        mgr.publish(make_graph(8), 42, 999);
        let snap = mgr.load();
        assert_eq!(snap.block_number, 42);
        assert_eq!(snap.timestamp_ns, 999);
        assert_eq!(snap.version, 1);
        assert_eq!(snap.graph.num_vertices(), 8);
    }

    #[test]
    fn test_load_full_returns_arc() {
        let mgr = SnapshotManager::new(make_graph(5));
        mgr.publish(make_graph(7), 50, 123);

        let arc = mgr.load_full();
        assert_eq!(arc.block_number, 50);
        assert_eq!(arc.graph.num_vertices(), 7);
    }

    #[test]
    fn test_old_snapshot_survives_publish() {
        let mgr = SnapshotManager::new(make_graph(5));
        mgr.publish(make_graph(5), 100, 1000);

        // Hold a reference to the current snapshot.
        let old_snap = mgr.load_full();
        assert_eq!(old_snap.block_number, 100);

        // Publish a new one.
        mgr.publish(make_graph(5), 200, 2000);

        // Old snapshot is still accessible.
        assert_eq!(old_snap.block_number, 100);
        assert_eq!(old_snap.version, 1);

        // New snapshot is different.
        let new_snap = mgr.load();
        assert_eq!(new_snap.block_number, 200);
        assert_eq!(new_snap.version, 2);
    }

    #[test]
    fn test_concurrent_reads() {
        use std::sync::Arc as StdArc;

        let mgr = StdArc::new(SnapshotManager::new(make_graph(5)));
        mgr.publish(make_graph(5), 100, 1000);

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let mgr = StdArc::clone(&mgr);
                std::thread::spawn(move || {
                    let snap = mgr.load();
                    assert_eq!(snap.block_number, 100);
                    assert_eq!(snap.graph.num_vertices(), 5);
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_concurrent_read_write() {
        use std::sync::Arc as StdArc;

        let mgr = StdArc::new(SnapshotManager::new(make_graph(5)));

        // Spawn a writer.
        let writer_mgr = StdArc::clone(&mgr);
        let writer = std::thread::spawn(move || {
            for i in 1..=100 {
                writer_mgr.publish(make_graph(5), i, i as i64 * 1000);
            }
        });

        // Spawn readers that continuously load.
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let mgr = StdArc::clone(&mgr);
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        let snap = mgr.load();
                        // Snapshot should always be internally consistent.
                        assert_eq!(snap.graph.num_vertices(), 5);
                        // Block number should be non-negative.
                        assert!(snap.block_number <= 100);
                    }
                })
            })
            .collect();

        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }

        // Final state should reflect the last publish.
        let final_snap = mgr.load();
        assert_eq!(final_snap.block_number, 100);
        assert_eq!(mgr.version(), 100);
    }
}
