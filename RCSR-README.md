# rcrsr-codanna — Fork Changes

This is a fork of [`bartolli/codanna`](https://github.com/bartolli/codanna). This
file lists what the fork adds or changes for you as a user, on top of its
upstream base. For the how, see the commit history.

- **Upstream base:** the latest `codanna` release the fork is built on
- **Fork build:** the upstream version with a `+rcrsr.N` suffix (see [Identifying the fork](#identifying-the-fork))

## Proxy mode: one backing server per workspace

`codanna serve --proxy` lets several MCP clients share a single backing server
for a workspace instead of each starting its own. Point every client at the
proxy: the first one starts (or discovers) a backing server for the workspace,
and the rest attach to it.

The backing server is started as a detached background process and keeps running
after the clients disconnect, so the next client reattaches to the warm index
instead of paying startup again. It is not shut down automatically — stop it
yourself when you're done with the workspace (or leave it running).

### Configuration

The workspace must be initialized (`codanna init` writes `.codanna/settings.toml`);
the proxy refuses to auto-spawn a backing server for a tree that has no config.
Proxy behavior is controlled by the `[server]` section of that file:

```toml
[server]
auto_spawn = true       # let the proxy start a backing server when none is found;
                        # set false to require starting `codanna serve --http --watch` yourself
spawn_timeout_ms = 8000 # how long to wait for a spawned server to become ready
health_poll_ms = 100    # how often to poll for readiness while waiting
```

The defaults shown above apply when the keys are absent, so an initialized
workspace works with no `[server]` block at all.

### Ports

When the proxy auto-spawns a backing server, it binds a random free port on
`127.0.0.1` (the OS assigns it). You never choose or need that port: the server
records it, and the proxy reads the record to connect. Your MCP clients only ever
talk to the proxy over stdio, so nothing on your side depends on the number.

If you start a backing server yourself instead (`codanna serve --http` /
`--https`), it uses the normal bind address — `--bind`, or `[server] bind` in
`settings.toml`, defaulting to `127.0.0.1:8080` for HTTP and `127.0.0.1:8443` for
HTTPS. All backing servers listen on loopback only; nothing is exposed off-host.

That port serves the MCP protocol (plus a `/health` check) — it is not a browser
dashboard or web UI. There is no separate web/UI port.

```bash
codanna serve --proxy
```

Use it when more than one tool or editor talks to codanna for the same project
and you don't want a separate index loaded into memory for each. Both HTTP and
HTTPS backing servers are supported; with `--https` the connection is verified
against codanna's own certificate.

If you only ever run a single client, you don't need this — plain `codanna serve`
is unchanged.

## Identifying the fork

Fork builds carry a `+rcrsr.N` suffix on the upstream version, so you can tell a
fork build from an upstream one:

```bash
codanna --version        # e.g. codanna <upstream-version>+rcrsr.N
```

MCP clients see the same string in the `initialize` handshake, so a connected
client can confirm which build it is talking to. The `+rcrsr.N` suffix is build
metadata — it does not change how the version compares, so a fork build counts as
the same release as the upstream version it is built on. `N` is just a running
count of fork additions on the current upstream base.

Everything not listed here behaves as it does in upstream codanna.
