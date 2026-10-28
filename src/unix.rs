//! The only unsafe module. Every descent opens one component with O_NOFOLLOW.
//! Handles prevent symlink redirection, but cannot prevent concurrent renames.
use std::{
    ffi::{CString, OsString},
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Component, Path},
};

pub(crate) struct Dir(File);

#[derive(Clone, Copy, Debug, serde::Serialize, PartialEq, Eq)]
pub(crate) struct Identity {
    pub dev: u64,
    pub ino: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct Stat {
    pub identity: Identity,
    pub directory: bool,
    pub uid: u32,
    pub allocated: u64,
    pub links: u64,
    pub mode: libc::mode_t,
}

fn cstring(name: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

fn stat(value: libc::stat) -> Stat {
    Stat {
        identity: Identity {
            dev: value.st_dev as u64,
            ino: value.st_ino,
        },
        directory: value.st_mode & libc::S_IFMT == libc::S_IFDIR,
        uid: value.st_uid,
        allocated: (value.st_blocks.max(0) as u64).saturating_mul(512),
        links: value.st_nlink as u64,
        mode: value.st_mode,
    }
}

impl Dir {
    pub fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected absolute path",
            ));
        }
        let root = CString::new("/").unwrap();
        // SAFETY: valid C string; ownership of a successful descriptor transfers to File.
        let fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut dir = Self(unsafe { File::from_raw_fd(fd) });
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => dir = dir.child(name)?,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "non-normal path component",
                    ));
                }
            }
        }
        Ok(dir)
    }

    pub fn child(&self, name: &std::ffi::OsStr) -> io::Result<Self> {
        let name = cstring(name)?;
        // SAFETY: live parent descriptor and valid C string. Never follow a symlink.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(unsafe { File::from_raw_fd(fd) }))
        }
    }

    pub fn stat(&self) -> io::Result<Stat> {
        let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: output buffer has the correct layout; read only after success.
        if unsafe { libc::fstat(self.0.as_raw_fd(), value.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat(unsafe { value.assume_init() }))
    }

    pub fn entry(&self, name: &std::ffi::OsStr) -> io::Result<Stat> {
        let name = cstring(name)?;
        let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: valid descriptor, string and output buffer. Inspect links themselves.
        if unsafe {
            libc::fstatat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(stat(unsafe { value.assume_init() }))
    }

    pub fn names(&self) -> io::Result<Vec<OsString>> {
        // Opening '.' gives an independent directory offset; dup would share offsets.
        let dot = CString::new(".").unwrap();
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let stream = Stream(stream);
        let mut names = Vec::new();
        loop {
            // POSIX readdir distinguishes end-of-stream from errors using errno.
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0;
            }
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error().unwrap_or(0) != 0 {
                    return Err(error);
                }
                break;
            }
            // SAFETY: readdir returned a live entry; copy its name before the next call.
            let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes != b"." && bytes != b".." {
                names.push(OsString::from_vec(bytes.to_vec()));
            }
        }
        Ok(names)
    }

    pub fn unlink(&self, name: &std::ffi::OsStr, directory: bool) -> io::Result<()> {
        let name = cstring(name)?;
        let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
        if unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), flags) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[allow(clippy::unnecessary_cast)] // statvfs field widths differ across Unix ABIs.
    pub fn free_bytes(&self) -> io::Result<u64> {
        let mut value = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::fstatvfs(self.0.as_raw_fd(), value.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let value = unsafe { value.assume_init() };
        Ok((value.f_bavail as u64).saturating_mul(value.f_frsize as u64))
    }

    pub fn cache_access(&self) -> io::Result<()> {
        let stat = self.stat()?;
        if stat.uid != uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cache directory is not user-owned",
            ));
        }
        if stat.mode & 0o700 != 0o700
            && unsafe { libc::fchmod(self.0.as_raw_fd(), stat.mode | 0o700) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

pub(crate) fn uid() -> u32 {
    unsafe { libc::getuid() }
}
