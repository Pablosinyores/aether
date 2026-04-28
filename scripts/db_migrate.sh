#!/usr/bin/env bash
# Apply trade-ledger migrations against $DATABASE_URL using sqlx-cli.
#
# Usage:
#   ./scripts/db_migrate.sh
#
# Loads .env if present so DATABASE_URL can live there. Requires sqlx-cli:
#   cargo install sqlx-cli --no-default-features --features postgres,native-tls

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [[ -f .env ]]; then
    # shellcheck disable=SC1091
    set -a; source .env; set +a
fi

if [[ -z "${DATABASE_URL:-}" ]]; then
    echo "DATABASE_URL not set. Export it or add to .env. Aborting." >&2
    exit 1
fi

if ! command -v sqlx >/dev/null 2>&1; then
    echo "sqlx-cli missing. Install with:" >&2
    echo "  cargo install sqlx-cli --no-default-features --features postgres,native-tls" >&2
    exit 1
fi

exec sqlx migrate run --source "$REPO_ROOT/migrations"
