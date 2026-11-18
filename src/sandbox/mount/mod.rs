// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Construction of the sandbox mount tree.
//!
//! The tree is built from detached mounts (open_tree / fsmount) that are
//! attached to descriptors inside a private tmpfs. Mounts attach to a
//! descriptor, not a path, so a file bind can land straight on a symlink --
//! /etc/resolv.conf when it points at a path the sandbox has not built yet --
//! and the mount shadows the link without its target ever existing.
//!
//! The private tmpfs is mounted on a fixed host directory (ANCHOR), the way
//! bubblewrap does it: the tree itself is built in /newroot, the first pivot
//! moves the host root to /oldroot, and every source is resolved there. That
//! keeps the host /tmp reachable as a source even after the anchor covers it,
//! and leaves nothing behind on the host.
//!
//! A destination is a guest path: /usr and usr name the same entry, and it is
//! always resolved from the root descriptor. Resolution is openat2's job:
//! RESOLVE_IN_ROOT follows symlinks and clamps ".." and absolute targets to
//! the root. Creation only ever adds the missing tail below the deepest
//! prefix that resolves (see Root::ensure), so the kernel still does all
//! resolution of what exists. Whole paths only ever reach openat2; the plain
//! *at() calls take one component at a time, so they cannot be made to ignore
//! their directory descriptor.

mod builder;

pub(super) use builder::Builder;

use crate::config::BindMode;
use crate::linux::fs;
use crate::linux::fs::{
    FsNode, MOUNT_ATTR_NODEV, MOUNT_ATTR_NOEXEC, MOUNT_ATTR_NOSUID, MOUNT_ATTR_RDONLY,
};
use anyhow::{Context, Result, bail};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// Split a guest path into the directory to walk into and the one name to
/// create there.
fn split_last(path: &Path) -> Result<(&Path, PathBuf)> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok((parent, PathBuf::from(name))),
        _ => bail!("{}: not a file name", path.display()),
    }
}

/// The host directory the sandbox root is anchored on.
///
/// It has to exist already, it must not be a symlink someone else controls,
/// and nothing may need it as a source once the root tmpfs is mounted on it.
/// Sources that used to live under it stay reachable through OLD_ROOT.
const ANCHOR: &str = "/tmp";

/// Where the tree is built; becomes the root after the second pivot.
const NEW_ROOT: &str = "/newroot";

/// Where the host root waits until the tree is complete.
const OLD_ROOT: &str = "/oldroot";

/// What the host has at a path.
///
/// Looking it up is mechanics; deciding what a link or a directory means is
/// the caller's policy, so this says only what is there.
pub(super) enum HostEntryKind {
    /// Nothing there, or a dangling symbolic link.
    Missing,
    /// A symbolic link, with the target as the link spells it.
    SymLink(PathBuf),
    /// A directory, file, device node or anything else.
    Other,
}

pub(super) struct Root {
    /// The tree under construction, after the first pivot.
    root: FsNode,
    /// The host root, until finish() detaches it.
    host: FsNode,
}

impl Root {
    /// Create the tree and pivot into it.
    ///
    /// Two pivots, the way bubblewrap does it. The first makes the anchor
    /// tmpfs the root and moves the host root to /oldroot, which uncovers the
    /// host /tmp: from then on every source is resolved inside /oldroot and
    /// never through a live path. The second, in finish(), makes /newroot the
    /// root and drops the anchor and everything left in it.
    pub fn create() -> Result<Root> {
        let anchor = Path::new(ANCHOR);
        let new_root = anchor.join("newroot");
        let old_root = anchor.join("oldroot");

        fs::FsContext::new("tmpfs")?
            .str("mode", "0755")?
            .mount(MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV)?
            .attach_at(libc::AT_FDCWD, anchor)
            .with_context(|| format!("mounting the root tmpfs on {ANCHOR}"))?;

        let anchor_fd = FsNode::open(
            anchor,
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
        )?;
        anchor_fd.as_dir().mkdir(Path::new("newroot"), 0o755)?;
        anchor_fd.as_dir().mkdir(Path::new("oldroot"), 0o755)?;

        // /newroot has to be a mount point: the second pivot targets it by
        // name.
        fs::FsMount::clone_from(&new_root)
            .context("cloning the new root")?
            .attach_at(libc::AT_FDCWD, &new_root)
            .context("binding the new root onto itself")?;

        fs::pivot_root(anchor, &old_root)
            .with_context(|| format!("pivoting to the anchor {ANCHOR}"))?;
        std::env::set_current_dir("/").context("chdir /")?;

        let root = FsNode::open(
            Path::new(NEW_ROOT),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
        )?;
        let host = FsNode::open(
            Path::new(OLD_ROOT),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
        )?;

        Ok(Root { root, host })
    }

