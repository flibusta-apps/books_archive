use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio_util::io::ReaderStream;
use tower::limit::ConcurrencyLimitLayer;
use tracing::error;

use crate::services::archive::{ArchiveError, Catalog};

#[derive(Clone)]
pub struct AppState {
    pub index: Arc<Catalog>,
    pub api_key: Option<Arc<str>>,
    pub max_entry_bytes: u64,
    pub timeout: Duration,
    pub max_concurrent: usize,
}

impl AppState {
    pub fn new(index: Arc<Catalog>, api_key: Option<Arc<str>>) -> AppState {
        AppState {
            index,
            api_key,
            max_entry_bytes: 512 * 1024 * 1024,
            timeout: Duration::from_secs(600),
            max_concurrent: 16,
        }
    }
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("not found")]
    NotFound,
    #[error("bad id")]
    BadId,
    #[error("internal: {0}")]
    Internal(String),
    #[error("timeout")]
    Timeout,
    #[error("unauthorized")]
    Unauthorized,
    #[error(transparent)]
    Archive(#[from] ArchiveError),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::BadId => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            ApiError::Internal(msg) => {
                error!("Internal error: {msg}");
                StatusCode::INTERNAL_SERVER_ERROR
            }
            ApiError::Archive(err) => {
                error!("Archive error: {err}");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (
            status,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            status.canonical_reason().unwrap_or("error"),
        )
            .into_response()
    }
}

pub fn content_type(ext: &str) -> &'static str {
    match ext {
        "fb2" => "application/x-fictionbook+xml",
        "epub" => "application/epub+zip",
        "mobi" => "application/x-mobipocket-ebook",
        "pdf" => "application/pdf",
        "djvu" | "djv" => "image/vnd.djvu",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "rtf" => "application/rtf",
        "txt" => "text/plain; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Maps the (case-insensitive) route format to the required extension.
/// `None` inside `Some` means "any extension".
fn parse_format(format: &str) -> Option<Option<&'static str>> {
    match format.to_ascii_lowercase().as_str() {
        "fb2" => Some(Some("fb2")),
        "epub" => Some(Some("epub")),
        "mobi" => Some(Some("mobi")),
        "download" => Some(None),
        _ => None,
    }
}

fn parse_id(id: &str) -> Result<u64, ApiError> {
    id.parse().map_err(|_| ApiError::BadId)
}

async fn get_book(
    State(s): State<AppState>,
    Path((id, format)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let want = parse_format(&format).ok_or(ApiError::NotFound)?;
    let id = parse_id(&id)?;
    let out = tokio::time::timeout(s.timeout, s.index.fetch(id, want, s.max_entry_bytes))
        .await
        .map_err(|_| ApiError::Timeout)??
        .ok_or(ApiError::NotFound)?;

    let (ctype, filename) = if out.zipped {
        ("application/zip", format!("{id}.{}.zip", out.ext))
    } else {
        (content_type(&out.ext), format!("{id}.{}", out.ext))
    };

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(out.len));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|_| ApiError::Internal(format!("bad content-disposition {filename:?}")))?,
    );
    let stream = ReaderStream::with_capacity(out.file, 64 * 1024);
    Ok((headers, Body::from_stream(stream)).into_response())
}

async fn auth(State(s): State<AppState>, req: Request, next: Next) -> Result<Response, ApiError> {
    if let Some(key) = &s.api_key {
        let given = req
            .headers()
            .get(header::AUTHORIZATION)
            .map(|v| v.as_bytes())
            .unwrap_or_default();
        if !bool::from(given.ct_eq(key.as_bytes())) {
            return Err(ApiError::Unauthorized);
        }
    }
    Ok(next.run(req).await)
}

