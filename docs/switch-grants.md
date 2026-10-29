# Switch-side routing grants

The Console may forward `POST /api/switch-grant` to the authoritative local hbbs
control port (ID-server port minus one) as:

```
register-switch-grant <standard-padded-base64(compact-JSON)>
```

The complete command must fit in 1024 bytes. Its JSON contains string fields
`id`, `switch_code_verifier`, `timestamp`, and `signature`. hbbs verifies a
detached Ed25519 signature against its registered device key, never Console
sysinfo metadata or a public key supplied by the caller. Signed bytes are:

```
"switch-grant\0" || id || "\0" || switch_code_verifier || "\0" || timestamp
```

For the canonical textual switch UUID `U`, the client computes
`C = base64(SHA256(U))`, then
`switch_code_verifier = base64(SHA256("switch-grant-verifier\0" || C))`.
Hashes and signatures use standard padded base64; timestamps are canonical
decimal Unix seconds. All concatenated strings are UTF-8.

The control response is bare JSON with boolean `accepted`. Only a validly signed
request outside the +/-60-second clock window receives numeric `server_time`.
An unknown command, malformed or truncated response, unavailable hbbs, or missing
boolean must never become an accepted HTTP response in Console.

An accepted grant allows default remote-desktop rendezvous and relay requests
carrying `C` to the signed destination ID for ten seconds. It does not authorize
other session types or replace the destination's password/whitelist checks.
Punch-hole requests retain their existing server licence-key check. The stock
client's `RequestRelay` does not populate `licence_key`, and the existing relay
path does not require it. Explicit switch relay requests therefore preserve the
absent-key shape, reject a nonempty supplied key that mismatches a configured
server key, and always require the verified destination-bound desktop grant.
That grant is the new relay-routing authorization, backed by the destination's
registered signing key, not proof that the requester knows a server secret.
Requests without a switch code retain the existing behavior, including existing
relay authentication behavior.

Routing grants are reusable within their original ten seconds because the client
uses fresh sockets and relay UUIDs during fallback. The destination independently
checks and consumes the raw switch UUID once; hbbs cannot do that because it does
not receive the raw UUID. Duplicate registration never renews a grant. Expired
entries remain as replay tombstones for 130 seconds from registration, beyond the
signed timestamp window. Capacity is bounded to 4096 entries, at most eight per
device ID; live replay protection is never evicted to accept another grant.

## Deployment and limits

- Console and the destination's single authoritative hbbs must share the existing
  trusted loopback control channel. Do not expose that administrative channel to
  untrusted networks. Independent hbbs instances do not share grants.
- Registration inherits hbbs's existing registered-key/TOFU trust and key rotation
  rules; it does not add a new device enrollment authority.
- Use HTTPS for the HTTP request and the client's encrypted rendezvous transport
  to protect bearer switch codes. A verifier alone is not a usable switch code.
  `secure_tcp` authenticates hbbs to the client and establishes encryption; it
  does not authenticate the requesting device to hbbs.
- The client fires registration without waiting for acceptance, so a reverse
  connection can arrive before the grant; that request fails closed. Grant loss
  on restart likewise fails closed. No delayed-request machinery is added.
- Replay protection assumes the server clock does not roll backwards beyond the
  timestamp window after tombstones have expired.
