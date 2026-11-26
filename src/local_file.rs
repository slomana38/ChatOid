use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    path::Path,
};

use zeroize::Zeroizing;

pub(crate) fn open_regular(path: &Path, follow_symlinks: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let symlink_flag = if follow_symlinks { 0 } else { libc::O_NOFOLLOW };
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | symlink_flag);
    }
    #[cfg(windows)]
    if !follow_symlinks {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Geçersiz dosya türü",
        ));
    }
    Ok(file)
}

pub(crate) fn read_bounded(file: &File, limit: usize) -> io::Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Dosya boyutu sınırı aşıldı",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn bounded_read_uses_open_descriptor_and_enforces_limit() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("file");
        fs::write(&path, b"1234")?;
        assert_eq!(&*read_bounded(&open_regular(&path, false)?, 4)?, b"1234");

        let file = open_regular(&path, false)?;
        fs::rename(&path, root.path().join("previous"))?;
        fs::write(&path, b"new file")?;
        assert_eq!(&*read_bounded(&file, 4)?, b"1234");

        let file = open_regular(&path, false)?;
        fs::write(&path, b"12345")?;
        assert_eq!(
            read_bounded(&file, 4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn refuses_fifo_without_waiting_for_writer() -> io::Result<()> {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        let root = tempfile::tempdir()?;
        let fifo = root.path().join("fifo");
        let name = CString::new(fifo.as_os_str().as_bytes())?;
        // `name` is a live, NUL-terminated filesystem path.
        if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        assert_eq!(
            open_regular(&fifo, true).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        Ok(())
    }
}
