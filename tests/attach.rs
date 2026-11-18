// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! End to end test of the supervisor/session path.
//!
//! The tests create real user and mount namespaces, so the host has to allow
//! unprivileged user namespaces; there is no namespace-free mode.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sbox")
}

/// Build a sbox invocation with the test environment already applied.
fn sbox_command(base: &std::path::Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("HOME", base).args(args);
    cmd
}

/// The same, with --sandbox-dir, for commands that create a sandbox.
fn sbox_run(base: &std::path::Path, sandbox: &std::path::Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("HOME", base)
        .arg("run")
        .arg("--sandbox-dir")
        .arg(sandbox)
        .args(args);
    cmd
}

/// A status or stop query: no --sandbox-dir, no namespace skipping.
fn sbox_query(base: &std::path::Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("HOME", base).args(args);
    cmd
}
fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sbox-it-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("mkdir");
    base
}

#[test]
fn attach_roundtrip() {
    let base = scratch("roundtrip");
    let sandbox = base.join("sandbox");
    let name = format!("t1{}", std::process::id());

    let status = sbox_run(&base, &sandbox, &["--no-attach", &name])
        .status()
        .expect("start");
    assert!(status.success(), "supervisor did not start");

    let out = sbox_command(
        &base,
        &["attach", &name, "--", "sh", "-c", "echo payload; exit 5"],
    )
    .output()
    .expect("attach");
    assert_eq!(out.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&out.stdout).contains("payload"));

    // A second session must work while the first one is gone.
    let out = sbox_command(&base, &["attach", &name, "--", "sh", "-c", "echo second"])
        .output()
        .expect("attach");
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("second"));

    let status = sbox_query(&base, &["stop", &name]).status().expect("stop");
    assert!(status.success());

    let out = sbox_query(&base, &["attach", &name, "--", "true"])
        .output()
        .expect("attach to stopped sandbox");
    assert!(!out.status.success());

    let _ = std::fs::remove_dir_all(&base);
}

/// Wait until "status" reports the sandbox as gone, or give up after a few
/// seconds. The sandbox is removed a moment after its last session ends.
fn wait_until_gone(base: &std::path::Path, name: &str) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let out = sbox_query(base, &["status", name])
            .output()
            .expect("status");
        if !out.status.success() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

