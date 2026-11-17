# sbox

[English](README.md) | **简体中文**

**当前 sbox 处于早期开发阶段，它可用但可能存在潜在的 Bug 和安全漏洞。**

## 简介

`sbox` 是 Linux 上的非特权沙盒实用工具：它利用 user namespace 和私有挂载树创建受限的文件系统视图，具有独立的 `HOME` 和只读的系统层，并按需共享 X11/Wayland、音频、DRI 和用户指定目录等资源，D-Bus 则经过 `xdg-dbus-proxy` 过滤。

与 `Bubblewrap` 的“每次启动一个新容器”不同，sbox 的沙盒是持久的（默认情况下直到沙盒中每一个会话都结束）：命名的沙盒在第一次进入时被创建，之后在新的终端在同名沙盒中执行命令会加入同一组 namespace。

运行要求：Linux 5.12 及以上，开启非特权 user namespace；默认配置下还需要 `xdg-dbus-proxy` 在 `PATH` 中（使用 `--share-raw-dbus` 时不需要）。

构建要求：Rust 1.85 及以上。

## 为什么要有 sbox

我之前创建了 [`sandbox.sh`](https://github.com/thdaemon/sandbox-sh)，一个简单的 shell 脚本，利用 [`Bubblewrap`](https://github.com/containers/bubblewrap) 和 `xdg-dbus-proxy` 创建沙盒，基本上拥有当前 sbox 实用工具的大部分能力。但是存在一些技术问题无法解决，其中最主要的是 `Bubblewrap` 在第一次进入沙盒时做的很好，可以很方便地创建 namespace，但对于加入一组已经存在的 namespace 则并不容易（至少很难仅通过 shell 脚本解决），每次执行 `sandbox.sh <名字>`，得到的都是一套全新的挂载树（尽管它们共享了 `HOME`）、新的 pid namespace、新的 D-Bus 代理，新的 `/tmp` 等等。因此我用 Rust 重写了它，不依赖 `Bubblewrap`，自己创建 namespace。

为了解决上述 `sandbox.sh` 中的问题，`sbox` 的做法是在启动指定沙盒后通过守护进程保持沙盒存活，并把 namespace 的文件描述符通过 Unix socket 交给每一次附加到该沙盒的会话，这样每次 `sbox attach`，真正是“进入”了已运行的沙盒而不是创建新的沙盒。

此外 sbox 挂载使用现代挂载 API（`fsopen`/`fsconfig`/`fsmount`/`open_tree`/`move_mount`/`mount_setattr`），所以可以使用符号链接作为目标在沙盒内重新 bind mount，而截至目前 `Bubblewrap` 尚未解决这个问题（`/etc/resolv.conf` 在某些情况下可能是符号链接）。

## 构建与安装

### 构建

```sh
git clone https://github.com/thdaemon/sbox.git
cd sbox
cargo build --release
```

产物是 `target/release/sbox`。

### 安装

```sh
cargo install --path .                 # 安装到 ~/.cargo/bin/sbox
```

或者手动复制：

```sh
sudo install -Dm755 target/release/sbox /usr/local/bin/sbox
```

验证：

```sh
sbox -V
sbox -h
```

## 快速开始

### 创建并进入沙盒

```sh
sbox run work
```

默认情况下，这将在 `~/Sandbox/work` 创建沙盒，它将作为沙盒内的 `$HOME`，在那之后，会在沙盒中启动 `$SHELL -l`（默认 `/bin/bash -l`）。

你也可以指定要在沙盒中执行的命令：

```sh
sbox run work -- sh -c 'pwd; hostname; id -u'
```

> 如果该沙盒没有其他会话，默认情况下命令退出后自动结束沙盒。

### 从另一个终端进入同一个沙盒

如果沙盒没有结束，可以使用 `attach` 子命令在同一个沙盒中执行命令

```sh
sbox attach work
sbox attach work -- ps aux
```

两次调用看到的是同一棵挂载树、同一个 `/tmp`、同一组 D-Bus 代理。


### 查看和停止

```sh
sbox list
sbox status work
sbox stop work
```

`list` 列出所有在运行中的沙盒，每行一个；`status` 输出显示指定运行中沙盒的详细信息。

### 生命周期

最后一个会话结束时，沙盒会自动退出并清理（比如在沙盒 shell 里按 Ctrl-D，或启动的命令结束）。两种方式可以让它留下：

```sh
sbox run -k work                 # 最后一个会话结束后继续运行
sbox run --no-attach work        # 后台启动，不打开终端（隐含 --keep）
```

之后你可以随时进入和停止沙盒：

```sh
sbox attach work
sbox stop work
```

### 共享目录和能力

```sh
sbox run -s ~/projects work              # 读写共享
sbox run --ro-share ~/data work          # 只读共享
sbox run -x work                         # 不共享 X11
sbox run --no-audio --no-wayland work    # 不共享音频服务器和 wayland
sbox run --fakeroot work                 # 沙盒里 uid/gid 为 0
sbox run -N work                         # 允许在沙盒里再建 user namespace
sbox run -D work                         # 共享真实 D-Bus socket（危险）
```

### 使用配置

```sh
mkdir -p ~/.config/sbox/sandbox.d
```

然后在 `~/.config/sbox/sandbox.d/work.toml` 里写：

```toml
command = ["/bin/bash", "-l"]

[[bind]]
source = "~/projects"
mode = "rw"
```

之后直接 `sbox run work`，你会发现 ~/projects 出现在沙盒的 HOME 中。

### 注意事项

- 只有创建沙盒的同一个 uid、同一个 user namespace 层级能 attach 它。这是内核实施的必要的安全限制。
- 沙盒设置在建沙盒时冻结，修改配置文件后需要 `sbox stop` 再 `sbox run`，在那之前，`attach` 仍然使用旧配置。
- 命令与调用它的终端共享；内核被配置为允许 `TIOCSTI` 时，沙盒内进程可能向该终端注入按键以造成沙盒逃逸，建议把 `dev.tty.legacy_tiocsti` 设为 0，或在沙盒里运行 `luit`。
- 默认情况下沙盒内无法再创建 user namespace：sbox 会花掉一层 user namespace 预算并校验确实无法创建（类似 bubblewrap 的 `--disable-userns`），以防沙盒内进程借嵌套 userns 逃逸；需要时用 `--nested` 打开。
- 沙盒的 `/dev` 是一个最小集合；其他设备节点需要显式用 `[[bind]]` 加 `mode = "dev"` 共享。

## 完整配置说明

### 文件位置

```text
$XDG_CONFIG_HOME/sbox/config.toml            程序配置
$XDG_CONFIG_HOME/sbox/global.toml            所有沙盒的默认值
$XDG_CONFIG_HOME/sbox/sandbox.d/<name>.toml  单个沙盒的设置
```

`XDG_CONFIG_HOME` 未设置时默认是 `~/.config`。

### 优先级

从低到高：

1. 内置默认值
2. `global.toml`
3. `sandbox.d/<name>.toml`
4. 命令行选项

合并规则：`bind`、`dbus.session`、`dbus.system` 在三层之间追加；其他键被覆盖。

### config.toml

```toml
sandbox_root = "~/Sandbox"
```

每个沙盒得到 `<sandbox_root>/<name>` 作为沙盒内的 `$HOME`，默认 `~/Sandbox`。单个沙盒可以用 `--sandbox-dir` 或配置文件里的 `dir` 覆盖。

### 沙盒设置

```toml
# 主机名
# 字符串：使用这个名字（会 unshare UTS）
# `true`：保留宿主主机名，共享宿主的 UTS namespace
# `false`：使用自动派生名 `<宿主名>-sbox-<沙盒名>`（会 unshare UTS）
hostname = "work"

dir = "~/Sandbox/work"   # 沙盒内 $HOME，默认为 <sandbox_root>/<name>（见下方说明）
command = ["/bin/bash", "-l"] # 默认执行的命令，不配置时使用 $SHELL -l，$SHELL 默认 /bin/bash
nested = false           # 允许沙盒内再建 user namespace
fakeroot = false         # 把 uid/gid 0 映射给调用者

[shares]
x11 = true               # 共享 X11 socket 和 XAUTHORITY
wayland = true           # 共享 Wayland socket
dri = true               # 共享 /dev/dri
audio = true             # 共享 PipeWire/PulseAudio
resolv_conf = true       # 绑定真实 /etc/resolv.conf
pki = "ro"               # 共享 $HOME/.pki，可配置为 "ro" 只读共享，"rw" 读写共享或 "isolate" 不共享

[gui]
gtk_config = true        # 只读共享用户级 GTK 配置文件
icons = true             # 只读共享用户级图标资源
fonts = true             # 只读共享用户级字体
fontconfig = true        # 只读共享用户级字体配置
ibus = true              # 设置 IBUS_USE_PORTAL=1

[dbus]
raw = false              # true 直接共享真实 socket（危险）
debug = false            # 启用 xdg-dbus-proxy 日志
no_default_rules = false # 不添加内置规则
# 给 xdg-dbus-proxy 添加自定义会话总线及系统总线规则（见下方说明）
session = ["talk=org.freedesktop.portal.*"]
system = []

[env]
FOO_BAR = "1"                     # 设置环境变量
DBUS_SESSION_BUS_ADDRESS = false    # 删除环境变量

# 向沙盒文件系统绑定共享主机目录
[[bind]]
source = "/data"
# mode 可配置为以下值
# `rw`：读写，挂载为 `nosuid,nodev`；
# `ro`：只读，在 rw 的基础上额外加 `ro`；
# `dev`：保留设备访问，挂载为 `nosuid`，用于设备节点、socket 等。
mode = "rw"              # 可选："rw", "ro", "dev"

[[bind]]
# `[[bind]]` 的 `source` 和 `dest` 必须是绝对路径。
source = "/srv/dataset"
dest = "/data/dataset"  # dest 不配置时与 source 相同
mode = "ro"
```

### 沙盒 HOME 目录优先级

1. `sbox run --sandbox-dir DIR`
2. 沙盒配置文件里配置的 `dir`
3. `<sandbox_root>/<name>`
4. `~/Sandbox/<name>`

### D-Bus 规则

`dbus.session` 和 `dbus.system` 里的字符串是 `xdg-dbus-proxy` 的规则，形如 `talk=org.freedesktop.portal.*`，支持 `talk`/`own`/`see`/`call`。

以下内置规则默认会被追加：

- session：`org.freedesktop.DBus`、`org.freedesktop.portal.*`、`org.gnome.SettingsDaemon.MediaKeys`、`org.kde.StatusNotifierWatcher`、`org.freedesktop.Notifications`；
- system：`org.freedesktop.DBus`。

命令行 `--dbus-session-rule` / `--dbus-system-rule` 与文件里的规则合并；`--no-default-dbus-rules` 去掉内置规则。

### 环境变量

会话的环境变量来自三个部分：

1. 终端继承：attach 的那个终端里有什么，命令就能看到什么（PATH、TERM、locale 等）；
2. 沙盒设置：沙盒在创建时修改的值，包括 `HOME`、`DISPLAY`、`WAYLAND_DISPLAY`、`XAUTHORITY`、`PIPEWIRE_REMOTE`/`PIPEWIRE_RUNTIME_DIR`、`DBUS_SESSION_BUS_ADDRESS`、`IBUS_USE_PORTAL` 等；
3. `[env]`：用户配置的环境变量最后应用。

默认还会删除 `VTE_VERSION`，以及 `XDG_DATA_HOME`、`XDG_CONFIG_HOME`、`XDG_STATE_HOME`、`XDG_CACHE_HOME`，它们在沙盒中未必存在，以使程序会回落到 `$HOME` 下的对应目录。`DBUS_SYSTEM_BUS_ADDRESS` 也会被删除：沙盒通常在默认路径暴露系统总线（或经过代理），不继承宿主的地址。`[env]` 可以把想要的变量放回来。

### 运行时文件

- 单个沙盒的运行时文件：`$XDG_RUNTIME_DIR/sbox/<name>/`（未设置时默认 `/run/user/<uid>/sbox/<name>/`），其中控制 socket 为 `sock`，锁为 `lock`，dbus 代理 socket 分别为 `dbus-session.sock` 和 `dbus-system.sock`；该路径必须支持 unix socket 和 flock，且总长度小于 108 字节。
- 状态目录：`$XDG_STATE_HOME/sbox/<name>/`（默认 `~/.local/state/sbox/<name>/`），daemon 日志在其中的 `daemon.log`；开启 `--debug-dbus` 时代理日志在 `xdg-dbus-proxy-debug.log`。启动沙盒失败时错误会直接打印在终端上。
- 沙盒目录：见上文“沙盒 HOME 目录优先级”。

### 命令行选项

全局：

| 选项 | 说明 |
| --- | --- |
| `-v, --verbose` | 打印更详细的日志 |

`run`：

| 选项 | 说明 |
| --- | --- |
| `NAME` | 沙盒名，必填 |
| `--sandbox-dir DIR` | 沙盒内 `$HOME` |
| `--hostname NAME` | 沙盒主机名 |
| `-H, --real-hostname` | 保留宿主主机名（与 `--hostname` 互斥） |
| `--fakeroot` | 映射 uid/gid 0 |
| `-N, --nested` | 允许沙盒内再建 user namespace |
| `--no-attach` | 后台启动，不进入（隐含 `--keep`） |
| `-k, --keep` | 最后一个会话结束后保留沙盒 |
| `-s, --share DIR` | 读写共享目录，可重复 |
| `--ro-share DIR` | 只读共享目录，可重复 |
| `--no-bind-resolv-conf` | 不绑定真实 `/etc/resolv.conf` |
| `-x, --no-x11` | 不共享 X11 |
| `--no-dri` | 不共享 `/dev/dri` |
| `--no-audio` | 不共享 PipeWire/PulseAudio |
| `--no-wayland` | 不共享 Wayland |
| `--pki MODE` | `~/.pki` 的共享方式：`ro`、`rw`、`isolate` |
| `-D, --share-raw-dbus` | 共享真实 D-Bus socket（危险） |
| `-d, --dbus-session-rule KIND=RULE` | 追加 session bus 规则，可重复 |
| `--dbus-system-rule KIND=RULE` | 追加 system bus 规则，可重复 |
| `--no-default-dbus-rules` | 不添加内置 D-Bus 规则 |
| `--debug-dbus` | 打印 xdg-dbus-proxy 日志 |
| `--no-gui-workarounds` | 关闭所有 GUI 兼容处理 |
| `--no-gtk-config` | 不共享 GTK 配置 |
| `--no-icons` | 不共享图标 |
| `--no-fonts` | 不共享字体 |
| `--no-fontconfig` | 不共享 fontconfig 配置 |
| `--no-ibus` | 不设置 `IBUS_USE_PORTAL` |

`attach`：

| 选项 | 说明 |
| --- | --- |
| `NAME` | 沙盒名，必填 |

`stop`：

| 选项 | 说明 |
| --- | --- |
| `NAME` | 沙盒名，必填 |

`list`： 无参数

`status`：

| 选项 | 说明 |
| --- | --- |
| `NAME` | 沙盒名，必填 |
| `-j, --json` | 以 JSON 格式输出状态 |

可在要执行的命令前加 `--`：

```sh
sbox run work -- -weird-command
```

## 法律声明

版权所有 © 2026 Eric Tian <thxdaemon@gmail.com>。所有权利保留。

本程序是自由软件：你可以依照由自由软件基金会（Free Software Foundation）发布的 GNU Affero 通用公共许可证条款，重新分发和/或修改本程序，许可证版本可以选择第 3 版，或者（由您自行决定）任何更高版本。

发布该程序是希望它能有用，但不提供任何明示或默示的保证，包括但不限于对适销性及特定用途适用性的默示担保。请参阅 GNU Affero 通用公共许可证了解详情。

你应当已经随本程序一并收到 GNU Affero 通用公共许可证的副本。如果没有，请访问 <https://www.gnu.org/licenses/>。

## 鸣谢

- **[sandbox.sh](https://github.com/thdaemon/sandbox-sh)**：沙盒的共享集合、命令行习惯和“用名字标识沙盒”的交互方式来自这里；
- **[Bubblewrap](https://github.com/containers/bubblewrap)**：namespace 生命周期、capability 等安全机制的处理方式提供了重要参照；
- Rust 和[依赖的 crates](Cargo.toml)

## TODO

- 清理/重构代码
- `sbox(1)` man page、shell 补全和发行版打包
- 考虑实现 cgroup v2 资源限制与统计（CPU、内存、进程数等）
- 考虑实现基于 eBPF 的出站网络控制（当前沙盒共享宿主网络）
- 考虑引入 seccomp 和 landlock 等更多安全强化机制
- 考虑实现 `TIOCSTI` 的默认缓解（集成“分配私有 pty”的功能）
- 代码中的 `TODO` 和 `FIXME` 注释
