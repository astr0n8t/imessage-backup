//! Serving attachment files, with HTTP range support (needed for video
//! seeking, especially in Safari) and optional transcoding of formats
//! browsers cannot show (HEIC photos, CAF/AMR voice messages) via external
//! tools when they are installed.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

#[derive(Debug, Clone, Default)]
pub struct Tools {
    pub heif_convert: Option<PathBuf>,
    pub magick: Option<PathBuf>,
    pub sips: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

impl Tools {
    pub fn detect() -> Self {
        Self {
            heif_convert: which("heif-convert"),
            magick: which("magick"),
            sips: which("sips"),
            ffmpeg: which("ffmpeg"),
        }
    }

    pub fn describe(&self) -> String {
        let mut v = vec![];
        if self.heif_convert.is_some() || self.magick.is_some() || self.sips.is_some() {
            v.push("HEIC→JPEG");
        }
        if self.ffmpeg.is_some() {
            v.push("CAF/AMR→M4A");
        }
        if v.is_empty() {
            "none (install libheif-examples / ffmpeg to enable)".into()
        } else {
            v.join(", ")
        }
    }
}

pub fn guess_mime(mime: Option<&str>, name: Option<&str>) -> String {
    if let Some(m) = mime.filter(|m| m.contains('/') && !m.starts_with("uti:")) {
        return m.to_lowercase();
    }
    let ext = name
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "mov" => "video/quicktime",
        "mp4" | "m4v" => "video/mp4",
        "m4a" => "audio/mp4",
        "mp3" => "audio/mpeg",
        "caf" => "audio/x-caf",
        "amr" => "audio/amr",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "vcf" => "text/vcard",
        _ => "application/octet-stream",
    }
    .into()
}

/// Types we let the browser render inline. Anything else (HTML, SVG, PDFs…)
/// is forced to download so attachment content can never run in our origin.
fn inline_ok(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/heic" | "image/heif"
    ) || mime.starts_with("video/")
        || mime.starts_with("audio/")
}

enum Conversion {
    Jpeg,
    M4a,
}

fn conversion_for(mime: &str, tools: &Tools) -> Option<Conversion> {
    match mime {
        "image/heic" | "image/heif"
            if tools.heif_convert.is_some() || tools.magick.is_some() || tools.sips.is_some() =>
        {
            Some(Conversion::Jpeg)
        }
        "audio/x-caf" | "audio/amr" | "audio/amr-wb" if tools.ffmpeg.is_some() => {
            Some(Conversion::M4a)
        }
        _ => None,
    }
}

async fn run(cmd: &Path, args: &[&std::ffi::OsStr]) -> bool {
    tokio::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Convert `src` (with a hint extension) into `dest`, caching the result.
async fn convert(
    src: &Path,
    file_id: &str,
    conv: &Conversion,
    tools: &Tools,
    cache: &Path,
) -> Option<PathBuf> {
    let (ext, src_ext) = match conv {
        Conversion::Jpeg => ("jpg", "heic"),
        Conversion::M4a => ("m4a", "caf"),
    };
    let dest = cache.join(format!("{file_id}.{ext}"));
    if dest.is_file() {
        return Some(dest);
    }
    tokio::fs::create_dir_all(cache).await.ok()?;
    let nonce: u64 = rand::random();
    // Some tools pick the decoder from the extension, so give the input one.
    let input = cache.join(format!("in-{file_id}-{nonce}.{src_ext}"));
    #[cfg(unix)]
    std::os::unix::fs::symlink(src, &input).ok()?;
    #[cfg(not(unix))]
    std::fs::copy(src, &input).ok()?;
    let tmp = cache.join(format!("out-{file_id}-{nonce}.{ext}"));
    let (i, o) = (input.as_os_str(), tmp.as_os_str());
    let ok = match conv {
        Conversion::Jpeg => {
            let mut ok = false;
            if let Some(t) = &tools.heif_convert {
                ok = run(t, &["-q".as_ref(), "85".as_ref(), i, o]).await;
            }
            if !ok && let Some(t) = &tools.magick {
                ok = run(t, &[i, "-quality".as_ref(), "85".as_ref(), o]).await;
            }
            if !ok && let Some(t) = &tools.sips {
                ok = run(
                    t,
                    &[
                        "-s".as_ref(),
                        "format".as_ref(),
                        "jpeg".as_ref(),
                        i,
                        "--out".as_ref(),
                        o,
                    ],
                )
                .await;
            }
            ok
        }
        Conversion::M4a => {
            let t = tools.ffmpeg.as_ref()?;
            run(
                t,
                &[
                    "-y".as_ref(),
                    "-loglevel".as_ref(),
                    "error".as_ref(),
                    "-i".as_ref(),
                    i,
                    "-c:a".as_ref(),
                    "aac".as_ref(),
                    "-b:a".as_ref(),
                    "96k".as_ref(),
                    o,
                ],
            )
            .await
        }
    };
    let _ = tokio::fs::remove_file(&input).await;
    if ok && tmp.is_file() && tokio::fs::rename(&tmp, &dest).await.is_ok() {
        Some(dest)
    } else {
        let _ = tokio::fs::remove_file(&tmp).await;
        None
    }
}

fn parse_range(h: &str, len: u64) -> Option<(u64, u64)> {
    let spec = h.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (len.saturating_sub(n), len.checked_sub(1)?)
    } else {
        let s: u64 = a.parse().ok()?;
        let e = if b.is_empty() {
            len.checked_sub(1)?
        } else {
            b.parse::<u64>().ok()?.min(len.checked_sub(1)?)
        };
        (s, e)
    };
    (start <= end && end < len).then_some((start, end))
}

