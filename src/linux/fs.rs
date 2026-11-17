//! Thin wrappers around the modern mount API (Linux 5.12+).
//!
//! Everything here works on file descriptors instead of paths: a target is
//! resolved once, inside the sandbox root, and the mount is attached to that
//! descriptor. That removes the "create a path, then mount on it" dance and,
//! more importantly, lets symlinks in the sandbox tree be resolved *inside*
//! the tree instead of on the host.
//!
//! Errors are `std::io::Error`: the errno is what callers match on, and the
//! context that explains an operation belongs to the policy layer.

use std::ffi::{CStr, CString, OsString};
use std::io::{self, Result};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
pub(crate) const RESOLVE_IN_ROOT: u64 = 0x10;

const OPEN_TREE_CLONE: u32 = 0x1;

const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x4;
const MOVE_MOUNT_T_EMPTY_PATH: u32 = 0x40;

const FSOPEN_CLOEXEC: u32 = 0x1;
const FSMOUNT_CLOEXEC: u32 = 0x1;

const FSCONFIG_SET_FLAG: u32 = 0;
const FSCONFIG_SET_STRING: u32 = 1;
const FSCONFIG_CMD_CREATE: u32 = 6;

const AT_EMPTY_PATH: u32 = 0x1000;
const AT_RECURSIVE: u32 = 0x8000;

pub(crate) const MOUNT_ATTR_RDONLY: u64 = 0x1;
pub(crate) const MOUNT_ATTR_NOSUID: u64 = 0x2;
pub(crate) const MOUNT_ATTR_NODEV: u64 = 0x4;
pub(crate) const MOUNT_ATTR_NOEXEC: u64 = 0x8;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

fn ret_owned(ret: libc::c_long) -> Result<OwnedFd> {
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(unsafe { OwnedFd::from_raw_fd(ret as RawFd) })
}

fn cpath(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// The C string for a name handed to an *at() call: exactly one component, not
/// absolute. The mount tree walks paths itself and only ever passes one
/// component here, so a violation is a bug in the caller, not in the path.
fn cname(p: &Path) -> Result<CString> {
    if p.is_absolute() || p.components().count() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{}: expected a single path component, not a path",
                p.display()
            ),
        ));
    }

    cpath(p)
}

/// A node reached through an open descriptor: the handle every other file
/// operation is made against.
///
/// It carries no path, and its methods open or describe the node itself:
/// open_child reaches an entry below it, open_child_in_root resolves a whole
/// path inside its tree, and mode, is_dir, read_link and chmod describe what
/// it is. Operations that change an entry in place go through as_dir(), so
/// "this node" and "a child of this node" stay apart.
pub(crate) struct FsNode(OwnedFd);

impl FsNode {
    /// Open a path in the current namespace.
    pub fn open(path: &Path, flags: i32, mode: u32) -> Result<FsNode> {
        let path = cpath(path)?;

        let ret = unsafe { libc::open(path.as_ptr(), flags, mode) } as libc::c_long;
        Ok(FsNode(ret_owned(ret)?))
    }

    /// Open a whole path inside this node's tree.
    pub fn open_child_in_root(&self, path: &Path, flags: i32, mode: u32) -> Result<FsNode> {
        let path = cpath(path)?;
        let how = OpenHow {
            flags: flags as u64,
            mode: mode as u64,
            resolve: RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS,
        };

        let ret = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.as_raw_fd(),
                path.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };

