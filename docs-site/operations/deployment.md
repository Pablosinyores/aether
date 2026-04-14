# Deployment

This page covers deploying Aether across different environments — from local development to production.

## Local Development (Docker Compose)

The fastest way to run the full stack locally:

```bash
./scripts/deploy.sh docker up
```

This starts:
- **aether-rust** — Rust core (gRPC server)
- **aether-go** — Go executor (bundle construction, risk, monitoring)
- **Prometheus** — Metrics collection

To stop:

```bash
./scripts/deploy.sh docker down
```

### Manual Docker Compose

```bash
# Start infrastructure only
docker compose -f deploy/docker/docker-compose.yml up -d prometheus

# Start Rust core
cargo run --release --bin aether-grpc-server

# Start Go executor (separate terminal)
go run ./cmd/executor
```

## Production Deployment

### Build

```bash
# Build all components
./scripts/deploy.sh build

# Or build individually:
RUSTFLAGS="-C target-cpu=native" cargo build --release   # Rust (with LTO)
go build -o bin/aether-executor ./cmd/executor            # Go
cd contracts && forge build                               # Solidity
```

### Deploy

```bash
# Deploy to staging (runs tests first)
./scripts/deploy.sh deploy staging

# Deploy to production
./scripts/deploy.sh deploy production

# Check deployment status
./scripts/deploy.sh status production

# Rollback if issues
./scripts/deploy.sh rollback production
```

### systemd Services

Production runs as two systemd services:

**`aether-rust.service`** — Rust core (gRPC server)
```ini
# deploy/systemd/aether-rust.service
[Unit]
Description=Aether Rust Core
After=network.target

[Service]
Type=simple
ExecStart=/opt/aether/bin/aether-grpc-server
WorkingDirectory=/opt/aether
Nice=-20
CPUAffinity=0 1 2 3
IOSchedulingClass=realtime
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

**`aether-go.service`** — Go executor
```ini
# deploy/systemd/aether-go.service
[Unit]
Description=Aether Go Executor
After=aether-rust.service
Requires=aether-rust.service

[Service]
Type=simple
ExecStart=/opt/aether/bin/aether-executor
WorkingDirectory=/opt/aether
Environment=GOMAXPROCS=2 GOGC=200
Nice=-15
CPUAffinity=4 5
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

::: tip Start Order
The Go executor depends on the Rust core. systemd handles this via `After=` and `Requires=` directives. For manual starts, always start Rust first, wait 2 seconds, then start Go.
:::

## Infrastructure Requirements

### Hardware

- **Server:** Bare metal, co-located at Equinix NY5 (same data center as major Ethereum node providers)
- **CPU:** 6+ cores. Rust core pinned to CPU 0-3, Go executor pinned to CPU 4-5.
- **Memory:** 4+ GB (Rust <2GB RSS, Go <512MB RSS)
- **Storage:** SSD for logs and PostgreSQL

### Kernel Tuning

```bash
# Network latency optimization
sysctl -w net.ipv4.tcp_nodelay=1
sysctl -w net.ipv4.tcp_low_latency=1

# Memory optimization
sysctl -w vm.swappiness=0

# Huge pages (for Rust arena allocators)
sysctl -w vm.nr_hugepages=1024

# Allow realtime scheduling
sysctl -w kernel.sched_rt_runtime_us=-1
```

### CPU Pinning

| Component | CPUs | Nice | IO Scheduling |
|---|---|---|---|
| Rust core | 0-3 | -20 | Realtime |
| Go executor | 4-5 | -15 | Best-effort |

### Failover

- Backup server with standby Reth node + Rust core + Go executor
- Consul health checks for automatic failover
- Standby server is always synced and ready to take over

## Ansible Provisioning

Server provisioning is managed via `deploy/ansible/`:

```bash
# Run the provisioning and deployment playbook
ansible-playbook -i deploy/ansible/inventory.yml deploy/ansible/playbook.yml
```

The inventory file (`deploy/ansible/inventory.yml`) defines target hosts. The playbook (`deploy/ansible/playbook.yml`) handles server setup and application deployment.

## Network Security

- **Firewall:** iptables rules allowing only necessary inbound/outbound traffic
- **Admin access:** WireGuard VPN only
- **gRPC:** mTLS if communicating over network (UDS is default for same-machine)
- **Outbound:** Pinned to known IP ranges (Flashbots, builder endpoints, node providers)
- **No arbitrary outbound traffic** — all connections are explicitly allowed

## Verify Deployment

After deployment, run the health check sequence:

```bash
# 1. Service status
systemctl status aether-rust aether-go

# 2. gRPC health
grpcurl -plaintext localhost:50051 aether.HealthService/Check

# 3. Metrics endpoint
curl -s http://localhost:9090/metrics | head -20

# 4. Dashboard
curl -s http://localhost:8080/ | head -5
```

See [Runbook](/operations/runbook) for detailed operational procedures.
