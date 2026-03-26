# Ethera Sidecar Integration

This module contains the remaining HTTP integration between `op-rbuilder` and the Ethera sidecar.

The old pull-based `/transactions` flow has been removed. The builder now owns XT pending state locally and only calls
back into the sidecar to confirm which XT instance IDs were actually included on-chain.

## Current Architecture

```
┌─────────────────────┐         ┌─────────────────────┐
│   Ethera Sidecar    │         │     op-rbuilder     │
│ control plane       │──RPC───►│ XT pool + txpool    │
└──────────┬──────────┘         └──────────┬──────────┘
           │                               │
           │ POST /ethera/confirm          │
           └──────────────HTTP─────────────┘
```

### Responsibilities

- Sidecar:
  - accepts XT submissions
  - coordinates simulation, peer voting, and commit/abort
  - pushes XT lifecycle events into the builder via `ethera_submitXt`, `ethera_releaseXt`, and `ethera_abortXt`
  - signs local `putInbox` transactions and attaches them to `ethera_releaseXt`

- Builder:
  - stores XT reservations in the local `XtPool`
  - stores release-time `putInbox` transactions in the same XT instance bucket
  - merges XT reservations into `eth_getTransactionCount(..., "pending")`
  - blocks normal pool txs from stealing reserved XT nonces
  - executes released `putInbox` transactions before released XT transactions inside the flashblock payload path

- Sidecar callback client in this module:
  - confirms included XT instance IDs after the flashblock is built and published
  - retries failed confirmations by re-queueing IDs locally

## Callback API

### Endpoint

```text
POST {sidecar_endpoint}/ethera/confirm
```

### Request body

```json
{
  "instance_ids": [
    "xt-77777-1",
    "xt-88888-4"
  ]
}
```

The sidecar removes these XTs from its pending set once the builder confirms they were included.

## Configuration

```bash
op-rbuilder \
  --sidecar.endpoint http://localhost:8082 \
  --sidecar.poll-timeout-ms 200 \
  --sidecar.max-retries 5
```

The existing CLI names remain in place, but they now control the confirmation callback client:

| Option | Purpose |
|---|---|
| `sidecar.endpoint` | Base HTTP endpoint for the Ethera sidecar |
| `sidecar.poll-timeout-ms` | Per-request timeout for confirmation callbacks |
| `sidecar.max-retries` | Retry count for failed confirmation callbacks |

## Module Structure

```text
sidecar/
├── mod.rs
├── client.rs    # confirm-only callback client
├── config.rs    # callback configuration
├── overrides.rs # local tests for legacy override helpers
└── types.rs     # callback request/error types
```
