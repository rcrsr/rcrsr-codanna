# rcrsr-codanna — Fork Changes

This is a fork of [`bartolli/codanna`](https://github.com/bartolli/codanna). This
file lists what the fork adds or changes for you as a user, on top of its
upstream base. For the how, see the commit history.

- **Upstream base:** `codanna` v0.9.23
- **Fork build:** `0.9.23+rcrsr.1` (see [Identifying the fork](#identifying-the-fork))

## Proxy mode: one backing server per workspace

`codanna serve --proxy` lets several MCP clients share a single backing server
for a workspace instead of each starting its own. Point every client at the
proxy: the first one starts (or discovers) a backing server for the workspace,
and the rest attach to it. When you close them, the shared server goes with them.

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

Fork builds report a version of `0.9.23+rcrsr.N`, so you can tell a fork build
from an upstream one:

```bash
codanna --version        # -> codanna 0.9.23+rcrsr.1
```

MCP clients see the same string in the `initialize` handshake, so a connected
client can confirm which build it is talking to. The `+rcrsr.N` suffix is build
metadata — it does not change how the version compares, so `0.9.23+rcrsr.1`
counts as the same release as upstream `0.9.23`. `N` is just a running count of
fork additions on the current upstream base.

Everything not listed here behaves as it does in upstream codanna.