#[test]
fn sandbox_is_removed_after_its_last_session() {
    let base = scratch("oneshot");
    let sandbox = base.join("sandbox");
    let name = format!("once{}", std::process::id());

    let status = sbox_run(&base, &sandbox, &[&name, "--", "sh", "-c", "echo ran"])
        .status()
        .expect("run");
    assert!(status.success());

    assert!(
        wait_until_gone(&base, &name),
        "the sandbox outlived its only session"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn keep_holds_the_sandbox_after_the_session() {
    let base = scratch("keep");
    let sandbox = base.join("sandbox");
    let name = format!("kept{}", std::process::id());

    let status = sbox_run(
        &base,
        &sandbox,
        &["--keep", &name, "--", "sh", "-c", "echo ran"],
    )
    .status()
    .expect("run");
    assert!(status.success());

    std::thread::sleep(std::time::Duration::from_millis(600));
    let out = sbox_query(&base, &["status", &name])
        .output()
        .expect("status");
    assert!(out.status.success(), "a kept sandbox was removed");
    assert!(String::from_utf8_lossy(&out.stdout).contains("running"));

    // It is still attachable after the creating session is gone.
    let out = sbox_command(&base, &["attach", &name, "--", "sh", "-c", "echo again"])
        .output()
        .expect("attach");
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("again"));

    let status = sbox_query(&base, &["stop", &name]).status().expect("stop");
    assert!(status.success());

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn no_attach_keeps_the_sandbox_after_a_session() {
    let base = scratch("detached");
    let sandbox = base.join("sandbox");
    let name = format!("det{}", std::process::id());

    let status = sbox_run(&base, &sandbox, &["--no-attach", &name])
        .status()
        .expect("start");
    assert!(status.success());

    let out = sbox_command(&base, &["attach", &name, "--", "sh", "-c", "echo session"])
        .output()
        .expect("attach");
    assert_eq!(out.status.code(), Some(0));

    std::thread::sleep(std::time::Duration::from_millis(600));
    let out = sbox_query(&base, &["status", &name])
        .output()
        .expect("status");
    assert!(out.status.success(), "--no-attach did not keep the sandbox");

    let status = sbox_query(&base, &["stop", &name]).status().expect("stop");
    assert!(status.success());

    let _ = std::fs::remove_dir_all(&base);
}

/// Wait until the daemon reports a value, or give up after a few seconds.
fn wait_for(base: &std::path::Path, name: &str, key: &str, want: &str) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if status_field(base, name, key).as_deref() == Some(want) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

fn status_field(base: &std::path::Path, name: &str, key: &str) -> Option<String> {
    let out = sbox_query(base, &["status", name]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{key}:")))
        .map(|value| value.trim().to_string())
}

#[test]
fn one_sandbox_per_name_not_per_directory() {
    // The lock is keyed by the sandbox name, like the socket and the state
    // directory. Two runs with different --sandbox-dir used to take different
    // locks, start two daemons and fight over one socket path.
    let base = scratch("lockname");
    let first = base.join("first");
    let second = base.join("second");
    // Sandbox names are global (they key the socket and the lock), so keep
    // them unique per run: a leftover from an interrupted run would otherwise
    // be picked up instead of the one under test.
    let name = format!("dup{}", std::process::id());

    for dir in [&first, &second] {
        let status = sbox_run(&base, dir, &["--keep", "--no-attach", &name])
            .status()
            .expect("start");
        assert!(status.success());
    }

    assert_eq!(
        status_field(&base, &name, "dir").as_deref(),
        Some(first.display().to_string().as_str()),
        "the second run replaced the first sandbox"
    );

    let out = sbox_command(&base, &["attach", &name, "--", "sh", "-c", "echo shared"])
        .output()
        .expect("attach");
    assert_eq!(out.status.code(), Some(0));

    let status = sbox_query(&base, &["stop", &name]).status().expect("stop");
    assert!(status.success());

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn stale_socket_from_a_crash_does_not_block_start() {
    // A daemon killed with SIGKILL leaves its socket behind. The next start
    // holds the lock, finds the socket dead and replaces it.
    let base = scratch("stale");
    let sandbox = base.join("sandbox");
    let name = format!("st{}", std::process::id());

    let start = |extra: &[&str]| sbox_run(&base, &sandbox, extra).status().expect("start");
    assert!(start(&["--keep", "--no-attach", &name]).success());

    let pid = status_field(&base, &name, "pid").expect("pid");
    let killed = Command::new("sh")
        .args(["-c", &format!("kill -9 {pid}")])
        .status()
        .expect("kill");
    assert!(killed.success());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the killed daemon is still there"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    assert!(
        start(&["--keep", "--no-attach", &name]).success(),
        "a leftover socket blocked the restart"
    );
    let out = sbox_command(
        &base,
        &["attach", &name, "--", "sh", "-c", "echo restarted"],
    )
    .output()
    .expect("attach");
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("restarted"));

    let status = sbox_query(&base, &["stop", &name]).status().expect("stop");
    assert!(status.success());

    let _ = std::fs::remove_dir_all(&base);
}

/// Count the zombie children of a process, read straight from /proc so that
/// the check does not depend on ps(1).
fn zombie_children(ppid: u32) -> usize {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // "pid (comm) state ppid ..." and comm may contain spaces or parens.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next().unwrap_or("");
        let parent = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        if parent == ppid && state == "Z" {
            count += 1;
        }
    }
    count
}

#[test]
fn start_reaps_the_setup_process() {
    // Starting a sandbox forks a short-lived setup process. If the caller does
    // not wait for it, it stays a zombie under the client for the whole
    // session, which is what ps shows from another terminal.
    let base = scratch("zombie");
    let sandbox = base.join("sandbox");
    let name = format!("zb{}", std::process::id());
    let mut client = sbox_run(
        &base,
        &sandbox,
        &["--keep", &name, "--", "sh", "-c", "sleep 2"],
    )
    .spawn()
    .expect("spawn");

    std::thread::sleep(std::time::Duration::from_millis(800));
    assert_eq!(
        zombie_children(client.id()),
        0,
        "the setup process was left unreaped"
    );

    let _ = client.wait();
    let status = sbox_query(&base, &["stop", &name]).status().expect("stop");
    assert!(status.success());

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn status_comes_from_the_daemon() {
    // The sandbox state lives in the daemon, not in a file: dir, host pid and
    // session list are answered over the control socket.
    let base = scratch("status");
    let sandbox = base.join("sandbox");
    let name = format!("st{}", std::process::id());

    let status = sbox_run(&base, &sandbox, &["--keep", "--no-attach", &name])
        .status()
        .expect("start");
    assert!(status.success());

    // A status query is a probe, not a session.
    for _ in 0..3 {
        let out = sbox_query(&base, &["status", &name])
            .output()
            .expect("status");
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(text.contains("sessions: 0"), "{text}");
    }

    assert_eq!(
        status_field(&base, &name, "dir"),
        Some(sandbox.display().to_string())
    );

    let pid: i32 = status_field(&base, &name, "pid")
        .expect("pid")
        .parse()
        .expect("numeric pid");
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).expect("daemon comm");
    assert_eq!(comm.trim(), format!("sbox:{name}"));

    let mut session = sbox_command(&base, &["attach", &name, "--", "sh", "-c", "sleep 2"])
        .spawn()
        .expect("attach");
    assert!(
        wait_for(&base, &name, "sessions", "1"),
        "an attached session is not reported"
    );
    let _ = session.wait();

    let status = sbox_query(&base, &["stop", &name]).status().expect("stop");
    assert!(status.success());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn list_tracks_live_sandboxes_only() {
    let base = scratch("listing");
    let sandbox = base.join("sandbox");
    let name = format!("ls{}", std::process::id());

    let status = sbox_run(&base, &sandbox, &["--keep", "--no-attach", &name])
        .status()
        .expect("start");
    assert!(status.success());

    let out = sbox_query(&base, &["list"]).output().expect("list");
    assert!(String::from_utf8_lossy(&out.stdout).contains(&name));

    let out = sbox_query(&base, &["status", "-j", &name])
        .output()
        .expect("json status");
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json status");
    assert_eq!(json["name"].as_str(), Some(name.as_str()));
    assert!(json["pid"].as_i64().is_some());
    assert!(json["sessions"].is_array());

    let status = sbox_query(&base, &["stop", &name]).status().expect("stop");
    assert!(status.success());

    let out = sbox_query(&base, &["list"]).output().expect("list");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains(&name),
        "a stopped sandbox is still listed"
    );

    // Stopping something that is not there is an error, not a hung process.
    let out = sbox_query(&base, &["stop", &name])
        .output()
        .expect("stop again");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not running"));

    let _ = std::fs::remove_dir_all(&base);
}

/// A client that asks for a big attach reply and never reads it must not stop
/// the sandbox from answering anyone else: the daemon buffers the reply and
/// waits for the socket to drain instead of blocking in write(2).
#[test]
fn a_client_that_does_not_read_does_not_stall_the_daemon() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let base = scratch("slowread");
    let sandbox = base.join("sandbox");
    let name = format!("sr{}", std::process::id());
    let config = base.join(".config");

    // A reply well past any default socket buffer.
    let pad = "x".repeat(60_000);
    let dir = config.join("sbox/sandboxes");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let mut toml = String::from("[env]\n");
    for index in 0..4 {
        toml.push_str(&format!("PAD{index} = \"{pad}\"\n"));
    }
    std::fs::write(dir.join(format!("{name}.toml")), toml).expect("config");

    let status = Command::new(bin())
        .env("HOME", &base)
        .env("XDG_CONFIG_HOME", &config)
        .arg("run")
        .arg("--sandbox-dir")
        .arg(&sandbox)
        .args(["--keep", "--no-attach", &name])
        .status()
        .expect("start");
    assert!(status.success(), "the sandbox did not start");

    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))
        });
    let socket = runtime.join("sbox").join(&name).join("sock");

    // Ask to attach, then never read a byte.
    let mut stalled = UnixStream::connect(&socket).expect("connect");
    let mut header = Vec::new();
    header.extend_from_slice(b"SBOX");
    header.extend_from_slice(&1u16.to_be_bytes());
    header.extend_from_slice(&(b'A' as u16).to_be_bytes());
    header.extend_from_slice(&0u32.to_be_bytes());
    stalled.write_all(&header).expect("request");

    // The daemon has to keep serving: status must answer while the reply sits
    // in the socket buffer.
    let out = Command::new("timeout")
        .env("HOME", &base)
        .args(["5", bin(), "status", &name])
        .output()
        .expect("status");
    assert!(
        out.status.success(),
        "status did not answer while a reply was pending: {out:?}"
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("sessions: 1"),
        "the pending session is not reported: {text}"
    );

    drop(stalled);
    let status = sbox_query(&base, &["stop", &name]).status().expect("stop");
    assert!(status.success());
    let _ = std::fs::remove_dir_all(&base);
}

