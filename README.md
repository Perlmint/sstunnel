# ssh-tunnel

A small cross-platform agent that keeps SSH tunnels open in the background.

- **One session per host.** Forwards are opened and closed against a live SSH
  session, so switching one on or off never reconnects the others.
- **Pure Rust SSH** ([russh]). No `ssh` binary, no OpenSSH install, no
  `ControlMaster` — Windows behaves exactly like macOS and Linux.
- **Reads your `~/.ssh/config`.** A host entry can be just an alias; `HostName`,
  `Port`, `User`, and `IdentityFile` fill themselves in.
- **Config-driven, hot-reloaded.** Save the file and the agent reconciles;
  a config that fails to parse is ignored and the running tunnels stay up.
- **Runtime control.** `enable`, `disable`, `toggle`, `restart`, `reload` over a
  local socket (unix domain socket, or a named pipe on Windows).
- **Local (`-L`), remote (`-R`), and dynamic (`-D`, SOCKS5) forwards.**

[russh]: https://github.com/Eugeny/russh

## Install

```sh
cargo install --path .
```

## Quick start

```sh
mkdir -p ~/.config/ssh-tunnel
cp example.toml ~/.config/ssh-tunnel/config.toml
$EDITOR ~/.config/ssh-tunnel/config.toml

ssh-tunnel check              # validate the config
ssh-tunnel daemon             # run in the foreground to try it out

# ...and in another terminal:
ssh-tunnel status
ssh-tunnel disable db
ssh-tunnel enable db
```

Once it behaves, hand it to the OS:

```sh
ssh-tunnel service install    # starts now, and at every login
ssh-tunnel service status
ssh-tunnel service uninstall
```

## Commands

| Command | What it does |
| --- | --- |
| `daemon` | Run the agent in the foreground. This is what the service manager starts. |
| `status [--json]` | Hosts, forwards, connection counts, and bytes moved. |
| `enable <NAME>… \| --all` | Turn forwards on. |
| `disable <NAME>… \| --all` | Turn forwards off. |
| `toggle <NAME>…` | Flip forwards. |
| `restart <NAME>…` | Tear a forward down and bring it back. |
| `reload` | Re-read the config file now. |
| `shutdown` | Stop the daemon. |
| `check` | Validate the config and exit non-zero if it is wrong. |
| `service install\|uninstall\|start\|stop\|status` | Register with the OS service manager. |
| `tray` | macOS menu bar / Windows tray icon with one submenu per host; check a forward to enable it. |

Runtime overrides beat the file: a forward you `disable` stays off across config
reloads and (with `persist_state`) across daemon restarts, until you enable it
again. Removing a forward from the config drops its override too.

## Configuration

See [`example.toml`](example.toml) for a fully commented file. The short version:

```toml
[hosts.prod]
hostname = "prod"              # the ~/.ssh/config alias, or a literal address

[[forwards]]
name = "db"
host = "prod"
type = "local"                 # local | remote | dynamic
listen = "127.0.0.1:5432"
target = "db.internal:5432"
```

Option precedence is **CLI flag → environment → `[daemon]` section → per-OS
default**. The environment variables are `SSH_TUNNEL_CONFIG`,
`SSH_TUNNEL_SOCKET`, and `SSH_TUNNEL_LOG`.

### `~/.ssh/config`

`hostname` is the only required field on a host. It is the alias you would type
after `ssh`, and it is the key looked up in `~/.ssh/config`, so a host that is
already described there needs nothing else:

```
# ~/.ssh/config
Host prod
  HostName bastion.example.com
  Port 2222
  User deploy
  IdentityFile ~/.ssh/id_prod
```

```toml
# config.toml — everything else comes from above
[hosts.prod]
hostname = "prod"
```

Per field the order is **your config → `~/.ssh/config` → default**:

| Field | Fills from | Default |
| --- | --- | --- |
| address | `HostName` | the alias, used literally |
| `port` | `Port` | 22 |
| `user` | `User` | *none — the host is invalid* |
| `identity` | every `IdentityFile`, accumulated | none (fine with the agent) |

`Host *` blocks merge the way OpenSSH merges them, `Include` is followed, and
directives the parser does not implement (`ControlMaster`, `ControlPath`, …) are
ignored rather than treated as errors. Point `[daemon] ssh_config` at a
different file, or set `use_ssh_config = false` on a host to ignore it.

