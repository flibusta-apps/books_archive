use std::sync::Arc;

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
use tracing::error;

use crate::services::archive::{self, ArchiveError, Catalog};

#[derive(Clone)]
pub struct AppState {
    pub index: Arc<Catalog>,
    pub api_key: Option<Arc<str>>,
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("not found")]
    NotFound,
    #[error("bad id")]
    BadId,
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

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Fb2,
    Epub,
    Mobi,
    Any,
}

fn parse_id(id: &str) -> Result<u64, ApiError> {
    id.parse().map_err(|_| ApiError::BadId)
}

async fn serve(state: AppState, id: &str, kind: Kind) -> Result<Response, ApiError> {
    let id = parse_id(id)?;
    let found = state
        .index
        .find(id, |ext| match kind {
            Kind::Fb2 => ext == "fb2",
            Kind::Epub => ext == "epub",
            Kind::Mobi => ext == "mobi",
            Kind::Any => true,
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    let ext = found.ext.clone();

    let as_zip = ext == "fb2";

    let out = archive::open(found, id, as_zip).await?;
    let (ctype, filename) = if as_zip {
        ("application/zip", format!("{id}.fb2.zip"))
    } else {
        (content_type(&ext), format!("{id}.{ext}"))
    };

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(out.len));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|_| ApiError::BadId)?,
    );
    Ok((headers, Body::from_stream(ReaderStream::new(out.file))).into_response())
}

async fn get_fb2(State(s): State<AppState>, Path(id): Path<String>) -> Result<Response, ApiError> {
    serve(s, &id, Kind::Fb2).await
}

async fn get_epub(State(s): State<AppState>, Path(id): Path<String>) -> Result<Response, ApiError> {
    serve(s, &id, Kind::Epub).await
}

async fn get_mobi(State(s): State<AppState>, Path(id): Path<String>) -> Result<Response, ApiError> {
    serve(s, &id, Kind::Mobi).await
}

async fn get_download(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    serve(s, &id, Kind::Any).await
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

pub fn get_router(state: AppState) -> Router {
    let books = Router::new()
        .route("/b/{id}/fb2", get(get_fb2))
        .route("/b/{id}/epub", get(get_epub))
        .route("/b/{id}/mobi", get(get_mobi))
        .route("/b/{id}/download", get(get_download))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state);

    Router::new()
        .route("/health", get(|| async { "OK" }))
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
        let state = AppState {
            index: Arc::new(Catalog::Ranges(ArchiveIndex::scan(dir.path()))),
            api_key: api_key.map(Arc::from),
        };
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
            ],
        );
        let inp = format!(
            "{}\r\n{}\r\n{}\r\n",
            line("fileA", "5000", "0", "fb2"),
            line("fileB", "5001", "0", "EPUB"),
            line("ghost", "5002", "0", "fb2"),
        );
        let inpx = sub.join("c.inpx");
        make_zip(
            &inpx,
            &[("version.info", b"1\r\n"), ("books-a.inp", inp.as_bytes())],
        );
        if missing_archive {
            std::fs::remove_file(sub.join("books-a.zip")).unwrap();
        }
        assert_eq!(inpx::discover(dir.path()), Some(inpx.clone()));
        let state = AppState {
            index: Arc::new(Catalog::Inpx(inpx::load(&inpx).unwrap())),
            api_key: None,
        };
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
        ] {
            let (a, _d) = inpx_app(false);
            assert_eq!(call(a, uri, None).await.0, StatusCode::NOT_FOUND, "{uri}");
        }
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
        (f.name().to_string(), buf)
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
