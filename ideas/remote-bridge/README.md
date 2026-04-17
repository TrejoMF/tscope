# Remote Bridge

A real-time layer that streams per-pane telemetry out of TScope and lets
you send input back into any pane from a web client — effectively a
"gateway" for chatting with your instances (Claude Code sessions, SSH
shells, services, Docker commands) from a browser or phone.

## Why this is feasible

TScope's current data model is already 90% of the way there:

- Every pane holds structured context in memory: `ClaudeContext`,
  `SshContext`, `ServiceInfo`, `DockerContext`, `typing_buffer`,
  `last_typed`. That is the telemetry payload, for free.
- Every pane owns a `writer` into its PTY. "Chatting with the instance"
  is literally the same as typing into that writer, so the inbound side
  needs no new process plumbing.

## Proposed shape (~1 new module)

A `bridge.rs` with one tokio task and two channels wired into the main
event loop in `app.rs`:

- **Outbound (stats → web)** — on each tick (or throttled to 2–4 Hz),
  serialize `[{pane_id, ctx, last_typed, ... }, ...]` and push it over a
  WebSocket to connected clients.
- **Inbound (web → TScope)** — commands like
  `{pane_id, type: "input", text: "..."}` arrive on an `mpsc`, the main
  `select!` drains the channel, and the handler calls
  `pane.writer.write_all(text)`. Same mechanism handles `new_pane`,
  `focus_pane`, `send_signal`, etc.

### Transport choices

- **LAN-only** — `axum` + `ws://` bound to `127.0.0.1`, tunneled via
  Tailscale or cloudflared when remote. Simplest. No auth story on the
  listener itself because only the tunnel terminates remote traffic.
- **Outbound relay** — TScope dials *out* to a tiny cloud WebSocket hub;
  your browser or phone dials in separately; the hub forwards. Best for
  NAT/firewalls — no port forwarding required on the developer machine.

### "Chat with my Claude Code session" specifically

Two flavors, both work, and they are additive:

1. **Typing proxy** — send text to the pane's PTY exactly like a user
   would. Zero Claude-specific integration. Works for any tool in the
   pane (SSH, bash, vim, etc.).
2. **Claude-aware** — since TScope already tails the JSONL transcript,
   it knows message boundaries. A proper chat UI on the web side can
   show the full conversation, send new user turns by writing to the
   PTY, and stream assistant tokens as they are parsed out of the
   JSONL.

## The real challenge: security

This is a remote code execution endpoint into your laptop. Treat it
that way:

- **Mandatory auth** — bearer token or mTLS on every connection. No
  "anonymous local" mode.
- **Bind to loopback** by default, never `0.0.0.0`. Remote access comes
  from an explicit tunnel (Tailscale, cloudflared, ssh -L).
- **Rate-limit inbound commands** — defend against credential leak
  replay.
- **Audit log** — every inbound command writes a line to disk with
  pane_id, timestamp, source IP, payload hash. Cheap; invaluable when
  something goes wrong.
- If the outbound-relay variant is chosen, the relay itself must not be
  able to forge commands — clients sign messages with a key TScope
  knows, and the relay is a dumb forwarder.

## Engineering tradeoffs

- **Snapshot vs. diff stream** — start with full snapshots at 2–4 Hz;
  move to a diff protocol only if bandwidth or CPU becomes a problem.
  Not worth the complexity up front.
- **Addressing panes** — clients need stable IDs. The existing pane
  index is position-dependent (shifts when panes close). Introduce a
  monotonic `pane_uid` on creation and use that on the wire.
- **State reconciliation** — when a new client connects mid-session, it
  needs a full snapshot before diffs resume. A `hello` handshake that
  returns the current state of all panes covers this.
- **Back-pressure** — if a client is slow, drop old snapshots rather
  than queueing; always send the latest state, never a stale backlog.

## Suggested first cut

Resist the urge to ship the full design in one go. Minimum viable bridge:

1. New `bridge.rs` module that opens an `axum` WebSocket server on
   `127.0.0.1:<port>`.
2. Pane snapshot serialized to JSON, pushed on each tick while any
   client is connected.
3. One inbound command type: `{pane_uid, type: "input", text}`.
4. Static bearer token read from `~/.config/tscope/config.toml`.
5. A tiny HTML test page that connects, renders the snapshot, and has a
   text box wired to `input`.

~200 lines, one new module, no changes to the existing pane model
beyond adding `pane_uid`. Prove the loop works end-to-end, *then* layer
on the relay variant, the Claude-aware chat UI, audit logs, and a real
frontend.

## Open questions

- Should the bridge be opt-in via CLI flag (`tscope --bridge`) or always
  on with auth? Opt-in is safer; always-on is more useful. Probably
  opt-in for v1.
- Multi-client semantics: if two web clients are connected, does an
  `input` from client A show up in client B's stream? (Yes, via the
  pane snapshot — but we may want to echo it faster.)
- Do we expose a "scrollback read" endpoint, or is the live snapshot
  enough? Scrollback read is a meaningful surface-area increase
  (history size, pagination, etc.) and probably belongs in v2.
- How do we handle pane input contention — a human typing locally while
  a remote client sends input? Interleave naively for v1; revisit if it
  causes real problems.
