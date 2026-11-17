# sbox

**English** | [简体中文](README.zh_CN.md)

**sbox is currently in early development. It is usable, but may contain bugs
and security vulnerabilities.**

## Introduction

`sbox` is an unprivileged sandbox utility for Linux: it uses user namespaces and a
private mount tree to create a restricted filesystem view, with an isolated `HOME`
and a read-only system layer. X11/Wayland, audio, DRI and user-selected directories
are shared on demand, and D-Bus is filtered through `xdg-dbus-proxy`.

Unlike `Bubblewrap`, which starts a new container on every invocation, sbox keeps
sandboxes alive (by default until every session in the sandbox has ended): a named
sandbox is created on first entry, and running a command in a new terminal under the
same name joins the same set of namespaces.

Runtime requirements: Linux 5.12 or newer with unprivileged user namespaces enabled.
The default configuration also needs `xdg-dbus-proxy` available in `PATH` (not needed
when using `--share-raw-dbus`).

Build requirements: Rust 1.85 or newer.

## Why sbox exists

I previously wrote [`sandbox.sh`](https://github.com/thdaemon/sandbox-sh), a simple
shell script that builds sandboxes with [`Bubblewrap`](https://github.com/containers/bubblewrap)
and `xdg-dbus-proxy`. It already covers most of what sbox does today. But some
technical problems could not be solved with it. The main one is that Bubblewrap is
very good at the first entry into a sandbox — creating namespaces is easy — while
joining an existing set of namespaces is not (at least not from a shell script).
Every `sandbox.sh <name>` produced a brand-new mount tree (even when `HOME` was
shared), a new pid namespace, a new D-Bus proxy, a new `/tmp`, and so on. So I
rewrote it in Rust, without Bubblewrap, creating the namespaces myself.

To fix the problems above, `sbox` keeps a sandbox alive through a daemon after it is
started, and hands the namespace file descriptors over a Unix socket to every session
that attaches. Each `sbox attach` therefore really enters the running sandbox instead
of creating a new one.

sbox also mounts through the modern mount API
(`fsopen`/`fsconfig`/`fsmount`/`open_tree`/`move_mount`/`mount_setattr`), so a
symlink can be used as the target of a bind mount inside the sandbox — something
Bubblewrap has not fixed yet (`/etc/resolv.conf` may be a symlink in some setups).

## Building and installing

### Building

```sh
git clone https://github.com/thdaemon/sbox.git
cd sbox
cargo build --release
```

The binary is `target/release/sbox`.

### Installing

```sh
cargo install --path .                 # installs to ~/.cargo/bin/sbox
```

Or copy it manually:

```sh
sudo install -Dm755 target/release/sbox /usr/local/bin/sbox
```

Verify the install:

```sh
sbox -V
sbox -h
```

## Quick start

### Creating and entering a sandbox

```sh
sbox run work
```

By default this creates the sandbox in `~/Sandbox/work`, which becomes `$HOME`
inside the sandbox, and then starts `$SHELL -l` in it (default `/bin/bash -l`).

You can also specify the command to run inside the sandbox:

```sh
sbox run work -- sh -c 'pwd; hostname; id -u'
```

> If the sandbox has no other sessions, it ends automatically once the command
> exits.

### Entering the same sandbox from another terminal

If the sandbox is still running, the `attach` subcommand runs a command in it:

```sh
sbox attach work
sbox attach work -- ps aux
```

Both invocations see the same mount tree, the same `/tmp` and the same set of
D-Bus proxies.

### Inspecting and stopping

```sh
sbox list
sbox status work
sbox stop work
```

`list` prints every running sandbox, one name per line; `status` shows the
details of one running sandbox.

### Lifetime

When the last session ends, the sandbox exits and cleans up after itself (for
example when you press Ctrl-D in the sandbox shell, or when the command it started
finishes). Two ways to keep it around:

```sh
sbox run -k work                 # keep running after the last session ends
sbox run --no-attach work        # start in the background without a terminal (implies --keep)
```

Afterwards you can enter and stop the sandbox at any time:

```sh
sbox attach work
sbox stop work
```

### Shared directories and capabilities

```sh
sbox run -s ~/projects work              # read-write share
sbox run --ro-share ~/data work          # read-only share
sbox run -x work                         # do not share X11
sbox run --no-audio --no-wayland work    # do not share the audio server or Wayland
sbox run --fakeroot work                 # uid/gid 0 inside the sandbox
sbox run -N work                         # allow nested user namespaces inside the sandbox
sbox run -D work                         # share the real D-Bus socket (dangerous)
```

### Using configuration

```sh
mkdir -p ~/.config/sbox/sandbox.d
```

Then write `~/.config/sbox/sandbox.d/work.toml`:

```toml
command = ["/bin/bash", "-l"]

[[bind]]
source = "~/projects"
mode = "rw"
```

Now `sbox run work` will make `~/projects` appear inside the sandbox's HOME.

### Notes

- Only a process with the same uid, at the same user-namespace level, can attach to a sandbox. This
  is a necessary kernel security restriction.
- Sandbox settings are frozen when the sandbox is created. After changing
  configuration files, run `sbox stop` and then `sbox run`; until then, `attach`
  keeps using the old settings.
- A command shares the terminal it was started from. When the kernel allows
  `TIOCSTI`, a process inside the sandbox could inject keystrokes into that
  terminal and escape the sandbox. Set `dev.tty.legacy_tiocsti` to 0, or run
  `luit` inside the sandbox.
- By default a sandbox cannot create further user namespaces inside it: sbox
  spends one level of the user-namespace budget and verifies that none can be
  created (like bubblewrap's `--disable-userns`), so a process inside cannot
  escape via a nested userns. Use `--nested` to allow it.
- The sandbox `/dev` is a minimal set; other device nodes have to be shared
  explicitly with `[[bind]]` and `mode = "dev"`.

## Full configuration reference

### File locations

```text
$XDG_CONFIG_HOME/sbox/config.toml            program settings
$XDG_CONFIG_HOME/sbox/global.toml            defaults for every sandbox
$XDG_CONFIG_HOME/sbox/sandbox.d/<name>.toml  settings for one sandbox
```

`XDG_CONFIG_HOME` defaults to `~/.config` when unset.

### Precedence

From lowest to highest:

1. Built-in defaults
2. `global.toml`
3. `sandbox.d/<name>.toml`
4. Command-line options

Merge rules: `bind`, `dbus.session` and `dbus.system` append across the three
layers; every other key is replaced.

### config.toml

```toml
sandbox_root = "~/Sandbox"
```

Each sandbox gets `<sandbox_root>/<name>` as its `$HOME`, defaulting to
`~/Sandbox`. A single sandbox can override it with `--sandbox-dir` or the `dir`
key in its configuration file.

### Sandbox settings

```toml
# Hostname
# A string: use this name (unshares UTS)
# `true`: keep the host hostname and share the host UTS namespace
# `false`: use the derived name `<hostname>-sbox-<sandbox>` (unshares UTS)
hostname = "work"

dir = "~/Sandbox/work"   # $HOME inside the sandbox; defaults to <sandbox_root>/<name> (see below)
command = ["/bin/bash", "-l"] # command run by default; when unset, $SHELL -l is used, $SHELL defaulting to /bin/bash
nested = false           # allow nested user namespaces inside the sandbox
fakeroot = false         # map uid/gid 0 to the caller

[shares]
x11 = true               # share the X11 socket and XAUTHORITY
wayland = true           # share the Wayland socket
dri = true               # share /dev/dri
audio = true             # share PipeWire/PulseAudio
resolv_conf = true       # bind the real /etc/resolv.conf
pki = "ro"               # share $HOME/.pki; "ro" read-only, "rw" read-write or "isolate" to not share it

[gui]
gtk_config = true        # read-only share of the user GTK configuration
icons = true             # read-only share of user icons
fonts = true             # read-only share of user fonts
fontconfig = true        # read-only share of the user fontconfig configuration
ibus = true              # set IBUS_USE_PORTAL=1

[dbus]
raw = false              # true shares the real socket directly (dangerous)
debug = false            # enable xdg-dbus-proxy logging
no_default_rules = false # do not add the built-in rules
# add custom session and system bus rules for xdg-dbus-proxy (see below)
session = ["talk=org.freedesktop.portal.*"]
system = []

[env]
FOO_BAR = "1"                     # set an environment variable
DBUS_SESSION_BUS_ADDRESS = false    # remove an environment variable

# bind shared host directories into the sandbox filesystem
[[bind]]
source = "/data"
# mode can be one of the following
# `rw`: read-write, mounted `nosuid,nodev`;
# `ro`: read-only, adds `ro` to the rw flags;
# `dev`: keep device access, mounted `nosuid`, for device nodes, sockets and so on.
mode = "rw"              # optional: "rw", "ro", "dev"

[[bind]]
# Bind sources and destinations must be absolute paths.
source = "/srv/dataset"
dest = "/data/dataset"  # dest defaults to source when unset
mode = "ro"
```

### Sandbox HOME precedence

1. `sbox run --sandbox-dir DIR`
2. The `dir` key in the sandbox configuration file
3. `<sandbox_root>/<name>`
4. `~/Sandbox/<name>`

### D-Bus rules

The strings in `dbus.session` and `dbus.system` are `xdg-dbus-proxy` rules of the
form `talk=org.freedesktop.portal.*`; `talk`/`own`/`see`/`call` are supported.

The following built-in rules are appended by default:

- session: `org.freedesktop.DBus`, `org.freedesktop.portal.*`,
  `org.gnome.SettingsDaemon.MediaKeys`, `org.kde.StatusNotifierWatcher`,
  `org.freedesktop.Notifications`;
- system: `org.freedesktop.DBus`.

The command-line `--dbus-session-rule` / `--dbus-system-rule` options merge with
the rules from the files; `--no-default-dbus-rules` removes the built-in ones.

### Environment variables

A session's environment comes from three parts:

1. Terminal inheritance: whatever the attaching terminal has, the command sees
   (PATH, TERM, locale, and so on);
2. Sandbox settings: values the sandbox sets when it is created, including
   `HOME`, `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY`,
   `PIPEWIRE_REMOTE`/`PIPEWIRE_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`,
   `IBUS_USE_PORTAL` and others;
3. `[env]`: user-configured variables, applied last.

By default `VTE_VERSION` is also removed, as are `XDG_DATA_HOME`,
`XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME`: they may not exist
inside the sandbox, so programs fall back to the matching directories under
`$HOME`. `DBUS_SYSTEM_BUS_ADDRESS` is dropped too: the sandbox exposes the
system bus at its default path (or through the proxy) instead of inheriting
the host address. `[env]` can put any of them back.

### Runtime files

- Runtime files for one sandbox: `$XDG_RUNTIME_DIR/sbox/<name>/` (defaulting
  to `/run/user/<uid>/sbox/<name>/`), holding the control socket `sock`, the
  lock `lock`, and the dbus proxy sockets `dbus-session.sock` and
  `dbus-system.sock`. The path has to support Unix sockets and flock, and be
  under 108 bytes long.
- State directory: `$XDG_STATE_HOME/sbox/<name>/` (default
  `~/.local/state/sbox/<name>/`), with the daemon log in `daemon.log` and,
  when `--debug-dbus` is given, the proxy log in
  `xdg-dbus-proxy-debug.log`. A failure while starting the sandbox is printed
  on the terminal instead.
- Sandbox directory: see "Sandbox HOME precedence" above.

### Command-line options

Global:

| Option | Description |
| --- | --- |
| `-v, --verbose` | Print more detailed logs |

`run`:

| Option | Description |
| --- | --- |
| `NAME` | Sandbox name, required |
| `--sandbox-dir DIR` | `$HOME` inside the sandbox |
| `--hostname NAME` | Sandbox hostname |
| `-H, --real-hostname` | Keep the host hostname (conflicts with `--hostname`) |
| `--fakeroot` | Map uid/gid 0 |
| `-N, --nested` | Allow nested user namespaces inside the sandbox |
| `--no-attach` | Start in the background without entering (implies `--keep`) |
| `-k, --keep` | Keep the sandbox after the last session ends |
| `-s, --share DIR` | Read-write share, repeatable |
| `--ro-share DIR` | Read-only share, repeatable |
| `--no-bind-resolv-conf` | Do not bind the real `/etc/resolv.conf` |
| `-x, --no-x11` | Do not share X11 |
| `--no-dri` | Do not share `/dev/dri` |
| `--no-audio` | Do not share PipeWire/PulseAudio |
| `--no-wayland` | Do not share Wayland |
| `--pki MODE` | How to share `~/.pki`: `ro`, `rw`, `isolate` |
| `-D, --share-raw-dbus` | Share the real D-Bus socket (dangerous) |
| `-d, --dbus-session-rule KIND=RULE` | Append a session bus rule, repeatable |
| `--dbus-system-rule KIND=RULE` | Append a system bus rule, repeatable |
| `--no-default-dbus-rules` | Do not add the built-in D-Bus rules |
| `--debug-dbus` | Print xdg-dbus-proxy logs |
| `--no-gui-workarounds` | Turn off all GUI compatibility handling |
| `--no-gtk-config` | Do not share the GTK configuration |
| `--no-icons` | Do not share icons |
| `--no-fonts` | Do not share fonts |
| `--no-fontconfig` | Do not share the fontconfig configuration |
| `--no-ibus` | Do not set `IBUS_USE_PORTAL` |

`attach`:

| Option | Description |
| --- | --- |
| `NAME` | Sandbox name, required |

`stop`:

| Option | Description |
| --- | --- |
| `NAME` | Sandbox name, required |

`list`: no options

`status`:

| Option | Description |
| --- | --- |
| `NAME` | Sandbox name, required |
| `-j, --json` | Print the state as JSON |

Put `--` before a command that starts with a dash:

```sh
sbox run work -- -weird-command
```

## Legal

Copyright © 2026 Eric Tian <thxdaemon@gmail.com>. All rights reserved.

This program is free software: you can redistribute it and/or modify it under 
the terms of the GNU Affero General Public License as published by the 
Free Software Foundation, either version 3 of the License, or (at your option) 
any later version.

This program is distributed in the hope that it will be useful, but 
WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or 
FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero General Public License 
for more details.

You should have received a copy of the GNU Affero General Public License along 
with this program. If not, see <https://www.gnu.org/licenses/>.

## Acknowledgements

- **[`sandbox.sh`](https://github.com/thdaemon/sandbox-sh)**: the set of resources shared into the sandbox,
  the command-line conventions and the "a name identifies a sandbox"
  interaction come from here;
- **[`Bubblewrap`](https://github.com/containers/bubblewrap)**: an important
  reference for namespace lifetime, capabilities and other security mechanisms;
- Rust and the [crates it depends on](Cargo.toml)

## TODO

- Clean up and refactor the code
- `sbox(1)` man page, shell completions and distribution packaging
- Consider cgroup v2 resource limits and statistics (CPU, memory, process count, ...)
- Consider eBPF based outbound network control (sandboxes currently share the
  host network)
- Consider seccomp, landlock and other hardening mechanisms
- Consider add mitigation for `TIOCSTI` (integrate "allocate a private pty")
- `TODO` and `FIXME` comments in the code
