# Deployment

## Running the Server Manually

```bash
# SQLite (default, zero-infra) — port assigned by OS, printed to stdout
proviz-server --storage sqlite --db-path ./proviz.db

# Force a specific port
proviz-server --storage sqlite --db-path ./proviz.db --port 63130

# PostgreSQL (shares existing DB - tables are pz_* prefixed)
proviz-server --storage postgres --database-url "postgresql://user:pass@host/db"

# Via env vars
PROVIZ_STORAGE=postgres PROVIZ_DATABASE_URL=postgresql://... proviz-server
PROVIZ_PORT=63130 proviz-server  # force port
```

In all cases, the server prints `PROVIZ_PORT=<n>` to stdout immediately after binding.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `PROVIZ_STORAGE` | `sqlite` | `sqlite` or `postgres` |
| `PROVIZ_DB_PATH` | `./proviz.db` | SQLite file path |
| `PROVIZ_DATABASE_URL` | — | PostgreSQL connection URL |
| `PROVIZ_PORT` | `0` (OS-assigned) | HTTP port; set to e.g. `63130` to force |
| `RUST_LOG` | — | Log level filter |

## Docker

### Pull from Docker Hub

```bash
docker pull justgu1/proviz-elekto:latest
```

### Run with PostgreSQL

```bash
docker run -d \
  --name proviz \
  -p 63130:63130 \
  -e PROVIZ_STORAGE=postgres \
  -e PROVIZ_DATABASE_URL="postgresql://user:pass@host/db" \
  justgu1/proviz-elekto:latest
```

### Docker Compose (recommended)

```yaml
services:
  proviz:
    image: justgu1/proviz-elekto:latest
    ports:
      - "63130:63130"
    environment:
      PROVIZ_STORAGE: postgres
      PROVIZ_DATABASE_URL: postgresql://user:pass@db/mydb
      PROVIZ_PORT: 63130
    depends_on:
      - db

  db:
    image: postgres:16-alpine
    environment:
      POSTGRES_USER: user
      POSTGRES_PASSWORD: pass
      POSTGRES_DB: mydb
    volumes:
      - pgdata:/var/lib/postgresql/data

volumes:
  pgdata:
```

### Python client with Docker

Point the Python client at the running container using env vars or constructor args:

```python
import os
from proviz_elekto import ProvizElekto

# Via env vars (no code change needed)
# PROVIZ_HOST=proviz PROVIZ_PORT=63130

# Or via constructor
pz = ProvizElekto(host="proviz", port=63130)
```

`PROVIZ_HOST` and `PROVIZ_PORT` env vars are read automatically; a non-localhost host with a
non-zero port skips subprocess spawning and attaches directly to the running container.

### Build the image locally

```bash
docker build -t proviz-elekto .
docker run -p 63130:63130 -e PROVIZ_DATABASE_URL=postgresql://... proviz-elekto
```

## Building from Source

```bash
git clone https://github.com/JustGui/proviz-elekto
cd proviz-elekto

# Build server + CLI
cargo build --release

# Run server
./target/release/proviz-server --storage sqlite --db-path ./dev.db

# Run CLI
./target/release/proviz --help

# Build Python wheel (requires maturin)
pip install maturin
cd python && maturin build --release
```