The file is read at startup and re-read on `ssh-tunnel reload` — it is
deliberately **not** watched, because `~/.ssh` also holds `known_hosts`, which
the agent writes to itself under `accept-new`, and watching that directory would
feed the daemon its own reloads.

### Invalid hosts

A host that cannot be used as configured is **reported, not fatal**. The daemon
starts every other host; the invalid one shows up in `status` with the reasons,
and its forwards report as failed rather than silently off:

```
$ ssh-tunnel status
hosts
  NAME     ADDRESS                          USER    STATE
  prod     prod -> bastion.example.com:2222  deploy  connected
  mystery  not-in-ssh-config                 -       invalid (no username: `user` is not set...)
```

`status --json` carries the full reasons plus a `sources` object saying whether
each value came from `config`, `ssh_config`, or a `default` — enough for a
management UI to explain itself. `ssh-tunnel check` prints the same reasons and
exits non-zero.

A host is invalid when it has no username from either source, when
`use_agent = false` leaves it with no identity, or when `~/.ssh/config` gives it
a `ProxyJump` — which the agent cannot honour, and ignoring would silently
connect to the wrong host.

### Authentication

The ssh-agent is tried first (`SSH_AUTH_SOCK` on unix; the OpenSSH agent pipe,
then Pageant, on Windows), then each `identity` file in order. A background
service cannot prompt, so an encrypted key needs either the agent or
`identity_passphrase_env`.

Host keys are checked against `known_hosts`. A **changed** key is always
rejected; `strict_host_key_checking` only decides what happens to an *unknown*
one (`accept-new` learns it, `yes` refuses it, `no` skips the check entirely).

### Reconnection

A dropped session reconnects with jittered exponential backoff. Local and
dynamic listeners stay bound throughout, so the port is never lost to another
process and clients get a clean per-connection failure instead of a burst of
connection-refused errors. Remote forwards are re-requested on the new session.

## Paths

|  | unix | Windows |
| --- | --- | --- |
| Config | `~/.config/ssh-tunnel/config.toml` | `%APPDATA%\ssh-tunnel\config.toml` |
| State | `~/.local/state/ssh-tunnel/state.json` | `%LOCALAPPDATA%\ssh-tunnel\state.json` |
| Control | `$XDG_RUNTIME_DIR/ssh-tunnel.sock`, `$TMPDIR/ssh-tunnel-<uid>.sock` on macOS | `\\.\pipe\ssh-tunnel-<user>.sock` |

The control socket is `0600` on unix; on Windows the pipe's default DACL already
limits it to the creating user.

## Service integration

All three are **per-user** on purpose: a system service runs without the user's
ssh-agent, `known_hosts`, and profile, which is exactly what SSH authentication
needs.

- **macOS** — a LaunchAgent at `~/Library/LaunchAgents/dev.perlmint.ssh-tunnel.plist`,
  logging to `~/Library/Logs/ssh-tunnel/`. A second agent,
  `dev.perlmint.ssh-tunnel.tray`, keeps the menu bar item (`ssh-tunnel tray`)
  running at login; skip it with `service install --no-tray`. Quitting the menu
  bar item keeps it closed until the next login or `service start`;
  `service stop` stops only the daemon.
- **Linux** — a systemd user unit at `~/.config/systemd/user/ssh-tunnel.service`
  (`journalctl --user -u ssh-tunnel -f`). On a headless box, run
  `loginctl enable-linger <user>` so it starts without a login session.
- **Windows** — a scheduled task that runs at logon, logging to a file (a
  scheduled task has nowhere to send stderr). A second task, `ssh-tunnel-tray`,
  starts the tray icon; skip it with `service install --no-tray`. `service stop`
  stops only the daemon.

  The Windows build uses the GUI subsystem so neither task opens a console
  window. From a terminal it attaches to the caller's console, but `cmd` and
  PowerShell do not wait for GUI programs: output can appear after the next
  prompt and the exit code is not reported. Redirected or piped output
  (`ssh-tunnel status --json > status.json`, `... | Out-Host`) waits and keeps
  the exit code.

## Development

```sh
cargo test
```

The integration suite in `tests/` runs a full SSH server, echo target, and
breakable TCP proxy in-process, so it needs no sshd and covers reconnection,
hot reload, and all three forward types on every platform.
