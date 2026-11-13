//! Who may connect to the service's sockets: the group and the mode, never the process's umask.
//!
//! A socket is reached by connecting, which needs write permission on the socket file. The
//! sockets are made with the group and mode the config names (`socket_group`, `socket_mode`), or
//! with owner and group only (`0660`, the process's own group) when the config names neither. The
//! directories that hold them get the traversal permission that group and other need to reach
//! the sockets.
//!
//! # How a socket gets its mode without a window
//!
//! `bind(2)` makes the socket with `0777 & !umask`, and std has no safe way to change the umask
//! (its only call is libc's `umask`, which this crate does not declare). So a socket is never
//! bound where it will be read. [`bind`] makes a private staging directory with mode `0700` (a
//! `DirBuilder` mode only ever loses bits to the umask, so it is never wider than `0700`), binds
//! the socket inside it, sets the configured mode and group, and renames the socket to its
//! final path. Until the rename, nobody but the service's user can reach the socket: the
//! directory's search bit is for the owner alone. The rename keeps the inode, so the listener
//! that was bound keeps working at the final path. Directories are made the same way, then
//! given the configured group and mode (`chown` and `set_permissions` ignore the umask).
//!
//! # Groups
//!
//! A group name is looked up in `/etc/group`, read directly (no libc lookup). A number is taken
//! as a group id as given, and the kernel decides at `chown` whether this process may use it.
//! A group that cannot be used is a start-up error naming the key.

use std::fs::{DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, chown};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::{Config, ConfigError, GroupSpec};

/// The socket mode when the config names none: owner and group only.
pub const DEFAULT_SOCKET_MODE: u32 = 0o660;

/// The group and modes the service's sockets and their directories get.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    /// The group id the sockets and directories are given.
    pub gid: u32,
    /// The group's name when `/etc/group` has one, else its number.
    pub group: String,
    /// The mode of every socket, as configured.
    pub socket_mode: u32,
    /// The mode of every directory that holds sockets (see [`dir_mode_for`]).
    pub dir_mode: u32,
}

impl Access {
    /// The access the config names. Reads `/etc/group` (for a name, and for a group's name).
    pub fn resolve(config: &Config) -> Result<Self, ConfigError> {
        let socket_mode = match config.socket_mode.as_deref() {
            None => DEFAULT_SOCKET_MODE,
            Some(text) => parse_mode(text).map_err(|message| ConfigError::Socket {
                key: "socket_mode",
                message,
            })?,
        };
        let groups = read_groups();
        let gid = match &config.socket_group {
            None => effective_gid().map_err(|e| ConfigError::Socket {
                key: "socket_group",
                message: format!("the process's own group is unknown: {e}"),
            })?,
            Some(GroupSpec::Gid(gid)) => *gid,
            Some(GroupSpec::Name(text)) if is_number(text) => {
                text.parse::<u32>().map_err(|_| ConfigError::Socket {
                    key: "socket_group",
                    message: format!("{text} is not a group id"),
                })?
            }
            Some(GroupSpec::Name(name)) => {
                let table = groups.as_ref().map_err(|e| ConfigError::Socket {
                    key: "socket_group",
                    message: format!("group {name:?} cannot be looked up: {e}"),
                })?;
                table
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, gid)| *gid)
                    .ok_or_else(|| ConfigError::Socket {
                        key: "socket_group",
                        message: format!("group {name:?} is not in /etc/group"),
                    })?
            }
        };
        let group = groups
            .ok()
            .and_then(|table| table.into_iter().find(|(_, g)| *g == gid))
            .map_or_else(|| gid.to_string(), |(name, _)| name);
        Ok(Self {
            gid,
            group,
            socket_mode,
            dir_mode: dir_mode_for(socket_mode),
        })
    }
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// The mode of a directory that holds sockets: owner `rwx`. Group and other get `r-x` when the
/// socket mode gives them read or write (so they can list and reach the sockets), and nothing
/// otherwise. `660` gives `750`, `666` gives `755`, `600` gives `700`.
pub const fn dir_mode_for(socket_mode: u32) -> u32 {
    let mut mode = 0o700;
    if socket_mode & 0o060 != 0 {
        mode |= 0o050;
    }
    if socket_mode & 0o006 != 0 {
        mode |= 0o005;
    }
    mode
}

/// Reads an octal mode such as `660`: one to four octal digits, at most `777`.
pub fn parse_mode(text: &str) -> Result<u32, String> {
    let well_formed =
        !text.is_empty() && text.len() <= 4 && text.bytes().all(|b| (b'0'..=b'7').contains(&b));
    if !well_formed {
        return Err(format!("{text:?} is not an octal mode such as \"660\""));
    }
    let mode = u32::from_str_radix(text, 8)
        .map_err(|_| format!("{text:?} is not an octal mode such as \"660\""))?;
    if mode > 0o777 {
        return Err(format!("{text:?} is more than \"777\""));
    }
    Ok(mode)
}

