# Current client routing capabilities

This server supports the existing RustDesk client messages without modifying or
building clients. Endpoint passwords, whitelists and session permissions remain
the destination client's responsibility.

## IPv6 coordination

hbbs forwards the client's IPv6 address through `PunchHole` and `FetchLocalAddr`,
and the destination's IPv6 address through `PunchHoleResponse`, including the
local-address response. It does not allocate IPv6 addresses or add a new IPv6
transport. Both clients must enable their existing IPv6 punch capability and
have working IPv6 connectivity; the existing TCP/relay fallback remains.

## Forced relay

The controller's `force_relay` flag is forwarded in `PunchHole`. Either that flag
or hbbs's `ALWAYS_USE_RELAY=Y` skips the local-address shortcut and marks the
request as relay-only, including for peers on the same intranet. Such requests
do not advertise the controller's IPv6 address.

While `ALWAYS_USE_RELAY` is enabled, hbbs also removes destination IPv6 addresses
from punch-hole, local-address and relay responses. This prevents a returned
IPv6 hint from reopening a direct connection when the controller does not know
the server's policy. The check uses the current server setting; it is not a new
per-session policy store. It does not terminate existing sessions or prevent
clients from making connections outside this server's coordination.

## Login and switch-side grants

With must-login enabled, ordinary and switch-side requests on both hbbs routing
entries require a valid account JWT or a destination-bound grant. Empty JWT
keys fail closed for account tokens; valid grants remain an independent
authorization mechanism. With must-login disabled, the legacy login-optional
paths remain. See [Switch-side routing grants](switch-grants.md) for the exact
signature, lifetime, key and deployment rules.

## Not implemented here

The shared protocol version pinned by this server does not expose the current
client's WebRTC SDP offer/answer fields. IPv6 hint forwarding is not WebRTC
signaling or TURN provisioning. This change does not implement complete WebRTC
coordination, UDP punch coordination, real-time account revocation, destination
ACL checks against Console, or a custom-client build service.

## S6 deployment and Console feature storage

The S6 image runs Console with `/app/conf/config.yaml`. Its optional feature
storage is therefore `/app/conf/features`, not `/app/data` or `/app/conf/data`.
The Compose example mounts it separately:

```yaml
volumes:
  - /data/rustdesk/api-features:/app/conf/features
```

Before starting Compose, create the private host directory explicitly. Do not
let Docker create it with permissive default permissions:

```sh
sudo install -d -m 700 /data/rustdesk/api-features
```

The current image runs as root. If you change its runtime user, assign this
directory and any restored files to that user's UID/GID. Keep the directory
at `0700` and the signing feature file at `0600`; do not loosen permissions
to work around a read error. Rootless Docker or user-namespace mappings also
require host ownership compatible with the container's mapped user.

### Migrating an existing enabled feature

Before recreating or removing the old container, stop the service and use
`docker cp` to copy its `/app/conf/features/self-signed-client.json` into a
protected backup location. A stopped container remains available to `docker cp`;
record its name or ID before replacing it. The file contains the private key,
saved profile and enabled flag, so never print it or put it in a public directory.

Create the host directory above, then restore that backup as
`/data/rustdesk/api-features/self-signed-client.json` with the correct owner and
`0600` mode before adding the new bind mount and starting the replacement. If a
host feature file already exists, securely back it up and compare the intended
public key/fingerprint before replacing anything. Verify the public key and
fingerprint in Console after startup. Retain an encrypted offline backup.

Adding an empty bind mount immediately hides the old container's feature file;
it does not copy or migrate it. Removing the old container first can lose its
unpersisted key. A public key or signed `custom.txt` cannot recover a private key.

### Optional Console v0.2.38 key backups and client assets

Self-signed client configuration remains disabled by default. In Console
v0.2.38, generating a key pair saves it on the server first and returns the new
private key only in that administrator generation response, with
`Cache-Control: no-store`. The UI immediately starts downloading
`self-signed-client-keypair.json`, containing `algorithm: "Ed25519"`,
`public_key`, `private_key` and `fingerprint`. Subsequent settings reads and
key imports do not return it; generating again replaces the key rather than
downloading the current one.

This download is an unencrypted private-key backup. Confirm it downloaded,
restrict local access and move it into encrypted offline storage; HTTP
`no-store` is not file encryption. To restore it, import its Base64 `private_key`
value and optional matching `public_key`, not the entire JSON document. It
does not contain the saved profile or feature flag; retain the full server-side
feature backup as well.

Configuration-signing keys are independent of hbbs/hbbr identity keys, Console
JWT secrets and OS application-signing certificates. Never put a private-key
backup in a client bundle. Official clients do not trust these self-owned keys:
users must replace the verification public key in their own client source and
build it themselves. Signed `custom.txt` supplies runtime settings, not logos
or platform icons; those remain compile-time assets in the user's fork. This
optional Console feature does not build clients or change normal server routing.
