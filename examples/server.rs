//! cargo run -p faxe-engine --example server -- --profile profile.json
mod common;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use common::AnyError;
use faxe_engine::*;
use std::{net::SocketAddr, path::PathBuf};
use tokio::io::AsyncWriteExt;

const MAX_UPLOAD: usize = 128 * 1024 * 1024;

#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    profile: PathBuf,
    #[arg(long, default_value = "./faxe-server-data")]
    data_dir: PathBuf,
    #[arg(long, default_value = "127.0.0.1:3000")]
    bind: SocketAddr,
    #[arg(long, env = "CONCURRENCY", default_value_t = 100)]
    concurrency: usize,
}
#[derive(Clone)]
struct App {
    engine: EngineHandle,
    profile: SipProfile,
    uploads: PathBuf,
}
struct ApiError(StatusCode, String);
impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let status = match error {
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Invalid(_) | Error::Image(_) | Error::Pdf(_) => StatusCode::BAD_REQUEST,
            Error::WorkerStopped => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, error.to_string())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}
fn bad(error: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, error.to_string())
}
struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn submit(
    State(app): State<App>,
    mut multipart: Multipart,
) -> std::result::Result<Response, ApiError> {
    let upload = tempfile::tempdir_in(&app.uploads).map_err(Error::from)?;
    let (mut destination, mut path, mut mode) = (None, None, None);
    while let Some(mut field) = multipart.next_field().await.map_err(bad)? {
        match field.name().unwrap_or("") {
            "destination" if destination.is_none() => {
                destination = Some(field.text().await.map_err(bad)?)
            }
            "mode" if mode.is_none() => {
                mode = Some(
                    match field
                        .text()
                        .await
                        .map_err(bad)?
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "auto" => FaxMode::Auto,
                        "t38" => FaxMode::T38,
                        "g711" => FaxMode::G711,
                        _ => return Err(bad("mode must be auto, t38, or g711")),
                    },
                );
            }
            "file" if path.is_none() => {
                let extension = std::path::Path::new(field.file_name().unwrap_or(""))
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !matches!(extension.as_str(), "pdf" | "png" | "jpg" | "jpeg") {
                    return Err(bad("Upload a PDF, PNG, or JPEG file"));
                }
                let file_path = upload.path().join(format!("document.{extension}"));
                let mut file = tokio::fs::File::create(&file_path)
                    .await
                    .map_err(Error::from)?;
                let mut size = 0;
                while let Some(chunk) = field.chunk().await.map_err(bad)? {
                    size += chunk.len();
                    if size > MAX_UPLOAD {
                        return Err(ApiError(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "File exceeds 128 MiB".into(),
                        ));
                    }
                    file.write_all(&chunk).await.map_err(Error::from)?;
                }
                file.flush().await.map_err(Error::from)?;
                path = Some(file_path);
            }
            _ => return Err(bad("Unknown or duplicate multipart field")),
        }
    }
    let destination: String = destination.ok_or_else(|| bad("destination is required"))?;
    if destination.trim().is_empty() {
        return Err(bad("destination is required"));
    }
    let path = path.ok_or_else(|| bad("file is required"))?;
    let cancellation = Cancellation::default();
    let _cancel = CancelOnDrop(cancellation.clone());
    // The task owns the upload until preparation has stopped, including disconnects.
    let job = tokio::spawn(async move {
        let _upload = upload;
        let document = app
            .engine
            .prepare(
                DocumentInput {
                    paths: vec![path],
                    options: DocumentOptions::default(),
                },
                cancellation.clone(),
            )
            .await?;
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        app.engine
            .enqueue(FaxRequest {
                profile_id: app.profile.id,
                destination,
                mode: mode.unwrap_or(app.profile.sending_mode),
                document,
            })
            .await
    })
    .await
    .map_err(|_| Error::WorkerStopped)??;
    let location = format!("/faxes/{}", job.id);
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, location.clone())],
        Json(serde_json::json!({"id": job.id, "status_url": location})),
    )
        .into_response())
}
async fn status(
    State(app): State<App>,
    Path(id): Path<Uuid>,
) -> std::result::Result<Json<Job>, ApiError> {
    Ok(Json(app.engine.job(id).await?))
}
fn router(app: App) -> Router {
    Router::new()
        .route("/faxes", post(submit))
        .route("/faxes/{id}", get(status))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD + 64 * 1024))
        .with_state(app)
}
#[tokio::main]
async fn main() -> std::result::Result<(), AnyError> {
    common::logging();
    let args = Arguments::parse();
    let profile = common::profile(&args.profile)?;
    let runtime = common::open(args.data_dir.clone(), args.concurrency).await?;
    let engine = runtime.handle();
    engine.save_profile(profile.clone()).await?;
    let uploads = args.data_dir.join("uploads");
    tokio::fs::create_dir_all(&uploads).await?;
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    eprintln!(
        "HTTP fax sender listening on http://{}",
        listener.local_addr()?
    );
    let result = axum::serve(
        listener,
        router(App {
            engine,
            profile,
            uploads,
        }),
    )
    .with_graceful_shutdown(common::interrupted())
    .await;
    runtime.shutdown().await?;
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use std::sync::Arc;
    use tower::ServiceExt;
    struct NoCredentials;
    impl Credentials for NoCredentials {
        fn password(&self, _: Uuid) -> Result<Option<Password>> {
            Ok(None)
        }
    }
    fn upload(destination: &str, bytes: &[u8]) -> Request<Body> {
        let mut body = format!("--fax\r\nContent-Disposition: form-data; name=\"destination\"\r\n\r\n{destination}\r\n--fax\r\nContent-Disposition: form-data; name=\"file\"; filename=\"page.png\"\r\nContent-Type: image/png\r\n\r\n").into_bytes();
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n--fax--\r\n");
        Request::post("/faxes")
            .header("Content-Type", "multipart/form-data; boundary=fax")
            .body(Body::from(body))
            .unwrap()
    }
    #[tokio::test]
    async fn overlapping_uploads_status_errors_and_cleanup() -> std::result::Result<(), AnyError> {
        let root = tempfile::tempdir()?;
        let runtime = EngineRuntime::open_with_options(
            root.path().join("data"),
            Arc::new(NoCredentials),
            None,
            EngineOptions {
                limits: CallLimits {
                    incoming: 2,
                    outgoing: 2,
                },
                document_workers: 2,
                ..Default::default()
            },
        )?;
        let handle = runtime.handle();
        let profile: SipProfile = serde_json::from_str(include_str!("common/profile.json"))?;
        handle.save_profile(profile.clone()).await?;
        let uploads = root.path().join("uploads");
        std::fs::create_dir(&uploads)?;
        let router = router(App {
            engine: handle.clone(),
            profile,
            uploads: uploads.clone(),
        });
        let mut png = std::io::Cursor::new(Vec::new());
        image::GrayImage::from_pixel(16, 16, image::Luma([255]))
            .write_to(&mut png, image::ImageFormat::Png)?;
        let (first, second) = tokio::join!(
            router.clone().oneshot(upload("1212", png.get_ref())),
            router.clone().oneshot(upload("2323", png.get_ref()))
        );
        let mut ids = Vec::new();
        for response in [first?, second?] {
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
            let id = body["id"].as_str().unwrap().to_owned();
            let response = router
                .clone()
                .oneshot(Request::get(format!("/faxes/{id}")).body(Body::empty())?)
                .await?;
            assert_eq!(response.status(), StatusCode::OK);
            ids.push(id);
        }
        assert_ne!(ids[0], ids[1]);
        assert_eq!(
            router
                .clone()
                .oneshot(upload("1212", b"not a PNG"))
                .await?
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            router
                .clone()
                .oneshot(upload("", png.get_ref()))
                .await?
                .status(),
            StatusCode::BAD_REQUEST
        );
        let missing = Request::get(format!("/faxes/{}", Uuid::new_v4())).body(Body::empty())?;
        assert_eq!(
            router.oneshot(missing).await?.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(handle.view().jobs.len(), 2);
        assert_eq!(std::fs::read_dir(uploads)?.count(), 0);
        runtime.shutdown().await?;
        Ok(())
    }
}
