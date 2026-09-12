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
│   ├── auth.rs              # Argon2id e geração de tokens CSPRNG 256-bit
│   ├── audit.rs             # Auditoria estruturada e sanitização de segredos
│   ├── manifest.rs          # Validação e esquemas de manifestos de sala
│   ├── rate_limit.rs        # Rate limiting por IP/sala contra brute-force
│   ├── storage.rs           # Armazenamento CAS com HMAC-SHA256 e cotas
│   ├── state.rs             # AppState compartilhado (pool, canais WS, CAS)
│   └── routes/              # Endpoints HTTP e WebSocket
│       ├── health.rs        # /health e /ready
│       ├── version.rs       # /v1/version
│       ├── rooms.rs         # Ciclo de vida de salas, manifestos, acks e auditoria
│       ├── blobs.rs         # Uploads/Downloads CAS com controle de acesso Zero-Trust
│       ├── ws.rs            # /v1/rooms/:room_id/ws (presença em tempo real)
│       └── mod.rs
├── migrations/              # Scripts SQL gerenciados pelo SQLx
│   ├── 20260910000001_initial_schema.sql
│   ├── 20260910000002_auth_and_roles.sql
│   ├── 20260910000003_revisions_and_presence.sql
│   ├── 20260910000004_cas_storage.sql
│   └── 20260910000005_audit_logs.sql
└── data/                    # Volume de dados persistente (ignorado pelo git)
    ├── postgres/            # Diretório de dados do PostgreSQL 16
    └── blobs/               # Objetos imutáveis e uploads parciais (CAS)
```

---

## 3. Serviços Docker em Execução

| Container         | Imagem                   | Porta Interna | Porta Exposta    | Finalidade                                 |
| ----------------- | ------------------------ | ------------- | ---------------- | ------------------------------------------ |
| `ltk-postgres`    | `postgres:16-alpine`     | `5432`        | `127.0.0.1:5432` | Banco relacional com migrações automáticas |
| `ltk-room-server` | `ltk-room-server:latest` | `3000`        | `0.0.0.0:3000`   | Serviço Axum HTTP + WebSocket              |

> [!NOTE]
> O servidor já possui um Nginx ativo nas portas 80 e 443 atendendo o site `mag.horuzprod.com`. O `ltk-room-server` roda na porta `3000`, mantendo total isolamento sem interferir nos sites existentes.

O endpoint público oficial é `https://mag.horuzprod.com/ltk-rooms`. O Nginx remove esse prefixo
antes de encaminhar para a porta 3000 e mantém suporte a WebSocket e transferências longas. A porta
3000 continua útil apenas para diagnóstico administrativo direto.

---

## 4. Comandos de Operação e Manutenção

Para executar qualquer comando no servidor, conecte-se via SSH e vá até o diretório:

```bash
cd /opt/ltk-room-server
```

O arquivo `.env` deve permanecer com permissão `0600` e conter `POSTGRES_PASSWORD` e
`STORAGE_SECRET`. O Compose recusa iniciar o backend sem uma `STORAGE_SECRET` de pelo menos 32
caracteres; nunca registre seu valor em logs ou no repositório.

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

### Publicar uma atualização assinada do aplicativo

O backend hospeda atualizações no volume somente-leitura `/data/updates`. A chave privada do Tauri
nunca deve ir para a VPS: o artefato é assinado na máquina de build e enviado de forma atômica pelo
script `scripts/publish-room-update.ps1`.

```powershell
$env:TAURI_SIGNING_PRIVATE_KEY_PATH = "$env:USERPROFILE\.tauri\ltk-manager-party.key"
pnpm tauri build --bundles nsis
./scripts/publish-room-update.ps1 `
  -Version 1.20.0 `
  -ArtifactPath ./target/release/bundle/nsis/LTK-Manager_1.20.0_x64-setup.exe `
  -Notes 'Resumo das alterações'
