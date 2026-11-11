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

impl Stat {
    /// An entry described by a bulk directory read. Ownership and mode are not
    /// requested there, so they are left unset; scans do not use them.
    pub fn known(known: Known) -> Self {
        let (identity, directory, allocated, links) = match known {
            Known::File(file) => (file.identity, false, file.allocated, file.links),
            Known::Directory(identity) => (identity, true, 0, 1),
        };
        Self {
            identity,
            directory,
            uid: u32::MAX,
            allocated,
            links,
            mode: if directory {
                libc::S_IFDIR
            } else {
                libc::S_IFREG
            },
        }
    }
}

pub(crate) struct Listed {
    pub name: OsString,
    pub known: Option<Known>,
}

/// What a bulk directory read established about an entry.
#[derive(Clone, Copy)]
pub(crate) enum Known {
    /// A regular file or symlink with its own block accounting.
    File(FileUsage),
    /// A plain directory: not a mount point, automount trigger or firmlink.
    /// Its identity must still be confirmed on the opened descriptor.
    Directory(Identity),
}

#[derive(Clone, Copy)]
pub(crate) struct FileUsage {
    pub identity: Identity,
    pub allocated: u64,
    pub links: u64,
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
        loop {
            // SAFETY: live parent descriptor and valid C string. Never follow a symlink.
            let fd = unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd >= 0 {
                return Ok(Self(unsafe { File::from_raw_fd(fd) }));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
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

    /// Entry names plus, where the kernel can supply them in bulk, what each
    /// entry is. Entries without that (mount points, firmlinks, anything
    /// unusual) must be inspected with [`Dir::entry`].
    ///
    /// Reads through this descriptor's own directory offset: call it once.
    #[cfg(target_os = "macos")]
    pub fn list(&self) -> io::Result<Vec<Listed>> {
        match bulk::list(self.0.as_raw_fd()) {
            Ok(entries) => Ok(entries),
            // Filesystems without bulk attribute support still list normally.
            Err(_) => self.names_only(),
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn list(&self) -> io::Result<Vec<Listed>> {
        self.names_only()
    }

    fn names_only(&self) -> io::Result<Vec<Listed>> {
        Ok(self
            .names()?
            .into_iter()
            .map(|name| Listed { name, known: None })
            .collect())
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

/// Width of the terminal on standard output, if it is one.
pub(crate) fn terminal_columns() -> Option<usize> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) } != 0 {
        return None;
    }
    let columns = unsafe { size.assume_init() }.ws_col as usize;
    (columns > 0).then_some(columns)
}

/// One mounted filesystem, as reported by the kernel without touching its files.
pub(crate) struct Mount {
    pub device: String,
    pub path: std::path::PathBuf,
    pub filesystem: String,
    pub size: u64,
    pub used: u64,
    pub available: u64,
}

#[cfg(target_os = "macos")]
pub(crate) fn mounts() -> io::Result<Vec<Mount>> {
    fn text(field: &[libc::c_char]) -> String {
        let bytes: Vec<u8> = field
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
    let mut list: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo stores a pointer to a libc-owned array of `count` entries.
    let count = unsafe { libc::getmntinfo(&mut list, libc::MNT_NOWAIT) };
    if count <= 0 || list.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the array is valid until the next getmntinfo call; copy it out now.
    let entries = unsafe { std::slice::from_raw_parts(list, count as usize) };
    Ok(entries
        .iter()
        .map(|entry| {
            let block = entry.f_bsize as u64;
            Mount {
                device: text(&entry.f_mntfromname),
                path: text(&entry.f_mntonname).into(),
                filesystem: text(&entry.f_fstypename),
                size: entry.f_blocks.saturating_mul(block),
                used: entry
                    .f_blocks
                    .saturating_sub(entry.f_bfree)
                    .saturating_mul(block),
                available: entry.f_bavail.saturating_mul(block),
            }
        })
        .collect())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn mounts() -> io::Result<Vec<Mount>> {
    Ok(vec![])
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

/// Space used by the one volume mounted at `path`. APFS volumes share a
/// container, so mount-table block counts describe the container instead.
#[cfg(target_os = "macos")]
pub(crate) fn volume_used(path: &Path) -> io::Result<u64> {
    #[repr(C, packed(4))]
    struct Reply {
        length: u32,
        used: i64,
    }
    let path = cstring(path.as_os_str())?;
    let mut request = AttrList {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_SPACEUSED,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut reply = Reply { length: 0, used: 0 };
    // SAFETY: valid C string; request and reply are live, correctly sized buffers.
    let status = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&raw mut request).cast(),
            (&raw mut reply).cast(),
            size_of::<Reply>(),
            0,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    if (reply.length as usize) < size_of::<Reply>() {
        return Err(io::Error::other("volume did not report its used space"));
    }
    Ok(reply.used.max(0) as u64)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn volume_used(_: &Path) -> io::Result<u64> {
    Err(io::Error::other("per-volume usage is unavailable"))
}

/// `getattrlistbulk`: one system call returns names and attributes for many
/// entries, replacing a `readdir` plus one `fstatat` per file.
#[cfg(target_os = "macos")]
mod bulk {
    use super::{AttrList, FileUsage, Identity, Known, Listed};
    use std::{ffi::OsString, io, os::unix::ffi::OsStringExt};

    const ATTR_CMN_ERROR: u32 = 0x2000_0000;
    const ATTR_CMN_FLAGS: u32 = 0x0004_0000;
    const ATTR_DIR_MOUNTSTATUS: u32 = 0x0000_0004;
    const SF_FIRMLINK: u32 = 0x0080_0000;
    const VREG: u32 = 1;
    const VDIR: u32 = 2;
    const VLNK: u32 = 5;
    /// Each record starts with the common attributes in ascending bit order
    /// (the error first). What follows depends on the entry's type: a
    /// directory carries only the directory attributes and anything else only
    /// the file attributes, so both groups begin at the same offset.
    const LENGTH: usize = 0;
    const RETURNED_COMMON: usize = 4;
    const RETURNED_DIR: usize = 12;
    const RETURNED_FILE: usize = 16;
    const ERROR: usize = 24;
    const NAME: usize = 28;
    const DEVICE: usize = 36;
    const TYPE: usize = 40;
    const FLAGS: usize = 44;
    const FILE_ID: usize = 48;
    const COMMON_END: usize = 56;
    const MOUNT_STATUS: usize = COMMON_END;
    const DIRECTORY_END: usize = COMMON_END + 4;
    const LINKS: usize = COMMON_END;
    const ALLOCATED: usize = COMMON_END + 4;
    const FILE_END: usize = COMMON_END + 12;

    fn u32_at(record: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(record[at..at + 4].try_into().unwrap())
    }
    fn u64_at(record: &[u8], at: usize) -> u64 {
        u64::from_ne_bytes(record[at..at + 8].try_into().unwrap())
    }

    pub(super) fn list(fd: libc::c_int) -> io::Result<Vec<Listed>> {
        let common = libc::ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | ATTR_CMN_FLAGS
            | libc::ATTR_CMN_FILEID;
        let file = libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE;
        let mut request = AttrList {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: common,
            volattr: 0,
            dirattr: ATTR_DIR_MOUNTSTATUS,
            fileattr: file,
            forkattr: 0,
        };
        // One reusable buffer per worker thread: allocating and zeroing it for
        // every directory would cost more than the calls it replaces. u64
        // storage gives the 8-byte alignment each record's length field has.
        thread_local! {
            static BUFFER: std::cell::RefCell<Vec<u64>> =
                std::cell::RefCell::new(vec![0u64; 4 * 1024]);
        }
        BUFFER.with_borrow_mut(|buffer| read(fd, &mut request, buffer, common, file))
    }

    fn read(
        fd: libc::c_int,
        request: &mut AttrList,
        buffer: &mut [u64],
        common: u32,
        file: u32,
    ) -> io::Result<Vec<Listed>> {
        let size = buffer.len() * 8;
        let mut entries = Vec::new();
        loop {
            // SAFETY: live descriptor; request and buffer outlive the call and
            // the kernel writes at most `size` bytes.
            let count = unsafe {
                libc::getattrlistbulk(
                    fd,
                    std::ptr::from_mut(request).cast(),
                    buffer.as_mut_ptr().cast(),
                    size,
                    libc::FSOPT_PACK_INVAL_ATTRS as u64,
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if count == 0 {
                return Ok(entries);
            }
            // SAFETY: the buffer is initialised and `size` bytes long.
            let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), size) };
            let mut at = 0;
            for _ in 0..count {
                let malformed = || io::Error::other("malformed directory attribute record");
                let head = bytes.get(at..at + COMMON_END).ok_or_else(malformed)?;
                let length = u32_at(head, LENGTH) as usize;
                let record = bytes
                    .get(at..at + length)
                    .filter(|_| length >= COMMON_END)
                    .ok_or_else(malformed)?;
                // The name reference is relative to its own position.
                let offset = i32::from_ne_bytes(record[NAME..NAME + 4].try_into().unwrap());
                let name_length = u32_at(record, NAME + 4) as usize;
                let start = NAME
                    .checked_add_signed(offset as isize)
                    .ok_or_else(malformed)?;
                let name = record
                    .get(start..start + name_length)
                    .ok_or_else(malformed)?;
                let name = name.split(|b| *b == 0).next().unwrap_or(name);
                let identity = Identity {
                    dev: u32_at(record, DEVICE) as i32 as u64,
                    ino: u64_at(record, FILE_ID),
                };
                // The kernel reports the bookkeeping bits inconsistently, so
                // require only the attributes that carry values.
                let needed = common & !(ATTR_CMN_ERROR | libc::ATTR_CMN_RETURNED_ATTRS);
                let common_ok = u32_at(record, ERROR) == 0
                    && u32_at(record, RETURNED_COMMON) & needed == needed;
                let known = match u32_at(record, TYPE) {
                    VREG | VLNK
                        if common_ok
                            && length >= FILE_END
                            && u32_at(record, RETURNED_FILE) & file == file =>
                    {
                        Some(Known::File(FileUsage {
                            identity,
                            allocated: (u64_at(record, ALLOCATED) as i64).max(0) as u64,
                            links: u32_at(record, LINKS) as u64,
                        }))
                    }
                    // For a mount point or firmlink these attributes describe
                    // the covered directory, not what an open would reach.
                    VDIR if common_ok
                        && length >= DIRECTORY_END
                        && u32_at(record, RETURNED_DIR) & ATTR_DIR_MOUNTSTATUS != 0
                        && u32_at(record, MOUNT_STATUS) == 0
                        && u32_at(record, FLAGS) & SF_FIRMLINK == 0 =>
                    {
                        Some(Known::Directory(identity))
                    }
                    _ => None,
                };
                if name != b"." && name != b".." && !name.is_empty() {
                    entries.push(Listed {
                        name: OsString::from_vec(name.to_vec()),
                        known,
                    });
                }
                at += length;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The bulk listing is an optimisation: whatever it claims about an entry
    /// must equal what inspecting that entry individually reports.
    #[test]
    fn listing_agrees_with_individual_inspection() {
        let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(root.join("data"), vec![7u8; 300_000]).unwrap();
        fs::write(root.join("empty"), []).unwrap();
        fs::hard_link(root.join("data"), root.join("alias")).unwrap();
        fs::create_dir(root.join("folder")).unwrap();
        fs::write(root.join("folder/inner"), b"x").unwrap();
        std::os::unix::fs::symlink("data", root.join("link")).unwrap();
        std::os::unix::fs::symlink("missing", root.join("dangling")).unwrap();
        for index in 0..700 {
            fs::write(
                root.join(format!("many-{index:04}-{}", "n".repeat(40))),
                b"y",
            )
            .unwrap();
        }

        let dir = Dir::open(&root).unwrap();
        let listing = dir.list().unwrap();
        let mut names: Vec<_> = listing.iter().map(|l| l.name.clone()).collect();
        names.sort();
        let mut expected = Dir::open(&root).unwrap().names().unwrap();
        expected.sort();
        assert_eq!(names, expected, "every entry is listed exactly once");

        for listed in &listing {
            let direct = dir.entry(&listed.name).unwrap();
            if let Some(known) = listed.known {
                let claimed = Stat::known(known);
                assert_eq!(claimed.identity, direct.identity, "{:?}", listed.name);
                assert_eq!(claimed.directory, direct.directory, "{:?}", listed.name);
                if !direct.directory {
                    assert_eq!(claimed.allocated, direct.allocated, "{:?}", listed.name);
                    assert_eq!(claimed.links, direct.links, "{:?}", listed.name);
                }
            }
            // On macOS the fast path must actually cover ordinary entries.
            #[cfg(target_os = "macos")]
            assert!(
                listed.known.is_some(),
                "{:?} was not described",
                listed.name
            );
        }
    }
}
