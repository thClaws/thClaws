//! Save an inbound `WsEnvelope::Upload` to the workspace.
//!
//! Mirrors the `--serve` `POST /upload` write path (see
//! `crate::server::serve_upload`): decode base64, run the bytes
//! through `crate::uploads::unique_path`, write, and return an
//! `UploadedFile` the caller hands to `render_upload_message`.
//!
//! Caller is responsible for the synth-message turn injection; this
//! module stays storage-only so it's pure (no `SharedSessionHandle`
//! dependency).

use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

use crate::uploads::{ensure_uploads_dir, write_unique, UploadedFile, UPLOADS_DIRNAME};

#[derive(Debug, thiserror::Error)]
pub enum SaveUploadError {
    #[error("cannot create uploads dir: {0}")]
    Mkdir(std::io::Error),
    #[error("base64 decode failed: {0}")]
    Decode(#[from] base64::DecodeError),
    #[error("declared size {declared} bytes != decoded {decoded} bytes")]
    SizeMismatch { declared: u64, decoded: u64 },
    #[error("write {0}: {1}")]
    Write(PathBuf, std::io::Error),
}

/// Decode + persist a single uploaded file under `<workspace>/_uploads/`.
/// Returns the [`UploadedFile`] descriptor the caller renders into
/// the synthetic chat turn via
/// [`crate::uploads::render_upload_message`]. `workspace` is captured
/// from the LINE session's start-time cwd (or injected by tests).
pub fn save_upload(
    workspace: &Path,
    filename: &str,
    content_b64: &str,
    declared_size: u64,
    media_type: Option<String>,
) -> Result<UploadedFile, SaveUploadError> {
    let bytes = B64.decode(content_b64.as_bytes())?;
    if bytes.len() as u64 != declared_size {
        return Err(SaveUploadError::SizeMismatch {
            declared: declared_size,
            decoded: bytes.len() as u64,
        });
    }
    let uploads_dir = ensure_uploads_dir(workspace).map_err(SaveUploadError::Mkdir)?;
    let dest = write_unique(&uploads_dir, filename, &bytes)
        .map_err(|e| SaveUploadError::Write(uploads_dir.join(filename), e))?;
    let relative_path = dest
        .strip_prefix(workspace)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| format!("{UPLOADS_DIRNAME}/{filename}"));
    Ok(UploadedFile {
        relative_path,
        media_type,
        size_bytes: bytes.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn save_upload_writes_file_and_returns_relative_path() {
        let td = tempdir().unwrap();
        let bytes = [1u8, 2, 3, 4, 5];
        let b64 = B64.encode(bytes);
        let saved = save_upload(
            td.path(),
            "hello.txt",
            &b64,
            bytes.len() as u64,
            Some("text/plain".into()),
        )
        .expect("save");
        assert_eq!(saved.relative_path, "_uploads/hello.txt");
        assert_eq!(saved.size_bytes, 5);
        assert_eq!(saved.media_type.as_deref(), Some("text/plain"));
        let disk = std::fs::read(td.path().join("_uploads/hello.txt")).unwrap();
        assert_eq!(disk, bytes);
    }

    #[test]
    fn save_upload_applies_underscore_n_suffix_on_collision() {
        let td = tempdir().unwrap();
        let bytes = [9u8, 9, 9];
        let b64 = B64.encode(bytes);
        let first = save_upload(td.path(), "note.pdf", &b64, 3, None).unwrap();
        let second = save_upload(td.path(), "note.pdf", &b64, 3, None).unwrap();
        assert_eq!(first.relative_path, "_uploads/note.pdf");
        assert_eq!(second.relative_path, "_uploads/note_1.pdf");
    }

    #[test]
    fn save_upload_rejects_size_mismatch() {
        let td = tempdir().unwrap();
        let b64 = B64.encode([1u8, 2, 3]);
        let err = save_upload(td.path(), "a.bin", &b64, 99, None).unwrap_err();
        assert!(matches!(err, SaveUploadError::SizeMismatch { .. }));
    }

    #[test]
    fn save_upload_rejects_invalid_base64() {
        let td = tempdir().unwrap();
        let err = save_upload(td.path(), "a.bin", "!!!not-base64!!!", 0, None).unwrap_err();
        assert!(matches!(err, SaveUploadError::Decode(_)));
    }
}
