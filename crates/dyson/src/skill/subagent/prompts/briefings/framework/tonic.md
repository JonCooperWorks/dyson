Starting points for tonic (Rust gRPC) — not exhaustive. Wire format is protobuf, which rules out many parser-level attacks but opens different ones. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)

Each `Request<T>` passed to a service method carries `T` = the attacker-controlled protobuf message.  Metadata (`request.metadata()`) carries headers including auth tokens.  Protobuf decoding enforces the schema; attacker can't send arbitrary shapes, but EVERY field value is attacker-chosen.

## Sinks

**Auth via metadata**
- `request.metadata().get("authorization")` — read the auth token from metadata.  Services relying on TCP-level mTLS alone skip per-call authorization; add an interceptor that validates tokens per RPC.
- `InterceptorFn` returning `request` without populating an authenticated principal — downstream handlers get `None` for the user and some paths assume-grant.

**SQL in handlers**
- Tonic handlers are normal async Rust; `sqlx::query(&format!("... {}", req.field)).execute(&pool)` is SQLi.

**Streaming**
- `tonic::Streaming<T>` server-side: `stream.message().await?` returns messages one at a time.  A handler that doesn't cap the stream length is DoS-prone (out of scope) but also: a handler that COMMITS each message individually without atomic auth checks can be partially-applied if auth expires mid-stream.
- Server-streaming / bidi: the client can hold the stream open; per-message auth is stronger than connection-level auth for long-lived streams.

**Decoded size / recursion limits**
- `Server::builder().max_decoding_message_size(...)` — default per-message cap is 4 MiB.  Raising without a cap is DoS-prone.
- Nested messages recursion: `Server::builder().max_frame_size(...)` applies to HTTP/2 frames.  Protobuf nesting depth is handled by the `prost` decoder which has its own limits.

**Reflection / health service**
- `tonic_reflection::server::Builder` — exposes the full service schema.  Like GraphQL introspection: for a private gRPC service the schema IS sensitive.  Disable in production or gate behind auth.
- `tonic_health` service — safe to expose but check it doesn't leak per-service healthcheck details that reveal internal topology.

**TLS config**
- `ServerTlsConfig::new().identity(...)` — must be set for production; `Server::builder()` without TLS is cleartext gRPC.  Often wrapped by a reverse proxy; confirm the reverse proxy terminates TLS before flagging absence.
- mTLS: `ServerTlsConfig::new().client_ca_root(ca)` — missing CA = no client-cert verification; claims of mTLS don't check.

**Error leakage**
- `Status::internal(format!("{:?}", err))` — Debug-formatting internal errors can include config / secret / path information.  Use `Status::internal("internal error")` and log the detail server-side.

**Interceptor ordering**
- `ServiceBuilder::new().layer(auth).layer(logging).service(svc)` — logging runs AFTER auth.  If a logging layer READs the body (unusual but happens for request recording), it consumes the body before the service sees it.

## Tree-sitter seeds (rust, tonic-focused)

```scheme
; #[tonic::async_trait] impl — service method definitions
(attribute_item (attribute (scoped_identifier
  path: (identifier) @ns
  name: (identifier) @n)
  (#eq? @ns "tonic")
  (#match? @n "^(async_trait|include_proto)$")))

; Status::<ctor>(...)
(call_expression function: (scoped_identifier
    path: (identifier) @ty
    name: (identifier) @fn)
  (#eq? @ty "Status")
  (#match? @fn "^(internal|unauthenticated|permission_denied|invalid_argument|not_found|cancelled|ok)$"))
```