async fn ready(State(s): State<AppState>) -> StatusCode {
    if s.index.is_empty() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

pub fn get_router(state: AppState) -> Router {
    let books = Router::new()
        .route("/b/{id}/{format}", get(get_book))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(ConcurrencyLimitLayer::new(state.max_concurrent))
        .with_state(state.clone());

    Router::new()
        .route("/health", get(|| async { "OK" }))
        .route("/ready", get(ready))
        .with_state(state)
        .merge(books)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::archive::{tests::make_zip, ArchiveIndex};
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::io::Cursor;
    use tower::ServiceExt;

    fn app(api_key: Option<&str>) -> (Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        make_zip(
            &dir.path().join("f.fb2-000001-000100.zip"),
            &[("7.fb2", b"<FictionBook/>"), ("8.epub", b"epubdata")],
        );
        make_zip(
            &dir.path().join("f.usr-000101-000200.zip"),
            &[("150.pdf", b"%PDF-1"), ("151.bin", b"zzz")],
        );
        let state = AppState::new(
            Arc::new(Catalog::Ranges(ArchiveIndex::scan(dir.path()))),
            api_key.map(Arc::from),
        );
        (get_router(state), dir)
    }

    fn inpx_app(missing_archive: bool) -> (Router, tempfile::TempDir) {
        use crate::services::inpx::{self, tests::line};
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("coll");
        std::fs::create_dir(&sub).unwrap();
        // FILE differs from LIBID; archive name has no usable range.
        make_zip(
            &sub.join("books-a.zip"),
            &[
                ("5000.fb2", b"WRONG-GUESS"),
                ("fileA.fb2", b"<FictionBook/>"),
                ("fileB.EPUB", b"epubdata"),
                // catalog says fb2 but entry is stored as .epub
                ("mism.epub", b"mismatch"),
                // FILE != LIBID, entry stored with different ext case
                ("dir/other.Mobi", b"mobidata"),
                // ambiguous fallback
                ("amb.pdf", b"a"),
                ("sub/amb.PDF", b"b"),
                // the LIBID-named decoy must never be served for FILE != LIBID
                ("5004.mobi", b"DECOY"),
            ],
        );
        let inp = format!(
            "{}\r\n{}\r\n{}\r\n{}\r\n",
            line("fileA", "5000", "0", "fb2"),
            line("fileB", "5001", "0", "EPUB"),
            line("ghost", "5002", "0", "fb2"),
            line("mism", "5003", "0", "fb2"),
        );
        let inp = format!(
            "{inp}{}\r\n{}\r\n",
            line("other", "5004", "0", "mobi"),
            line("amb", "5005", "0", "pdf"),
        );
        let inpx = sub.join("c.inpx");
        make_zip(
            &inpx,
            &[("version.info", b"1\r\n"), ("books-a.inp", inp.as_bytes())],
        );
        if missing_archive {
            std::fs::remove_file(sub.join("books-a.zip")).unwrap();
        }
        assert_eq!(inpx::discover(dir.path()), Ok(Some(inpx.clone())));
        let state = AppState::new(Arc::new(Catalog::Inpx(inpx::load(&inpx).unwrap())), None);
        (get_router(state), dir)
    }

    #[tokio::test]
    async fn inpx_lookup_end_to_end() {
        let (a, _d) = inpx_app(false);
        let (st, h, body) = call(a, "/b/5000/fb2", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "application/zip");
        assert_eq!(
            h[header::CONTENT_DISPOSITION],
            "attachment; filename=\"5000.fb2.zip\""
        );
        let (name, data) = unzip_entry(body);
        assert_eq!(name, "5000.fb2");
        assert_eq!(data, b"<FictionBook/>");

        let (a, _d) = inpx_app(false);
        let (st, h, body) = call(a, "/b/5001/epub", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "application/epub+zip");
        assert_eq!(body, b"epubdata");

        // ext mismatch and unknown id and entry absent from archive
        for uri in [
            "/b/5000/epub",
            "/b/5001/fb2",
            "/b/5001/mobi",
            "/b/1/fb2",
            "/b/5002/fb2",
            // fallback by FILE finds .epub but fb2 was requested: ext mismatch
            "/b/5003/fb2",
            // ambiguous fallback is rejected
            "/b/5005/pdf",
        ] {
            let (a, _d) = inpx_app(false);
            assert_eq!(call(a, uri, None).await.0, StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn inpx_fallback_by_file_basename() {
        // FILE != LIBID: found via FILE basename, never via `{LIBID}.*` decoy.
        let (a, _d) = inpx_app(false);
        let (st, h, body) = call(a, "/b/5004/mobi", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "application/x-mobipocket-ebook");
        assert_eq!(body, b"mobidata");
    }

    #[tokio::test]
    async fn format_is_case_insensitive() {
        let (a, _d) = app(None);
        let (st, _, body) = call(a, "/b/8/EPUB", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body, b"epubdata");
        let (a, _d) = app(None);
        assert_eq!(call(a, "/b/7/Fb2", None).await.0, StatusCode::OK);
        let (a, _d) = app(None);
        assert_eq!(call(a, "/b/7/nope", None).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn overlapping_ranges_with_format_filter() {
        let dir = tempfile::tempdir().unwrap();
        make_zip(&dir.path().join("a-1-100.zip"), &[("5.fb2", b"fb")]);
        make_zip(&dir.path().join("b-1-100.zip"), &[("5.epub", b"epubdata")]);
        let state = AppState::new(
            Arc::new(Catalog::Ranges(ArchiveIndex::scan(dir.path()))),
            None,
        );
        let (st, _, body) = call(get_router(state.clone()), "/b/5/epub", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body, b"epubdata");
        assert_eq!(
            call(get_router(state), "/b/5/mobi", None).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn corrupt_archive_is_500() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a-1-100.zip"), b"not a zip").unwrap();
        let state = AppState::new(
            Arc::new(Catalog::Ranges(ArchiveIndex::scan(dir.path()))),
            None,
        );
        assert_eq!(
            call(get_router(state), "/b/5/fb2", None).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn spool_read_error_is_500() {
        // Entry exceeding the byte cap fails while spooling.
        let dir = tempfile::tempdir().unwrap();
        make_zip(&dir.path().join("a-1-100.zip"), &[("5.epub", &[0u8; 100])]);
        let mut state = AppState::new(
            Arc::new(Catalog::Ranges(ArchiveIndex::scan(dir.path()))),
            None,
        );
        state.max_entry_bytes = 10;
        assert_eq!(
            call(get_router(state), "/b/5/epub", None).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn ready_reflects_catalog() {
        let (a, _d) = app(None);
        assert_eq!(call(a, "/ready", None).await.0, StatusCode::OK);
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            Arc::new(Catalog::Ranges(ArchiveIndex::scan(dir.path()))),
            None,
        );
        assert_eq!(
            call(get_router(state), "/ready", None).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn inpx_missing_archive_is_404() {
        let (a, _d) = inpx_app(true);
        assert_eq!(call(a, "/b/5000/fb2", None).await.0, StatusCode::NOT_FOUND);
    }

    async fn call(app: Router, uri: &str, key: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut req = Request::builder().uri(uri);
        if let Some(k) = key {
            req = req.header("authorization", k);
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, headers, body)
    }

    fn unzip_entry(data: Vec<u8>) -> (String, Vec<u8>) {
        use std::io::Read;
        let mut z = zip::ZipArchive::new(Cursor::new(data)).unwrap();
        assert_eq!(z.len(), 1);
        let mut f = z.by_index(0).unwrap();
        let mut buf = vec![];
        f.read_to_end(&mut buf).unwrap();
        (f.name().unwrap().into_owned(), buf)
    }

    #[tokio::test]
    async fn fb2_is_zipped() {
        for path in ["/b/7/fb2", "/b/7/download"] {
            let (app, _d) = app(None);
            let (st, h, body) = call(app, path, None).await;
            assert_eq!(st, StatusCode::OK);
            assert_eq!(h[header::CONTENT_TYPE], "application/zip");
            assert_eq!(h[header::CONTENT_LENGTH], body.len().to_string());
            assert_eq!(
                h[header::CONTENT_DISPOSITION],
                "attachment; filename=\"7.fb2.zip\""
            );
            let (name, data) = unzip_entry(body);
            assert_eq!(name, "7.fb2");
            assert_eq!(data, b"<FictionBook/>");
        }
    }

    #[tokio::test]
    async fn pdf_download_raw() {
        let (app, _d) = app(None);
        let (st, h, body) = call(app, "/b/150/download", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "application/pdf");
        assert_eq!(body, b"%PDF-1");
        assert_eq!(
            h[header::CONTENT_DISPOSITION],
            "attachment; filename=\"150.pdf\""
        );
        let (app, _d) = self::app(None);
        let (_, h, _) = call(app, "/b/151/download", None).await;
        assert_eq!(h[header::CONTENT_TYPE], "application/octet-stream");
    }

    #[tokio::test]
    async fn format_mismatch_and_missing() {
        let (a, _d) = app(None);
        assert_eq!(call(a, "/b/7/epub", None).await.0, StatusCode::NOT_FOUND);
        let (a, _d) = app(None);
        assert_eq!(call(a, "/b/150/fb2", None).await.0, StatusCode::NOT_FOUND);
        let (a, _d) = app(None);
        assert_eq!(call(a, "/b/150/mobi", None).await.0, StatusCode::NOT_FOUND);
        let (a, _d) = app(None);
        assert_eq!(
            call(a, "/b/999/download", None).await.0,
            StatusCode::NOT_FOUND
        );
        let (a, _d) = app(None);
        assert_eq!(call(a, "/b/abc/fb2", None).await.0, StatusCode::BAD_REQUEST);
        let (a, _d) = app(None);
        let (st, h, body) = call(a, "/b/8/epub", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "application/epub+zip");
        assert_eq!(body, b"epubdata");
    }

    #[tokio::test]
    async fn auth_behaviour() {
        let (a, _d) = app(Some("secret"));
        assert_eq!(call(a, "/b/7/fb2", None).await.0, StatusCode::UNAUTHORIZED);
        let (a, _d) = app(Some("secret"));
        assert_eq!(
            call(a, "/b/7/fb2", Some("wrong")).await.0,
            StatusCode::UNAUTHORIZED
        );
        let (a, _d) = app(Some("secret"));
        assert_eq!(call(a, "/b/7/fb2", Some("secret")).await.0, StatusCode::OK);
        let (a, _d) = app(Some("secret"));
        assert_eq!(call(a, "/health", None).await.0, StatusCode::OK);
    }
}
