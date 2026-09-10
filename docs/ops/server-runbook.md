# Guia Operacional da VPS & Servidor de Salas (Server Runbook)

Este documento serve como referência técnica completa para qualquer agente de IA ou desenvolvedor operar, manter, debugar e evoluir o serviço de sincronização de salas (**Room Synchronization Service**).

---

## 1. Dados e Acesso da VPS

- **Endereço IP:** `177.153.59.168`
- **Hostname:** `horuz.vps-kinghost.net`
- **Sistema Operacional:** Ubuntu 24.04.4 LTS (Noble Numbat), Kernel Linux 6.8 x86_64
- **Recursos de Hardware:** 2 vCPU Cores, ~4 GB RAM, 20 GB Swap, ~34 GB de disco livre
- **Usuário SSH:** `root`
- **Porta SSH:** `22`

### Como Acessar via Terminal

O servidor já está configurado com a chave pública SSH local do desenvolvedor (`~/.ssh/id_ed25519.pub`).
Para conectar diretamente sem necessidade de senha interativa:

```bash
# No Windows (PowerShell) ou Linux/macOS
ssh -i ~/.ssh/id_ed25519 root@177.153.59.168
```

---

## 2. Estrutura de Diretórios na VPS

O serviço foi instalado em `/opt/ltk-room-server`:

```text
/opt/ltk-room-server/
├── docker-compose.yml       # Orquestração dos containers (PostgreSQL + Rust Axum)
├── Dockerfile               # Build multi-stage da imagem do ltk-room-server
├── Cargo.toml               # Manifesto Rust do servidor
├── Cargo.lock               # Lockfile de dependências
├── .env                     # Variáveis de ambiente da instância
├── src/                     # Código fonte Rust (Axum, Tokio, SQLx, Tower)
│   ├── main.rs              # Ponto de entrada, graceful shutdown e migrações
│   ├── config.rs            # Configurações de porta e database
│   ├── error.rs             # Mapeamento de erros estruturados HTTP/JSON
│   ├── state.rs             # AppState compartilhado (pool de conexões, broadcasters WS)
│   └── routes/              # Endpoints HTTP e WebSocket
│       ├── health.rs        # /health e /ready
│       ├── version.rs       # /v1/version
│       ├── ws.rs            # /v1/rooms/:room_id/ws
│       └── mod.rs
├── migrations/              # Scripts SQL gerenciados pelo SQLx
│   └── 20260910000001_initial_schema.sql
└── data/                    # Volume de dados persistente (ignorado pelo git)
    └── postgres/            # Diretório de dados do PostgreSQL 16
```

---

## 3. Serviços Docker em Execução

| Container         | Imagem                   | Porta Interna | Porta Exposta    | Finalidade                                 |
| ----------------- | ------------------------ | ------------- | ---------------- | ------------------------------------------ |
| `ltk-postgres`    | `postgres:16-alpine`     | `5432`        | `127.0.0.1:5432` | Banco relacional com migrações automáticas |
| `ltk-room-server` | `ltk-room-server:latest` | `3000`        | `0.0.0.0:3000`   | Serviço Axum HTTP + WebSocket              |

> [!NOTE]
> O servidor já possui um Nginx ativo nas portas 80 e 443 atendendo o site `mag.horuzprod.com`. O `ltk-room-server` roda na porta `3000`, mantendo total isolamento sem interferir nos sites existentes.

---

## 4. Comandos de Operação e Manutenção

Para executar qualquer comando no servidor, conecte-se via SSH e vá até o diretório:

```bash
cd /opt/ltk-room-server
```

### Ver Status dos Containers

```bash
docker compose ps
```

### Ver Logs em Tempo Real

```bash
# Logs do servidor Axum
docker compose logs -f room-server

# Logs do PostgreSQL
docker compose logs -f postgres
```

### Reiniciar os Serviços

```bash
docker compose restart
```

### Reconstruir e Atualizar após Modificações de Código

```bash
docker compose up -d --build
```

### Acessar o Banco de Dados PostgreSQL (CLI `psql`)

```bash
docker compose exec postgres psql -U ltk -d ltk_rooms
```

---

## 5. Endpoints da API e Verificação

### Health Check (Liveness)

Indica se o processo HTTP está respondendo:

```bash
curl http://177.153.59.168:3000/health
# Resposta esperada: {"status":"ok","version":"0.1.0"}
```

### Readiness Check

Indica se o servidor conseguiu conectar ao PostgreSQL:

```bash
curl http://177.153.59.168:3000/ready
# Resposta esperada: {"status":"ready","database":"connected"}
```

### Informações de Versão

```bash
curl http://177.153.59.168:3000/v1/version
# Resposta esperada: {"name":"ltk-room-server","version":"0.1.0","protocol_version":1}
```

### WebSocket de Presença e Sincronização

- Rota: `ws://177.153.59.168:3000/v1/rooms/<room_id>/ws`
- Trata eventos em tempo real, broadcast para todos os membros conectados à mesma sala e suporte a ping/pong.

---

## 6. Esquema do Banco de Dados (SQLx)

As migrações em `migrations/` são executadas automaticamente na inicialização do container pelo comando `sqlx::migrate!("./migrations").run(&pool)`.

- **`rooms`**:
  - `room_id VARCHAR(64) PRIMARY KEY`
  - `revision BIGINT NOT NULL DEFAULT 0`
  - `password_hash VARCHAR(255) NULL` (Argon2id nas etapas 13+)
  - `owner_token VARCHAR(255) NULL`
  - `created_at`, `updated_at TIMESTAMPTZ`
- **`room_members`**:
  - `room_id VARCHAR(64) REFERENCES rooms(room_id)`
  - `member_id VARCHAR(64)`
  - `member_token VARCHAR(255) NULL`
  - `role VARCHAR(32)` ('owner' ou 'member')
  - `last_acknowledged_revision BIGINT`
  - `joined_at`, `last_seen_at TIMESTAMPTZ`
- **`room_manifests`**:
  - `room_id VARCHAR(64) REFERENCES rooms(room_id)`
  - `revision BIGINT`
  - `manifest_json JSONB NOT NULL` (conteúdo validado do manifesto)
  - `created_at TIMESTAMPTZ`
- **`room_blobs`**:
  - `content_hash VARCHAR(64) PRIMARY KEY` (SHA-256)
  - `size_bytes BIGINT`
  - `format VARCHAR(32)` (.fantome ou .modpkg)
  - `storage_path TEXT NULL` (preparado para a integração S3 da Etapa 15)
