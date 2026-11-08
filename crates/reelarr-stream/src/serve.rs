//! Serving a file over HTTP with byte ranges.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::range::{self, ByteRange};

const CHUNK: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("file not found")]
    NotFound,
    #[error("file is outside its library root")]
    OutsideRoot,
    #[error("not a regular file")]
    NotAFile,
}

/// Resolve symlinks and `..` in `path` and confirm the result is a regular
/// file inside `root`. Guards against a symlink or bad record pointing at
/// files outside the library.
pub async fn resolve_within(path: &Path, root: &Path) -> Result<PathBuf, PathError> {
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|_| PathError::NotFound)?;
    let resolved = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| PathError::NotFound)?;
    if !resolved.starts_with(&root) {
        return Err(PathError::OutsideRoot);
    }
    let meta = tokio::fs::metadata(&resolved)
        .await
        .map_err(|_| PathError::NotFound)?;
    if !meta.is_file() {
        return Err(PathError::NotAFile);
    }
    Ok(resolved)
}

/// Content type from the file extension.
pub fn mime_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("avi") => "video/x-msvideo",
        Some("ts" | "m2ts") => "video/mp2t",
        _ => "application/octet-stream",
    }
}

/// Serve `path` for a GET or HEAD request: 200, 206, 304 or 416.
pub async fn serve_file(path: &Path, mime: &str, method: &Method, headers: &HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let meta = match file.metadata().await {
        Ok(m) if m.is_file() => m,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let len = meta.len();
    let modified = meta.modified().ok();
    let mtime_nanos = modified
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let etag = format!("\"{len:x}-{mtime_nanos:x}\"");
    let last_modified = modified.map(httpdate::fmt_http_date);

    let mut base = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, &etag)
        .header(header::CACHE_CONTROL, "private, no-cache")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if let Some(lm) = &last_modified {
        base = base.header(header::LAST_MODIFIED, lm);
    }

    let header_str = |name| {
        headers
            .get(name)
            .and_then(|v: &HeaderValue| v.to_str().ok())
    };

    if header_str(header::IF_NONE_MATCH)
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
    {
        return base
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap();
    }

    // If-Range: only honor Range when the validator still matches.
    let range_allowed = match header_str(header::IF_RANGE) {
        None => true,
        Some(v) => v == etag || last_modified.as_deref() == Some(v),
    };
    let requested = if range_allowed {
        range::parse(header_str(header::RANGE), len)
    } else {
        ByteRange::Full
    };

    let (status, start, count) = match requested {
        ByteRange::Full => (StatusCode::OK, 0, len),
        ByteRange::Partial { start, end } => {
            base = base.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
            (StatusCode::PARTIAL_CONTENT, start, end - start + 1)
        }
        ByteRange::Unsatisfiable => {
            return base
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap();
        }
    };

    let base = base
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, count);
    if method == Method::HEAD {
        return base.body(Body::empty()).unwrap();
    }
    if start > 0 && file.seek(SeekFrom::Start(start)).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let body = Body::from_stream(ReaderStream::with_capacity(file.take(count), CHUNK));
    base.body(body).unwrap()
}
