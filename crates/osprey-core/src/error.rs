use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("file error: {0}")]
    Io(#[from] std::io::Error),

    #[error("bad URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("state file error: {0}")]
    State(#[from] serde_json::Error),

    #[error("server replied {status} for {url}")]
    Status { status: u16, url: String },

    #[error("could not read playlist: {0}")]
    Parse(String),

    #[error("this stream is DRM protected ({0}). BM Osprey does not decrypt DRM content")]
    Drm(String),

    #[error("ffmpeg: {0}")]
    Ffmpeg(String),

    #[error("{0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;