fn safe_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".-_ ".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub struct ServeRequest<'a> {
    pub path: PathBuf,
    pub file_id: &'a str,
    pub mime: Option<&'a str>,
    pub name: Option<&'a str>,
    pub download: bool,
    pub raw: bool,
}

pub async fn serve(
    req: ServeRequest<'_>,
    headers: &HeaderMap,
    tools: &Tools,
    cache: &Path,
) -> Response {
    let mut mime = guess_mime(req.mime, req.name);
    let mut path = req.path.clone();
    let mut name = req
        .name
        .map(safe_filename)
        .unwrap_or_else(|| req.file_id.to_owned());
    if !req.raw
        && let Some(conv) = conversion_for(&mime, tools)
        && let Some(converted) = convert(&req.path, req.file_id, &conv, tools, cache).await
    {
        path = converted;
        let (m, ext) = match conv {
            Conversion::Jpeg => ("image/jpeg", "jpg"),
            Conversion::M4a => ("audio/mp4", "m4a"),
        };
        mime = m.into();
        name = format!(
            "{}.{ext}",
            name.rsplit_once('.').map(|(b, _)| b).unwrap_or(&name)
        );
    }

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                "attachment file not found in archive",
            )
                .into_response();
        }
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let disposition = if req.download || !inline_ok(&mime) {
        "attachment"
    } else {
        "inline"
    };
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    resp_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    resp_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    resp_headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    resp_headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("{disposition}; filename=\"{name}\"")) {
        resp_headers.insert(header::CONTENT_DISPOSITION, v);
    }

    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    if let Some(r) = range {
        let Some((start, end)) = parse_range(r, len) else {
            resp_headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{len}")).unwrap(),
            );
            return (StatusCode::RANGE_NOT_SATISFIABLE, resp_headers).into_response();
        };
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let n = end - start + 1;
        resp_headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")).unwrap(),
        );
        resp_headers.insert(header::CONTENT_LENGTH, HeaderValue::from(n));
        let body = Body::from_stream(ReaderStream::new(file.take(n)));
        return (StatusCode::PARTIAL_CONTENT, resp_headers, body).into_response();
    }
    resp_headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    (
        StatusCode::OK,
        resp_headers,
        Body::from_stream(ReaderStream::new(file)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=0-5000", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=0-1,4-5", 1000), None);
    }

    #[test]
    fn mimes() {
        assert_eq!(guess_mime(None, Some("IMG_1.HEIC")), "image/heic");
        assert_eq!(guess_mime(Some("image/jpeg"), None), "image/jpeg");
        assert!(!inline_ok("image/svg+xml"));
        assert!(!inline_ok("text/html"));
    }
}