/// A source under the host /tmp has to survive the anchor mount: sources are
/// resolved through the old root, not through live paths.
#[test]
fn shares_are_resolved_through_the_old_root() {
    let base = scratch("oldroot");
    let sandbox = base.join("sandbox");
    let share = std::env::temp_dir().join(format!("sbox-it-share-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&share);
    std::fs::create_dir_all(&share).expect("mkdir share");
    std::fs::write(share.join("marker"), "payload").expect("write marker");

    let name = format!("t9{}", std::process::id());
    let status = sbox_run(
        &base,
        &sandbox,
        &[
            "--no-attach",
            "--ro-share",
            share.to_str().expect("utf-8 share"),
            &name,
        ],
    )
    .status()
    .expect("start");
    assert!(status.success(), "supervisor did not start");

    let script = format!(
        "cat '{}/marker'; test -e /oldroot && echo oldroot-leaked; true",
        share.display()
    );
    let out = sbox_command(&base, &["attach", &name, "--", "sh", "-c", &script])
        .output()
        .expect("attach");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("payload"), "share not mounted: {text}");
    assert!(
        !text.contains("oldroot-leaked"),
        "old root reachable: {text}"
    );

    let status = sbox_query(&base, &["stop", &name]).status().expect("stop");
    assert!(status.success());
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&share);
}

