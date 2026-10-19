//! AF_VSOCK client. libkrun forwards guest connections to CID 2 onto host
//! Unix sockets.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

pub const VMADDR_CID_HOST: u32 = 2;

pub fn connect(cid: u32, port: u32) -> io::Result<File> {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_cid = cid;
        addr.svm_port = port;
        let rc = libc::connect(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        );
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(File::from(fd))
    }
}

pub fn set_timeouts(f: &File, t: Option<Duration>) -> io::Result<()> {
    let tv = match t {
        Some(t) => libc::timeval { tv_sec: t.as_secs() as libc::time_t, tv_usec: t.subsec_micros() as libc::suseconds_t },
        None => libc::timeval { tv_sec: 0, tv_usec: 0 },
    };
    for opt in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        let rc = unsafe {
            libc::setsockopt(
                f.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                &tv as *const libc::timeval as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
