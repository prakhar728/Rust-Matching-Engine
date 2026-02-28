# CLOB (Phase -1)

Web2-first CLOB project with on-chain compatibility goals for later NEAR settlement integration.

## Docker Setup

### Files
- `Dockerfile` (production-style Rust API build)
- `Dockerfile.dev` (development image)
- `docker-compose.yml` (api + postgres + redis + migrator)

### Current State
- API binary name is currently a placeholder: `clob-api`.
- If Rust API is not implemented yet, run infra only.

## Common Commands

### 1) Start infra only (recommended right now)
```bash
docker compose up -d postgres redis
```

### 2) Start full stack (after Rust API binary exists)
```bash
docker compose up --build
```

### 3) Stop all services and remove containers/network
```bash
docker compose down
```

### 4) Stop all services and remove volumes (resets DB/cache data)
```bash
docker compose down -v
```

## Environment Notes
- Set API binary name when available:
```env
APP_BIN=<your_binary_name>
```
- You can place env values in `.env` at repo root.

## Next Step
- Create DB migrations (`markets`, `orders`, `order_events`, `trades`, `balances`, `snapshots`) and wire `MIGRATION_COMMAND` in compose.

## Document Structure
- `docs/README.md` (documentation index)
- `docs/product/vision.md`
- `docs/product/scope.md`
- `docs/architecture/system-overview.md`
- `docs/architecture/hybrid-near-model.md`
- `docs/api/rest-api.md`
- `docs/api/websocket-api.md`
- `docs/database/schema.md`
- `docs/database/migrations.md`
- `docs/security/signing-and-hashing.md`
- `docs/testing/test-strategy.md`
- `docs/testing/performance-plan.md`
- `docs/ops/deployment.md`
- `docs/ops/environment.md`
- `docs/runbooks/recovery.md`
- `docs/runbooks/incident.md`
- `migrations/README.md`