    /// Resolve a path inside the new root, creating the missing parts. Returns
    /// a descriptor for the final component, which is what mounts attach to.
    ///
    /// A destination that resolves is returned as is; it is opened O_NOFOLLOW,
    /// so a dangling symlink at the end -- /etc/resolv.conf before its target
    /// is built -- yields the link itself, and a file bind attaches straight to
    /// it (the mount shadows the link). A directory bind onto a link fails the
    /// want_dir check. Otherwise the parent is ensured as a directory and the
    /// one final component is created under it.
    fn ensure(&self, dest: impl AsRef<Path>, want_dir: bool) -> Result<FsNode> {
        let dest = dest.as_ref();

        match self.root.open_child_in_root(
            dest,
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        ) {
            Ok(fd) => {
                if want_dir && fd.mode()? & libc::S_IFMT != libc::S_IFDIR {
                    bail!("{}: not a directory", dest.display());
                }
                return Ok(fd);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("resolving {}", dest.display())),
        }

        // dest does not resolve: ensure its parent is a directory, then create
        // the one final component under it, and return that descriptor directly.
        let (parent, name) =
            split_last(dest).with_context(|| format!("creating {}", dest.display()))?;
        let dir = if parent.as_os_str().is_empty() {
            self.root.dup()?
        } else {
            self.ensure_dir(parent)?
        };

        if want_dir {
            dir.as_dir()
                .mkdir(&name, 0o755)
                .with_context(|| format!("mkdir {}", name.to_string_lossy()))
        } else {
            dir.open_child(
                &name,
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC,
                0o644,
            )
            .with_context(|| format!("creating {}", name.to_string_lossy()))
        }
    }

    /// Ensure `dest` exists as a directory, creating the missing components,
    /// and return its descriptor. Everything it creates is a directory, so
    /// there is no final-file case. openat2(RESOLVE_IN_ROOT) follows symlinks
    /// above the final component and clamps ".." and absolute targets to the
    /// root; O_NOFOLLOW on the descent makes a dangling symlink in the path
    /// fail as ENOTDIR instead of being followed into a missing target.
    fn ensure_dir(&self, dest: &Path) -> Result<FsNode> {
        match self.root.open_child_in_root(
            dest,
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
        ) {
            Ok(fd) => return Ok(fd),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("resolving {}", dest.display())),
        }

        // Walk upward to the deepest prefix that resolves, collecting the
        // missing components (deepest first).
        let mut missing: Vec<PathBuf> = Vec::new();
        let mut cur_path = dest.to_path_buf();
        let dir = loop {
            let (parent, name) =
                split_last(&cur_path).with_context(|| format!("creating {}", dest.display()))?;
            missing.push(name);

            if parent.as_os_str().is_empty() {
                // A relative path with one component: the root is its parent.
                break self.root.dup()?;
            }

            match self
                .root
                .open_child_in_root(parent, libc::O_PATH | libc::O_CLOEXEC, 0)
            {
                Ok(fd) => {
                    if fd.mode()? & libc::S_IFMT != libc::S_IFDIR {
                        bail!("{}: not a directory", parent.display());
                    }
                    break fd;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    cur_path = parent.to_path_buf();
                }
                Err(e) => return Err(e).with_context(|| format!("resolving {}", parent.display())),
            }
        };

        // Create them all as directories, shallowest first; mkdir returns the
        // new directory, so it doubles as the descent.
        let mut cur = dir;
        for component in missing.into_iter().rev() {
            cur = cur
                .as_dir()
                .mkdir(&component, 0o755)
                .with_context(|| format!("mkdir {}", component.to_string_lossy()))?;
        }

