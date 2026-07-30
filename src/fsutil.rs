// SPDX-License-Identifier: MIT

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
};

/// Atomically replace `destination` with `source` on the same filesystem.
pub(crate) fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    replace_file_impl(source, destination)
}

/// Move `source` to `destination` without replacing an existing destination.
/// The operation is atomic on supported local filesystems and maps a race with
/// an existing target to `ErrorKind::AlreadyExists`.
pub(crate) fn move_file_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    move_file_noreplace_impl(source, destination)
}

/// Write a small metadata file durably before atomically replacing its target.
pub(crate) fn write_atomic(destination: &Path, bytes: &[u8]) -> io::Result<()> {
    let (temporary, mut file) = create_unique_temporary(destination)?;
    let write_result = (|| {
        file.write_all(bytes)?;
        file.sync_all()
    })();
    // Close the source before rename/cleanup; Windows otherwise rejects both
    // operations when a custom share mode does not allow deletion.
    drop(file);
    let result = write_result.and_then(|()| replace_file(&temporary, destination));

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_unique_temporary(destination: &Path) -> io::Result<(PathBuf, File)> {
    destination.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic-write destination has no filename",
        )
    })?;

    // create_new makes a guessed/colliding path fail instead of following a
    // symlink. UUID collisions are implausible, but retry a few times so an
    // existing crash artifact cannot make the write fail. Keep the temporary
    // component independent of the destination name: download sidecars already
    // approach common 255-byte component limits.
    for _ in 0..16 {
        let temporary_name = format!(".rgfile-tmp-{}.tmp", uuid::Uuid::new_v4().simple());
        let temporary = destination.with_file_name(temporary_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique atomic-write temporary file",
    ))
}

#[cfg(not(windows))]
fn replace_file_impl(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(target_os = "linux")]
fn move_file_noreplace_impl(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

    const RENAME_NOREPLACE: libc::c_uint = 1;
    let source_path = source.to_owned();
    let destination_path = destination.to_owned();
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains a NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination path contains a NUL",
        )
    })?;
    // SAFETY: both C strings remain alive for the syscall and point to valid
    // NUL-terminated path bytes; AT_FDCWD selects the process working tree.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    let unsupported = error.raw_os_error().is_some_and(|code| {
        code == libc::ENOSYS
            || code == libc::EINVAL
            || code == libc::EOPNOTSUPP
            || code == libc::ENOTSUP
    });
    if unsupported {
        // Older kernels/filesystems may not implement renameat2. A hard link
        // still provides create-without-replace semantics on those systems.
        return hard_link_move_noreplace(&source_path, &destination_path);
    }
    Err(error)
}

#[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos"), not(windows)))]
fn move_file_noreplace_impl(source: &Path, destination: &Path) -> io::Result<()> {
    hard_link_move_noreplace(source, destination)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn hard_link_move_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::hard_link(source, destination)?;
    if let Err(error) = fs::remove_file(source) {
        // The destination has already been committed without replacing
        // anything. Never try to roll it back by name: another process with
        // directory write access could replace that name between the failed
        // source unlink and a rollback unlink. Leaving the source hard link is
        // a recoverable cleanup artifact and preserves truthful commit
        // semantics.
        tracing::warn!(
            source = ?source,
            destination = ?destination,
            %error,
            "source cleanup failed after a no-replace move was committed"
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn move_file_noreplace_impl(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

    const RENAME_EXCL: libc::c_uint = 0x0004;
    unsafe extern "C" {
        fn renamex_np(
            from: *const libc::c_char,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains a NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination path contains a NUL",
        )
    })?;
    // SAFETY: paths are valid NUL-terminated strings for the duration of call.
    let result = unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn move_file_noreplace_impl(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains a NUL character",
            ));
        }
        wide.push(0);
        Ok(wide)
    }
    let source = wide_path(source)?;
    let destination = wide_path(destination)?;
    // Without MOVEFILE_REPLACE_EXISTING, Windows keeps an existing target and
    // reports ERROR_ALREADY_EXISTS, providing the required no-replace race.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_destination_without_leaving_temporary_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let destination = temp.path().join("file.part.json");
        fs::write(&destination, b"old").unwrap();

        write_atomic(&destination, b"new").unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn move_noreplace_preserves_a_late_existing_destination() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::write(&source, b"new").unwrap();
        fs::write(&destination, b"old").unwrap();

        let error = move_file_noreplace(&source, &destination).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&source).unwrap(), b"new");
        assert_eq!(fs::read(&destination).unwrap(), b"old");
    }

    #[test]
    fn atomic_write_cleans_temporary_file_when_replace_fails() {
        let temp = tempfile::TempDir::new().unwrap();
        let destination = temp.path().join("destination");
        fs::create_dir(&destination).unwrap();

        assert!(write_atomic(&destination, b"new").is_err());

        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], destination.file_name().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_supports_destination_near_component_length_limit() {
        let temp = tempfile::TempDir::new().unwrap();
        let destination = temp.path().join(format!("{}.json", "a".repeat(245)));
        assert_eq!(
            destination.file_name().unwrap().as_encoded_bytes().len(),
            250
        );

        write_atomic(&destination, b"sidecar").unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"sidecar");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_sidecar_write_does_not_follow_legacy_temporary_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let destination = temp.path().join("file.part.json");
        let legacy_temporary = temp.path().join("file.part.json.tmp");
        let victim = temp.path().join("victim");
        fs::write(&victim, b"keep me").unwrap();
        symlink(&victim, &legacy_temporary).unwrap();

        write_atomic(&destination, b"sidecar").unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"sidecar");
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
        assert!(
            fs::symlink_metadata(&legacy_temporary)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_sidecar_write_replaces_destination_symlink_not_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let destination = temp.path().join("file.part.json");
        let victim = temp.path().join("victim");
        fs::write(&victim, b"keep me").unwrap();
        symlink(&victim, &destination).unwrap();

        write_atomic(&destination, b"sidecar").unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"sidecar");
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
        assert!(
            !fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[cfg(windows)]
fn replace_file_impl(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains a NUL character",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    let source = wide_path(source)?;
    let destination = wide_path(destination)?;
    // The slices are NUL-terminated and remain alive for the duration of the call.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