```

O script exige o `.sig` criado pelo Tauri ao lado do instalador, envia primeiro o artefato imutável
e substitui `latest.json` por último. Clientes consultam automaticamente
`/v1/updates/{target}/{arch}/{current_version}` no início, ao voltar ao aplicativo e a cada hora.

### Acessar o Banco de Dados PostgreSQL (CLI `psql`)

```bash
docker compose exec postgres psql -U ltk -d ltk_rooms
```

---

## 5. Endpoints da API e Verificação

> [!WARNING]
> O endpoint IP em HTTP é somente para validação privada. Senhas e tokens não têm
> confidencialidade de transporte até que um hostname com TLS confiável seja configurado.

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
  -d '{"room_id":"minha-sala","password":"senha-secreta","game_build":"14.1.1","display_name":"DESKTOP-ANA"}'
# Resposta (201 Created):
# {"room_id":"minha-sala","member_id":"owner-...","owner_token":"<hash-owner>","member_token":"<hash-member>","role":"owner"}
```

### Entrar na Sala (Member com Senha)

```bash
curl -X POST http://177.153.59.168:3000/v1/rooms/minha-sala/join \
  -H "Content-Type: application/json" \
  -d '{"password":"senha-secreta","display_name":"DESKTOP-BRUNO"}'
# Resposta (200 OK):
# {"room_id":"minha-sala","member_id":"member-...","member_token":"<hash>","role":"member","revision":0}
```

### Publicar Manifesto com CAS (Compare-and-Swap - Qualquer Membro Autenticado)

```bash
curl -X POST http://177.153.59.168:3000/v1/rooms/minha-sala/manifest \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <member_token>" \
  -d '{
    "previous_revision": 0,
    "manifest": {
      "schemaVersion": 2,
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
          "enabled": true,
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
#   {"member_id":"owner-8e7f8484","display_name":"DESKTOP-ANA","role":"owner","last_acknowledged_revision":1,"ack_status":"synchronized","joined_at":"...","last_seen_at":"...","is_online":true,"is_stale":false},
#   {"member_id":"member-...","display_name":"DESKTOP-BRUNO","role":"member","last_acknowledged_revision":1,"ack_status":"synchronized","joined_at":"...","last_seen_at":"...","is_online":true,"is_stale":false}
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

### Verificar Hashes Ausentes (Upload Only Missing Hashes)

```bash
curl -X POST http://177.153.59.168:3000/v1/rooms/minha-sala/blobs/check \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <token>" \
  -d '{"hashes":["74237cf6d69c51d7ecfd807361e9bf04e489a2a7b850b0c532b2650f3372132a","aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}'
# Resposta (200 OK):
# {"existing_hashes":["74237cf..."],"missing_hashes":["aaaaaa..."]}
```

### Requisitar URL Assinada de Upload (Membro Autenticado)

```bash
curl -X POST http://177.153.59.168:3000/v1/rooms/minha-sala/blobs/upload_url \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <member_token>" \
  -d '{"content_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size_bytes":2048,"format":"modpkg"}'
# Resposta (200 OK):
# {"content_hash":"aaaa...","upload_url":"http://177.153.59.168:3000/v1/blobs/upload/aaaa...?grant=<hmac>&expires=<ts>","expires_at":1757548800}
```

### Protocolo de Upload Resumável (transfer.rs)

1. **Probe com `HEAD`**:

   ```bash
   curl -I "http://177.153.59.168:3000/v1/blobs/upload/<hash>?grant=<grant>&expires=<ts>"
   # Cabeçalhos retornados:
   # Upload-Offset: 0 (ou N bytes gravados até o momento)
   # ETag: "<hash>"
   ```

2. **Upload com `PUT`**:
   ```bash
   curl -X PUT "http://177.153.59.168:3000/v1/blobs/upload/<hash>?grant=<grant>&expires=<ts>" \
     -H "Content-Type: application/octet-stream" \
     -H "Content-Range: bytes 0-2047/2048" \
     --data-binary @arquivo.modpkg
   # Resposta ao concluir (200 OK):
   # X-Content-SHA256: <hash>
   # ETag: "<hash>"
   ```

### Requisitar URL Assinada de Download (Membro)

```bash
curl http://177.153.59.168:3000/v1/rooms/minha-sala/blobs/<hash>/download_url \
  -H "Authorization: Bearer <member_token>"
