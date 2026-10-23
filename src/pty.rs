//! Pseudo-terminal that stands in for a modem serial port.

use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

pub struct Pty {
    pub master: File,
    /// Held open so the master never reads EIO while no client is attached.
    _slave: OwnedFd,
    pub slave_path: PathBuf,
}

pub fn open() -> io::Result<Pty> {
    let (mut m, mut s) = (0, 0);
    // SAFETY: openpty writes two fds; name/termios/winsize may be null.
    let rc = unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both fds were just returned by openpty and are owned here.
    let (master, slave) = unsafe { (File::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };

    let mut buf = [0 as libc::c_char; 128];
    // SAFETY: buf is valid for buf.len() bytes.
    if unsafe { libc::ttyname_r(s, buf.as_mut_ptr(), buf.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ttyname_r NUL-terminated buf on success.
    let slave_path = PathBuf::from(
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    );

    // SAFETY: termios is plain data; tcgetattr fills it before use.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(s, &mut t) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut t);
        if libc::tcsetattr(s, libc::TCSANOW, &t) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(Pty {
        master,
        _slave: slave,
        slave_path,
    })
}

/// Point `link` at `target`, replacing an existing symlink but never a real file.
pub fn link(target: &Path, link: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(link) {
        Ok(md) if md.file_type().is_symlink() => std::fs::remove_file(link)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a symlink", link.display()),
            ))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::os::unix::fs::symlink(target, link)
}

/// Remove `link` only if it is still our symlink to `target`.
pub fn unlink_if_ours(target: &Path, link: &Path) {
    if std::fs::read_link(link).ok().as_deref() == Some(target) {
        let _ = std::fs::remove_file(link);
    }
}

/// Wait up to `ms` for `fd` to become readable.
pub fn readable(fd: i32, ms: i32) -> io::Result<bool> {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: p is a valid pollfd array of length 1.
    let rc = unsafe { libc::poll(&mut p, 1, ms) };
    match rc {
        -1 => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e)
            }
        }
        0 => Ok(false),
        _ => Ok(true),
    }
}