        Ok(FsNode(ret_owned(ret)?))
    }

    /// Open one name below this node.
    pub fn open_child(&self, name: &Path, flags: i32, mode: u32) -> Result<FsNode> {
        let name = cname(name)?;

        let ret =
            unsafe { libc::openat(self.as_raw_fd(), name.as_ptr(), flags, mode) } as libc::c_long;
        Ok(FsNode(ret_owned(ret)?))
    }

    /// The st_mode of the node.
    pub fn mode(&self) -> Result<u32> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };

        if unsafe { libc::fstat(self.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(st.st_mode)
    }

    pub fn is_dir(&self) -> Result<bool> {
        Ok(self.mode()? & libc::S_IFMT == libc::S_IFDIR)
    }

    /// The target of the symbolic link this node refers to. The node has to
    /// have been opened with O_NOFOLLOW, so it names the link itself.
    pub fn read_link(&self) -> Result<PathBuf> {
        let mut buf = vec![0u8; 256];

        loop {
            let n = unsafe {
                libc::readlinkat(
                    self.as_raw_fd(),
                    c"".as_ptr(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                )
            };

            if n < 0 {
                return Err(io::Error::last_os_error());
            }

            // readlinkat truncates instead of failing, so a buffer that filled
            // completely may have cut the target short: grow and ask again.
            let n = n as usize;
            if n < buf.len() {
                buf.truncate(n);
                return Ok(PathBuf::from(OsString::from_vec(buf)));
            }

            buf.resize(buf.len() * 2, 0);
        }
    }

    /// The entries below this node, for the operations that change them. The
    /// node has to be a directory.
    pub fn as_dir(&self) -> FsDir<'_> {
        FsDir(self)
    }

    pub fn chmod(&self, mode: u32) -> Result<()> {
        if unsafe { libc::fchmod(self.as_raw_fd(), mode) } != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// A second descriptor for the same node.
    pub fn dup(&self) -> Result<FsNode> {
        let ret = unsafe { libc::fcntl(self.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };

        Ok(FsNode(ret_owned(ret as libc::c_long)?))
    }
}

/// The directory side of a node: what acts on the directory as a whole or on
/// the entries directly below it -- creating, removing and linking entries,
/// and reopening the directory itself.
///
/// Reading a child is a node operation: open_child reaches it and read_link
/// describes it. The node has to be a directory; the view is a marker, not a
/// check, because the caller has already resolved the node it is asking about.
pub(crate) struct FsDir<'a>(&'a FsNode);

impl FsDir<'_> {
    /// Create a directory and return a descriptor for it. An existing
    /// directory is not an error; a non-directory already sitting there is.
    pub fn mkdir(&self, name: &Path, mode: u32) -> Result<FsNode> {
        let cpath = cname(name)?;

        if unsafe { libc::mkdirat(self.0.as_raw_fd(), cpath.as_ptr(), mode) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EEXIST) {
                return Err(err);
            }
        }

        self.0.open_child(
            name,
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
    }

    /// Remove one name. A missing name is not an error.
    pub fn unlink(&self, name: &Path) -> Result<()> {
        let name = cname(name)?;

        if unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ENOENT) {
                return Err(err);
            }
        }

        Ok(())
    }

    pub fn symlink(&self, target: &Path, name: &Path) -> Result<()> {
        let target = cpath(target)?;
        let name = cname(name)?;

        if unsafe { libc::symlinkat(target.as_ptr(), self.0.as_raw_fd(), name.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// Whether the real user may write one name below the directory. Any
    /// refusal counts as "no", which is how the sandbox uses it: a probe, not
    /// a check.
    pub fn accessible(&self, name: &Path, mode: i32) -> Result<bool> {
        let name = cname(name)?;

        Ok(unsafe { libc::faccessat(self.0.as_raw_fd(), name.as_ptr(), mode, 0) } == 0)
    }

    /// Reopen the directory as a real (non-O_PATH) descriptor, which is what
    /// operations such as chmod need. Opening "." below the O_PATH descriptor
    /// yields a fresh descriptor for the same directory without a path lookup.
    pub fn reopen_self(&self) -> Result<FsNode> {
        self.0.open_child(
            Path::new("."),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
        )
    }
}

impl AsRawFd for FsNode {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl AsFd for FsNode {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// A filesystem context: what fsopen() returns. Options can be set here and
/// only here; mounting the context turns it into a detached tree.
pub(crate) struct FsContext(OwnedFd);

impl FsContext {
    /// Open a filesystem context for a type such as "tmpfs" or "proc".
    pub fn new(fs: &str) -> Result<FsContext> {
        let fs = CString::new(fs).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "fs kind contains a NUL byte")
        })?;

        let ret = unsafe { libc::syscall(libc::SYS_fsopen, fs.as_ptr(), FSOPEN_CLOEXEC) };

        Ok(FsContext(ret_owned(ret)?))
    }

    /// A "key=value" option.
    pub fn str(self, key: &str, value: &str) -> Result<FsContext> {
        let key = CString::new(key).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "attr key contains a NUL byte")
        })?;
        let value = CString::new(value).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "attr value contains a NUL byte",
            )
        })?;

        self.config(FSCONFIG_SET_STRING, Some(&key), Some(&value))?;

        Ok(self)
    }

    /// A bare flag such as "newinstance".
    pub fn flag(self, key: &str) -> Result<FsContext> {
        let key = CString::new(key).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "attr flag contains a NUL byte")
        })?;

        self.config(FSCONFIG_SET_FLAG, Some(&key), None)?;

        Ok(self)
    }

    /// Seal the context and create the detached mount.
    pub fn mount(self, attrs: u64) -> Result<FsMount> {
        // fsconfig(create): the key and value must both be NULL, and after
        // this no option can change the context.
        self.config(FSCONFIG_CMD_CREATE, None, None)?;

        let ret = unsafe {
            libc::syscall(
                libc::SYS_fsmount,
                self.0.as_raw_fd(),
                FSMOUNT_CLOEXEC,
                attrs,
            )
        };

        Ok(FsMount(ret_owned(ret)?))
    }

    /// fsconfig(2): the one call behind every option. The key is absent for
    /// the command that seals the context.
    fn config(&self, command: u32, key: Option<&CStr>, value: Option<&CStr>) -> Result<()> {
        let key = key.map_or(std::ptr::null(), |key| key.as_ptr());
        let value = value.map_or(std::ptr::null(), |value| value.as_ptr());

        let ret = unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                self.0.as_raw_fd(),
                command,
                key,
                value,
                0,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }
}