/// The groups in `/etc/group` text: each line `name:x:gid:members` gives `(name, gid)`. Lines
/// that do not parse are skipped.
pub fn parse_groups(text: &str) -> Vec<(String, u32)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let _password = fields.next()?;
            let gid = fields.next()?.parse::<u32>().ok()?;
            (!name.is_empty()).then(|| (name.to_string(), gid))
        })
        .collect()
}

fn read_groups() -> io::Result<Vec<(String, u32)>> {
    std::fs::read_to_string("/etc/group").map(|text| parse_groups(&text))
}

/// The process's effective group, from `/proc/self/status` (`Gid:` real, effective, saved, fs).
pub fn effective_gid() -> io::Result<u32> {
    let text = std::fs::read_to_string("/proc/self/status")?;
    text.lines()
        .find_map(|line| line.strip_prefix("Gid:"))
        .and_then(|rest| rest.split_ascii_whitespace().nth(1))
        .and_then(|gid| gid.parse::<u32>().ok())
        .ok_or_else(|| io::Error::other("no effective group in /proc/self/status"))
}

/// Numbers the staging directories, so two binds in one process never share one.
static STAGED: AtomicU64 = AtomicU64::new(0);

/// Makes a directory (and any missing parents) with the configured group and traversal mode.
/// An existing directory gets the same group and mode.
pub fn make_dir(dir: &Path, access: &Access) -> io::Result<()> {
    // Created no wider than the target under any umask; the exact mode is set below.
    DirBuilder::new()
        .recursive(true)
        .mode(access.dir_mode)
        .create(dir)?;
    chown(dir, None, Some(access.gid)).map_err(|e| group_error(dir, access, &e))?;
    std::fs::set_permissions(dir, Permissions::from_mode(access.dir_mode))
}

/// Binds a socket at `path` with the configured mode and group, and returns its listener. The
/// socket is never reachable at `path` with any other permissions (see the module's notes). The
/// parent directory must exist.
pub fn bind(path: &Path, access: &Access) -> io::Result<UnixListener> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other(format!("{} has no directory", path.display())))?;
    let file = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} has no file name", path.display())))?;
    let staging = dir.join(format!(
        ".bind-{}-{}",
        std::process::id(),
        STAGED.fetch_add(1, Ordering::Relaxed)
    ));
    // Owner only, under any umask: nobody else can reach the socket in here.
    DirBuilder::new().mode(0o700).create(&staging)?;
    let made = bind_inside(&staging.join(file), path, access);
    // The socket has moved out on success, so only the empty staging directory is left. On
    // failure this removes the socket too.
    let _ = std::fs::remove_dir_all(&staging);
    made
}

fn bind_inside(bound: &Path, path: &Path, access: &Access) -> io::Result<UnixListener> {
    let listener = UnixListener::bind(bound)?;
    chown(bound, None, Some(access.gid)).map_err(|e| group_error(path, access, &e))?;
    std::fs::set_permissions(bound, Permissions::from_mode(access.socket_mode))?;
    std::fs::rename(bound, path)?;
    Ok(listener)
}

fn group_error(path: &Path, access: &Access, e: &io::Error) -> io::Error {
    io::Error::new(
        e.kind(),
        format!(
            "socket_group: cannot give {} to group {} ({}): {e}; the service's user must be in that group",
            path.display(),
            access.group,
            access.gid,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_as_octal_and_nothing_wider_than_777_is_accepted() {
        assert_eq!(parse_mode("660"), Ok(0o660));
        assert_eq!(parse_mode("666"), Ok(0o666));
        assert_eq!(parse_mode("0640"), Ok(0o640));
        assert!(parse_mode("").is_err());
        assert!(parse_mode("668").is_err());
        assert!(parse_mode("1000").is_err());
        assert!(parse_mode("00660").is_err());
        assert!(parse_mode("rw").is_err());
    }

    #[test]
    fn traversal_follows_the_read_and_write_bits_of_the_socket_mode() {
        assert_eq!(dir_mode_for(0o660), 0o750);
        assert_eq!(dir_mode_for(0o666), 0o755);
        assert_eq!(dir_mode_for(0o600), 0o700);
        assert_eq!(dir_mode_for(0o640), 0o750);
        assert_eq!(dir_mode_for(0o604), 0o705);
    }

    #[test]
    fn the_group_file_is_read_by_name_and_number() {
        let text = "root:x:0:\nstyx:x:1001:alice\n# comment\nbad line\nnogid:x:x:\n";
        let groups = parse_groups(text);
        assert_eq!(
            groups,
            vec![("root".to_string(), 0), ("styx".to_string(), 1001)]
        );
    }
}