        Ok(cur)
    }

    pub fn dir(&self, dest: impl AsRef<Path>) -> Result<()> {
        self.ensure_dir(dest.as_ref()).map(|_| ())
    }

    pub fn dir_mode(&self, dest: impl AsRef<Path>, mode: u32) -> Result<()> {
        let dest = dest.as_ref();
        self.ensure_dir(dest)?
            .as_dir()
            .reopen_self()?
            .chmod(mode)
            .with_context(|| format!("chmod {}", dest.display()))
    }

    /// Look up a path in the host root without following the final
    /// component. The caller decides what a link or a directory means.
    pub fn host_entry(&self, path: &Path) -> Result<HostEntryKind> {
        let fd = match self.host.open_child_in_root(
            path,
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        ) {
            Ok(fd) => fd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HostEntryKind::Missing);
            }
            Err(e) => return Err(e).with_context(|| format!("resolving {}", path.display())),
        };

        if fd.mode()? & libc::S_IFMT != libc::S_IFLNK {
            return Ok(HostEntryKind::Other);
        }

        Ok(HostEntryKind::SymLink(fd.read_link()?))
    }

    /// The source tree a bind will clone, resolved inside the host root.
    ///
    /// The anchor tmpfs covers the host's /tmp for the rest of the build, so a
    /// source must never be resolved as a live path here; this resolves it in
    /// the old root instead. None means the source does not exist there.
    fn source(&self, src: &Path) -> Result<Option<FsNode>> {
        match self
            .host
            .open_child_in_root(src, libc::O_PATH | libc::O_CLOEXEC, 0)
        {
            Ok(fd) => Ok(Some(fd)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("resolving {}", src.display())),
        }
    }

    /// Mount an already resolved source at dest.
    fn attach_source(&self, source: FsNode, src: &Path, dest: &Path, mode: BindMode) -> Result<()> {
        let is_dir = source.is_dir()?;

        // Matching bubblewrap's --bind/--dev-bind: nothing mounted in can be
        // setuid, and only `dev` binds may expose device nodes. `ro` also
        // makes the whole subtree read-only.
        let mut attrs = MOUNT_ATTR_NOSUID;
        if mode != BindMode::Dev {
            attrs |= MOUNT_ATTR_NODEV;
        }
        if mode == BindMode::Ro {
            attrs |= MOUNT_ATTR_RDONLY;
        }

        let target = self
            .ensure(dest, is_dir)
            .with_context(|| format!("preparing {}", dest.display()))?;

        fs::FsMount::clone_of(source.as_raw_fd())
            .with_context(|| format!("cloning {}", src.display()))?
            .setattr(attrs)
            .with_context(|| format!("setting mount attributes on {}", src.display()))?
            .attach_to(target.as_raw_fd())
            .with_context(|| format!("mounting {} on {}", src.display(), dest.display()))
    }

    /// Bind a host path onto another inside the new root. A missing source is
    /// an error: the caller asked for it by name.
    pub fn bind(
        &self,
        src: impl AsRef<Path>,
        dest: impl AsRef<Path>,
        mode: BindMode,
    ) -> Result<()> {
        let src = src.as_ref();

        let source = self
            .source(src)?
            .with_context(|| format!("{}: no such file or directory", src.display()))?;

        self.attach_source(source, src, dest.as_ref(), mode)
    }

    /// Bind a host path onto the same path inside the sandbox. A missing
    /// source is an error.
    pub fn bind_same(&self, src: impl AsRef<Path>, mode: BindMode) -> Result<()> {
        let src = src.as_ref();

        self.bind(src, src, mode)
    }

    /// The same, for sources that come and go with the host session -- an X11
    /// socket, /opt, a user share: a missing source is not an error.
    pub fn bind_try(
        &self,
        src: impl AsRef<Path>,
        dest: impl AsRef<Path>,
        mode: BindMode,
    ) -> Result<()> {
        let src = src.as_ref();

        let Some(source) = self.source(src)? else {
            return Ok(());
        };

        self.attach_source(source, src, dest.as_ref(), mode)
    }

    /// Bind a host path onto the same path inside the sandbox, which is what
    /// nearly every share does. A missing source is not an error.
    pub fn bind_same_try(&self, src: impl AsRef<Path>, mode: BindMode) -> Result<()> {
        let src = src.as_ref();

        self.bind_try(src, src, mode)
    }

    pub fn tmpfs(&self, dest: impl AsRef<Path>, opts: &str, attrs: u64) -> Result<()> {
        let dest = dest.as_ref();
        let mut context = fs::FsContext::new("tmpfs")?;

        for pair in opts.split(',').filter(|p| !p.is_empty()) {
            context = match pair.split_once('=') {
                Some((k, v)) => context.str(k, v)?,
                None => context.flag(pair)?,
            };
        }

        let target = self.ensure_dir(dest)?;

        context
            .mount(attrs)?
            .attach_to(target.as_raw_fd())
            .with_context(|| format!("mounting tmpfs on {}", dest.display()))
    }

    pub fn procfs(&self, dest: impl AsRef<Path>) -> Result<()> {
        let dest = dest.as_ref();

        let target = self.ensure_dir(dest)?;

        fs::FsContext::new("proc")?
            .mount(MOUNT_ATTR_NOSUID | MOUNT_ATTR_NOEXEC | MOUNT_ATTR_NODEV)?
            .attach_to(target.as_raw_fd())
            .with_context(|| format!("mounting proc on {}", dest.display()))?;

        // Defence in depth, the way bubblewrap does it: these are odd legacy
        // procfs subtrees, and a writable sysrq-trigger would let the sandbox
        // reboot the machine. The kernel already refuses such writes from a
        // user namespace -- proc_sys_permission() checks the caller's kernel
        // uid against the file mode and ignores capabilities granted in a
        // user namespace -- so this mask is kept for setups that run sbox as
        // real root. See readonly_rebind() for why it is skipped in the common
        // unprivileged case.
        for name in ["sys", "sysrq-trigger", "irq", "bus"] {
            let _ = self.readonly_rebind(dest.join(name));
        }

        Ok(())
    }

    /// Bind a subtree read-only onto itself. Returns false when the entry is
    /// already read-only or does not exist.
    ///
    /// The `faccessat(W_OK)` probe asks whether the *real* user may write the
    /// entry. For procfs sysctls the kernel checks the caller's kernel uid
    /// against the file mode and owner, ignoring capabilities granted in a
    /// user namespace, so an unprivileged run always skips the mask --
    /// correctly so, because the sandbox could not write those files either.
    /// The mask is applied when sbox runs as real root, which is the case it
    /// guards against.
    fn readonly_rebind(&self, path: impl AsRef<Path>) -> Result<bool> {
        let path = path.as_ref();

        let (parent, name) = split_last(path)?;
        let parent = self.ensure_dir(parent)?;

        if !parent.as_dir().accessible(&name, libc::W_OK)? {
            return Ok(false);
        }

        let tree = fs::FsMount::clone_from_at(parent.as_raw_fd(), &name)
            .with_context(|| format!("cloning {}", path.display()))?
            .setattr(MOUNT_ATTR_RDONLY)
            .with_context(|| format!("making {} read-only", path.display()))?;

        let target =
            parent.open_child(&name, libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0)?;

        tree.attach_to(target.as_raw_fd())
            .with_context(|| format!("covering {}", path.display()))?;

        Ok(true)
    }

    pub fn devpts(&self, dest: impl AsRef<Path>) -> Result<()> {
        let dest = dest.as_ref();
        let target = self.ensure_dir(dest)?;
        fs::FsContext::new("devpts")?
            .flag("newinstance")?
            .str("ptmxmode", "0666")?
            .str("mode", "0620")?
            .mount(MOUNT_ATTR_NOSUID | MOUNT_ATTR_NOEXEC)?
            .attach_to(target.as_raw_fd())
            .with_context(|| format!("mounting devpts on {}", dest.display()))
    }

    pub fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> Result<()> {
        let link = link.as_ref();

        let (parent, name) = split_last(link)?;
        let pfd = if parent.as_os_str().is_empty() {
            self.root.dup()?
        } else {
            self.ensure_dir(parent)?
        };

        pfd.as_dir().unlink(&name)?;
        pfd.as_dir()
            .symlink(target.as_ref(), &name)
            .with_context(|| {
                format!(
                    "symlink {} -> {}",
                    link.display(),
                    target.as_ref().display()
                )
            })
    }

    pub fn setup_dev(&self) -> Result<()> {
        // A private, minimal /dev. Device nodes cannot be created here: mknod
        // of a char/block device is refused inside a user namespace even with
        // CAP_MKNOD, so the handful that matter are bound in from the host.
        self.tmpfs("/dev", "mode=0755", MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV)?;

        for name in ["null", "zero", "full", "random", "urandom", "tty"] {
            self.bind_same_try(format!("/dev/{name}"), BindMode::Dev)?;
        }

        // Private pty instance. /dev/ptmx points into it so that ptys opened
        // inside the sandbox come from this devpts and not the host's.
        self.devpts("/dev/pts")?;
        self.symlink("pts/ptmx", "/dev/ptmx")?;

        // A terminal inherited from outside the sandbox has no node in this
        // devpts, so ttyname() fails on it. /dev/console is /dev/tty -- the
        // magic device that resolves to the controlling terminal of whoever
        // opens it -- so every session, present and future, gets a name for
        // its own terminal. bubblewrap does the same with the tty it was
        // started from, which a shared sandbox cannot know in advance.
        self.bind_try(Path::new("/dev/tty"), "/dev/console", BindMode::Dev)?;

        // Private shared memory and message queues.
        self.tmpfs(
            "/dev/shm",
            "mode=1777",
            MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV,
        )?;
        let _ = self.mqueue("/dev/mqueue");

        // /dev is ours, so these are safe to create.
        self.symlink("/proc/self/fd", "/dev/fd")?;
        self.symlink("/proc/self/fd/0", "/dev/stdin")?;
        self.symlink("/proc/self/fd/1", "/dev/stdout")?;
        self.symlink("/proc/self/fd/2", "/dev/stderr")?;
        // Legacy paths that docker and nspawn provide as well.
        self.symlink("/proc/kcore", "/dev/core")?;
        Ok(())
    }

    fn mqueue(&self, dest: impl AsRef<Path>) -> Result<()> {
        let dest = dest.as_ref();
        let target = self.ensure_dir(dest)?;
        fs::FsContext::new("mqueue")?
            .mount(MOUNT_ATTR_NOSUID | MOUNT_ATTR_NOEXEC | MOUNT_ATTR_NODEV)?
            .attach_to(target.as_raw_fd())
            .with_context(|| format!("mounting mqueue on {}", dest.display()))
    }

    /// Leave the sandbox on the tree it just built.
    ///
    /// The host root must not survive into the sandbox, so it is detached
    /// first. The second pivot then makes /newroot the root: pivot_root(".", ".")
    /// stacks the anchor on top of the new root, and the anchor descriptor is
    /// what selects it for the final unmount.
    pub fn finish(self) -> Result<()> {
        fs::umount(Path::new(OLD_ROOT), libc::MNT_DETACH).context("unmounting the old root")?;
        let _ = std::fs::remove_dir(OLD_ROOT);

        let anchor = FsNode::open(
            Path::new("/"),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
        )?;

        std::env::set_current_dir(NEW_ROOT).context("chdir /newroot")?;
        fs::pivot_root(Path::new("."), Path::new(".")).context("pivoting to the new root")?;

        nix::unistd::fchdir(&anchor).context("returning to the anchor")?;
        fs::umount(Path::new("."), libc::MNT_DETACH).context("unmounting the anchor")?;

        std::env::set_current_dir("/").context("chdir /")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::split_last;
    use std::path::Path;

    #[test]
    fn a_path_without_a_name_cannot_be_created() {
        assert!(split_last(Path::new("/")).is_err());
        assert!(split_last(Path::new("")).is_err());
        let (parent, name) = split_last(Path::new("/dev/ptmx")).expect("split");
        assert_eq!(parent, Path::new("/dev"));
        assert_eq!(name.to_string_lossy(), "ptmx");
    }
}
