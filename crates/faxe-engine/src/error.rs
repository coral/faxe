#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Transmission(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("Database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("Stored data: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Image: {0}")]
    Image(#[from] image::ImageError),
    #[error("TIFF: {0}")]
    Tiff(#[from] tiff::TiffError),
    #[error("PDF: {0}")]
    Pdf(String),
    #[error("Operation cancelled")]
    Cancelled,
    #[error("{0} was not found")]
    NotFound(String),
    #[error("Engine worker stopped")]
    WorkerStopped,
    #[error("Credentials: {0}")]
    Credentials(String),
}

pub type Result<T> = std::result::Result<T, Error>;