/// A file bind whose destination is a symlink with a missing target -- the
/// /etc/resolv.conf case, when the host file is a symlink to a path the
/// sandbox has not built yet -- mounts straight onto the link. The new mount
/// API can attach to a symlink descriptor, so the target never has to exist:
/// reading the link returns the bound content, and the link is no longer a
/// symlink.
#[test]
fn a_file_bind_mounts_onto_a_dangling_symlink() {
    let base = scratch("dangling");
    let sandbox = base.join("sandbox");
    let name = format!("dang{}", std::process::id());

    std::fs::create_dir_all(&sandbox).expect("mkdir sandbox");
    let payload = base.join("payload");
    std::fs::write(&payload, "through-the-link\n").expect("write payload");

    // The sandbox directory becomes $HOME, so a symlink placed in it is
    // visible inside the sandbox at $HOME/link.
    std::os::unix::fs::symlink("/dangling-target", sandbox.join("link")).expect("symlink");

    // Bind the payload onto $HOME/link, which is a symlink to a path that
    // does not exist in the sandbox yet.
    let config = base.join(".config");
    let dir = config.join("sbox/sandbox.d");
    std::fs::create_dir_all(&dir).expect("mkdir config");
    let dest = base.join("link");
    let toml = format!(
        "[[bind]]\nsource = \"{}\"\ndest = \"{}\"\nmode = \"ro\"\n",
        payload.display(),
        dest.display()
    );
    std::fs::write(dir.join(format!("{name}.toml")), toml).expect("config");

    let status = Command::new(bin())
        .env("HOME", &base)
        .env("XDG_CONFIG_HOME", &config)
        .arg("run")
        .arg("--sandbox-dir")
        .arg(&sandbox)
        .args(["--no-attach", &name])
        .status()
        .expect("start");
    assert!(status.success(), "the sandbox did not start");

    // The bind lands on the link itself: reading it returns the payload, the
    // link is now a regular file, and its (missing) target was never created.
    let script = format!(
        "cat '{dest}' || echo cat-failed; \
         test -e /dangling-target && echo target-made; \
         test -L '{dest}' && echo still-link; \
         true",
        dest = dest.display()
    );
    let out = sbox_command(&base, &["attach", &name, "--", "sh", "-c", &script])
        .output()
        .expect("attach");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("through-the-link"),
        "bind missed the link: {text}"
    );
    assert!(
        !text.contains("target-made"),
        "the dangling target was created: {text}"
    );
    assert!(
        !text.contains("still-link"),
        "the link was not covered by the mount: {text}"
    );

    let status = sbox_query(&base, &["stop", &name]).status().expect("stop");
    assert!(status.success());
    let _ = std::fs::remove_dir_all(&base);
}
