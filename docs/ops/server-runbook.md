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

### Criar Sala (Owner)

```bash
curl -X POST http://177.153.59.168:3000/v1/rooms \
  -H "Content-Type: application/json" \
  -d '{"room_id":"minha-sala","password":"senha-secreta","game_build":"14.1.1"}'
# Resposta (201 Created):
# {"room_id":"minha-sala","owner_token":"<hash-owner>","member_token":"<hash-member>","role":"owner"}
```

### Entrar na Sala (Member com Senha)

```bash
curl -X POST http://177.153.59.168:3000/v1/rooms/minha-sala/join \
  -H "Content-Type: application/json" \
  -d '{"password":"senha-secreta","member_id":"convidado-1"}'
# Resposta (200 OK):
# {"room_id":"minha-sala","member_id":"convidado-1","member_token":"<hash>","role":"member","revision":0}
```

### Publicar Manifesto com CAS (Compare-and-Swap - Somente Owner)

```bash
curl -X POST http://177.153.59.168:3000/v1/rooms/minha-sala/manifest \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <owner_token>" \
  -d '{
    "previous_revision": 0,
    "manifest": {
      "schemaVersion": 1,
      "roomId": "minha-sala",
      "revision": 1,
      "gameBuild": "14.1.1",
      "mods": [
        {
          "contentHash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "sizeBytes": 2048,
          "format": "modpkg",
          "displayName": "Lux Custom Skin",
          "version": "1.0.0",
          "suggestedLayers": ["Default"]
        }
      ]
    }
  }'
# Resposta (201 Created):
# {"room_id":"minha-sala","revision":1,"mod_count":1,"total_size_bytes":2048}
# Se previous_revision != revisão atual no banco -> Retorna 409 Conflict (REVISION_CONFLICT)
```

### Confirmar Sincronização (Member Ack)

```bash
curl -X POST http://177.153.59.168:3000/v1/rooms/minha-sala/ack \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <member_token>" \
  -d '{"revision":1,"status":"synchronized"}'
# Resposta (200 OK):
# {"success":true,"member_id":"convidado-1","revision":1,"status":"synchronized"}
```

### Consultar Membros e Presença

```bash
curl http://177.153.59.168:3000/v1/rooms/minha-sala/members \
  -H "Authorization: Bearer <token>"
# Resposta (200 OK):
# [
#   {"member_id":"owner-8e7f8484","role":"owner","last_acknowledged_revision":1,"ack_status":"synchronized","joined_at":"...","last_seen_at":"...","is_online":true,"is_stale":false},
#   {"member_id":"convidado-1","role":"member","last_acknowledged_revision":1,"ack_status":"synchronized","joined_at":"...","last_seen_at":"...","is_online":true,"is_stale":false}
# ]
```

### Transferir Propriedade da Sala (Somente Owner)

```bash
curl -X POST http://177.153.59.168:3000/v1/rooms/minha-sala/transfer_owner \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <owner_token>" \
  -d '{"new_owner_member_id":"convidado-1"}'
# Resposta (200 OK):
# {"success":true,"room_id":"minha-sala","previous_owner":"owner-...","new_owner":"convidado-1"}
```

### Consultar Manifesto Mais Recente

```bash
curl http://177.153.59.168:3000/v1/rooms/minha-sala/manifest \
  -H "Authorization: Bearer <token>"
```

### WebSocket de Presença e Sincronização

- Rota: `ws://177.153.59.168:3000/v1/rooms/<room_id>/ws?token=<token>` (ou via header `Authorization: Bearer <token>`)
- Eventos recebidos do servidor:
  - `member_presence`: `{"member_id":"...","role":"...","state":"joined"|"left"}`
  - `owner_disconnected`: `{"room_id":"...","member_id":"...","warning":"Room owner disconnected..."}`
  - `manifest_published`: `{"room_id":"...","revision":1,"mod_count":1,...}`
  - `member_acknowledged`: `{"room_id":"...","member_id":"...","revision":1,"status":"synchronized"}`
  - `owner_transferred`: `{"room_id":"...","previous_owner":"...","new_owner":"..."}`
- Ações enviadas pelo cliente:
  - `{"action":"ping"}` -> Responde com heartbeat e renova `last_seen_at`.
  - `{"action":"ack","payload":{"revision":1,"status":"synchronized"}}` -> Registra ack via WebSocket.

---

## 6. Esquema do Banco de Dados (SQLx)

As migrações em `migrations/` são executadas automaticamente na inicialização do container pelo comando `sqlx::migrate!("./migrations").run(&pool)`.

- **`rooms`**:
  - `room_id VARCHAR(64) PRIMARY KEY`
  - `revision BIGINT NOT NULL DEFAULT 0`
  - `password_hash VARCHAR(255) NULL` (Argon2id)
  - `owner_token VARCHAR(255) NULL`
  - `game_build VARCHAR(64) NULL`
  - `created_at`, `updated_at TIMESTAMPTZ`
  - `expires_at TIMESTAMPTZ NOT NULL DEFAULT (NOW() + INTERVAL '24 hours')`
- **`room_members`**:
  - `room_id VARCHAR(64) REFERENCES rooms(room_id) ON DELETE CASCADE`
  - `member_id VARCHAR(64)`
  - `member_token VARCHAR(255) NULL`
  - `role VARCHAR(32)` ('owner' ou 'member')
  - `last_acknowledged_revision BIGINT DEFAULT 0`
  - `ack_status VARCHAR(32) DEFAULT 'joined'`
  - `token_expires_at TIMESTAMPTZ NULL`
  - `joined_at`, `last_seen_at TIMESTAMPTZ`
- **`room_manifests`**:
  - `room_id VARCHAR(64) REFERENCES rooms(room_id) ON DELETE CASCADE`
  - `revision BIGINT`
  - `schema_version INT NOT NULL DEFAULT 1`
  - `game_build VARCHAR(64) NULL`
  - `manifest_json JSONB NOT NULL`
  - `created_at TIMESTAMPTZ`
- **`room_blobs`**:
  - `content_hash VARCHAR(64) PRIMARY KEY` (SHA-256)
  - `size_bytes BIGINT`
  - `format VARCHAR(32)` (.fantome ou .modpkg)
  - `storage_path TEXT NULL` (preparado para a integração S3 da Etapa 15)

---

## 7. Política de Expiração e Tarefas em Segundo Plano

- **Expiração de Salas**: Salas expiram após 24 horas de inatividade. Qualquer atividade (entrada de membro, publicação de manifesto, ack de revisão ou heartbeat) renova `expires_at = NOW() + INTERVAL '24 hours'`.
- **Limpeza Automática**: Um worker Tokio roda a cada 5 minutos no servidor (`DELETE FROM rooms WHERE expires_at < NOW()`), removendo salas expiradas e seus membros/manifestos associados em cascata.
