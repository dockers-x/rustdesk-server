# Rendezvous KX v1 compatibility investigation

**Status**　Source comparison complete. Adoption is pending released-client and
deployment acceptance tests. The server continues using its existing handshake.

## Source comparison

| Source | Native TCP rendezvous | WebSocket rendezvous |
|---|---|---|
| Current server baseline `b7149ea` | Signs the 32-byte X25519 public key and accepts the legacy two-part client reply. The pinned `hbb_common` at `69cea8d` has the legacy `KeyExchange` schema. | Uses WebSocket framing and skips the native rendezvous key exchange. |
| SCTGDesk `7c265b1` | Uses `hbb_common` at `edefa08`. Advertises KX v1 through a signed marker and signed parameters. The v1 key derivation binds the negotiation transcript and uses separate directions. Its default accepts a legacy v0 reply. | Its native KX v1 implementation runs only on its TCP path. |
| Official RustDesk `1.5.1` (`23e5bb4`) and `1.5.3` (`95cca3f`) | Both pin `hbb_common` at `fae47e0`. Their `key_exchange` checks the signed server key and selects the advertised supported version. With a v1 marker it requires signed parameters. A valid legacy offer selects v0. | Both skip native KX in `secure_tcp_impl` and `secure_tcp_required` when using WebSocket. WSS uses TLS for this transport. |
| Deployed self-signed clients | Exact versions and artifacts still need to be recorded and exercised. | Confirm their WSS proxy path and end-to-end relay separately. |

Relevant source entries are `src/common.rs::key_exchange` in the two official
client tags and `src/rendezvous_server.rs::key_exchange_phase1` in SCTGDesk.
SCTGDesk's `tests/hbbs_e2e.rs` covers the rendezvous exchange through a test
client. It provides a starting point for a wire-level regression test.

## Acceptance before enabling negotiation

| Scenario | Required observation on actual client binaries |
|---|---|
| Official 1.5.1 and 1.5.3, and each deployed self-signed build, against the current HBBS over TCP | Registration and connection succeed against the legacy server and each client verifies the configured server identity. |
| Those clients against a candidate v1 HBBS over TCP | v1 clients use the authenticated negotiated version. Legacy clients finish a v0 handshake. Both complete registration and an authenticated connection. A bad server identity is rejected. |
| Independent upgrade and rollback of HBBS and HBBR | Force a TCP relay connection and complete a transfer on both versions, including while HBBS and HBBR versions differ. |
| Existing WSS deployment through its reverse proxy | Verify registration, connection, and a relayed transfer through both the current and candidate servers. Check the deployed Web Client build and TLS termination. |

The HBBR relay leg and the HBBS rendezvous exchange serve different parts of the
connection. A successful TCP exchange on HBBS alone does not establish WSS or
relayed-session compatibility. SCTGDesk's Web Client is based on upstream Web
Client v1. Official Web Client v2 licensing remains outside this investigation.