/// A prepared mount: a detached tree that only needs its attributes and a
/// place to be attached.
///
/// It comes either from an FsContext (a filesystem made from scratch, such as
/// tmpfs or proc) or from cloning an existing mount, which is the modern
/// equivalent of mount --bind.
pub(crate) struct FsMount(OwnedFd);

impl FsMount {
    /// Clone the mount at a path, whole subtree included.
    pub fn clone_from(path: &Path) -> Result<FsMount> {
        Self::clone_from_at(libc::AT_FDCWD, path)
    }

    /// The same, for a path relative to an already-open directory.
    pub fn clone_from_at(dirfd: RawFd, path: &Path) -> Result<FsMount> {
        Self::open_tree(dirfd, path, OPEN_TREE_CLONE | AT_RECURSIVE)
    }

    /// Clone the mount a descriptor refers to, whole subtree included. The
    /// descriptor is named with AT_EMPTY_PATH, so a source resolved inside the
    /// host root becomes a detached mount without a second path lookup.
    pub fn clone_of(fd: RawFd) -> Result<FsMount> {
        Self::open_tree(
            fd,
            Path::new(""),
            OPEN_TREE_CLONE | AT_RECURSIVE | AT_EMPTY_PATH,
        )
    }

    /// Open the mount at a path without cloning it: the descriptor refers to
    /// the existing mount, so its attributes can be changed in place.
    pub fn open_from(path: &Path) -> Result<FsMount> {
        Self::open_tree(libc::AT_FDCWD, path, 0)
    }

    fn open_tree(dirfd: RawFd, path: &Path, flags: u32) -> Result<FsMount> {
        let path = cpath(path)?;

        let ret = unsafe {
            libc::syscall(
                libc::SYS_open_tree,
                dirfd,
                path.as_ptr(),
                flags | libc::OPEN_TREE_CLOEXEC,
            )
        };

        Ok(FsMount(ret_owned(ret)?))
    }