# Resposta (200 OK):
# {"content_hash":"<hash>","download_url":"http://177.153.59.168:3000/v1/blobs/download/<hash>?grant=<hmac>&expires=<ts>","expires_at":1757552400}
```

### Download com Suporte a Range (HTTP 206 Partial Content)

```bash
# Download completo
curl "http://177.153.59.168:3000/v1/blobs/download/<hash>?grant=<grant>&expires=<ts>" -O

# Download retomado ou parcial com Range
curl -H "Range: bytes=1024-" "http://177.153.59.168:3000/v1/blobs/download/<hash>?grant=<grant>&expires=<ts>" -O
# Resposta: 206 Partial Content com cabeçalho Content-Range: bytes 1024-2047/2048
```

### Auditoria e Histórico de Ações da Sala (Zero-Trust & Privacidade)

```bash
curl http://177.153.59.168:3000/v1/rooms/<room_id>/audit \
  -H "Authorization: Bearer <token>"
```

- Retorna eventos de segurança auditados (`room_created`, `member_joined`, `manifest_published`, `revision_acknowledged`, `owner_transferred`, `upload_url_requested`, `download_url_requested`, `blob_uploaded`).
- **Garantia de Privacidade**: Senhas, tokens de autenticação (Bearer tokens), grants HMAC e caminhos absolutos do sistema de arquivos são estritamente sanitizados/redigidos (`[REDACTED]`, `[REDACTED_PATH]`).
- **Controle de Acesso Zero-Trust**: O download de blobs é estritamente limitado aos membros de salas cujo manifesto ativo referencia o hash do blob (`403 BLOB_NOT_IN_ROOM`). Grants de download e upload são assinados com HMAC-SHA256 e vinculados ao ID específico da sala (`{op}:{room_id}:{hash}:{expires}`), impedindo reutilização cross-room.

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
  - `storage_path TEXT NULL` (Caminho no volume persistente do CAS ou chave S3)
  - `uploaded_by_room_id VARCHAR(64) NULL REFERENCES rooms(room_id)`
  - `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`
  - `last_accessed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`
- **`room_audit_logs`**:
  - `id BIGSERIAL PRIMARY KEY`
  - `room_id VARCHAR(64) REFERENCES rooms(room_id) ON DELETE CASCADE`
  - `actor_member_id VARCHAR(64) NOT NULL`
  - `actor_role VARCHAR(32) NOT NULL`
  - `action VARCHAR(64) NOT NULL`
  - `details JSONB NULL` (sanitizado, sem senhas/tokens/caminhos)
  - `client_ip VARCHAR(45) NULL`
  - `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`

---

## 7. Política de Expiração e Tarefas em Segundo Plano

- **Expiração de Salas**: Salas expiram após 24 horas de inatividade. Qualquer atividade (entrada de membro, publicação de manifesto, ack de revisão ou heartbeat) renova `expires_at = NOW() + INTERVAL '24 hours'`.
- **Limpeza Automática de Salas**: Um worker Tokio roda a cada 5 minutos no servidor (`DELETE FROM rooms WHERE expires_at < NOW()`), removendo salas expiradas e seus membros/manifestos associados em cascata.
- **Limpeza Automática de Blobs Órfãos**: Um worker Tokio roda a cada 1 hora no servidor, identificando blobs que não estão associados a nenhum manifesto ativo de sala há mais de 48 horas. Os arquivos em disco e os registros em `room_blobs` são removidos para liberar espaço em disco.