    /// Apply mount attributes to the whole tree.
    pub fn setattr(self, attrs: u64) -> Result<FsMount> {
        self.set_attr(MountAttr {
            attr_set: attrs,
            attr_clr: 0,
            propagation: 0,
            userns_fd: 0,
        })
    }

    /// Make the mount, and everything below it, a slave of its parent: mounts
    /// made in this namespace no longer propagate to the host.
    pub fn make_slave(self) -> Result<()> {
        self.set_attr(MountAttr {
            attr_set: 0,
            attr_clr: 0,
            propagation: libc::MS_SLAVE,
            userns_fd: 0,
        })?;

        Ok(())
    }

    fn set_attr(self, attr: MountAttr) -> Result<FsMount> {
        let ret = unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                self.0.as_raw_fd(),
                c"".as_ptr(),
                AT_RECURSIVE | AT_EMPTY_PATH,
                &attr as *const MountAttr,
                std::mem::size_of::<MountAttr>(),
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(self)
    }

    /// Attach onto an already-open target directory.
    pub fn attach_to(self, target: RawFd) -> Result<()> {
        self.move_mount(
            target,
            c"",
            MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
        )
    }

    /// Attach at a path resolved by the kernel.
    pub fn attach_at(self, dirfd: RawFd, path: &Path) -> Result<()> {
        let path = cpath(path)?;
        self.move_mount(dirfd, &path, MOVE_MOUNT_F_EMPTY_PATH)
    }

    /// move_mount(2), with the source taken from the descriptor and the target
    /// either given as a descriptor or as a path.
    fn move_mount(self, to: RawFd, to_path: &CStr, flags: u32) -> Result<()> {
        let ret = unsafe {
            libc::syscall(
                libc::SYS_move_mount,
                self.0.as_raw_fd(),
                c"".as_ptr(),
                to,
                to_path.as_ptr(),
                flags,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }
}

pub(crate) fn pivot_root(new_root: &Path, put_old: &Path) -> Result<()> {
    let new_root = cpath(new_root)?;
    let put_old = cpath(put_old)?;

    let ret = unsafe { libc::syscall(libc::SYS_pivot_root, new_root.as_ptr(), put_old.as_ptr()) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

pub(crate) fn umount(path: &Path, flags: i32) -> Result<()> {
    let path = cpath(path)?;

    if unsafe { libc::umount2(path.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FsNode, cname};
    use std::path::Path;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("sbox-fsnode-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("inner")).expect("mkdir");
        path
    }

    #[test]
    fn children_are_named_one_component() {
        let dir = scratch("child");
        let node = FsNode::open(&dir, libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC, 0)
            .expect("open");

        assert!(node.open_child(Path::new("inner"), libc::O_PATH, 0).is_ok());
        assert!(
            node.open_child(Path::new("inner/x"), libc::O_PATH, 0)
                .is_err()
        );
        assert!(node.open_child(Path::new("/etc"), libc::O_PATH, 0).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_in_root_is_clamped_to_the_node() {
        let dir = scratch("root");
        let node = FsNode::open(&dir, libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC, 0)
            .expect("open");

        // ".." stops at the node, so this names dir/inner, not its parent.
        let inner = node
            .open_child_in_root(Path::new("inner/../inner"), libc::O_PATH, 0)
            .expect("resolve");
        assert!(inner.is_dir().expect("mode"));

        // An absolute path is resolved inside the node, not in the process root.
        assert!(
            node.open_child_in_root(Path::new("/etc"), libc::O_PATH, 0)
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_is_one_component() {
        assert!(cname(Path::new("dev")).is_ok());
        assert!(cname(Path::new("..")).is_ok());
        assert!(cname(Path::new("/etc/passwd")).is_err());
        assert!(cname(Path::new("etc/passwd")).is_err());
        assert!(cname(Path::new("")).is_err());
        assert!(cname(Path::new("./x")).is_err());
    }
}
