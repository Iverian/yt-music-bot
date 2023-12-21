use std::convert::Into;
use std::thread;
use std::time::Duration;

use anyhow::Result as AnyResult;
use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use once_cell::sync::Lazy;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyModule;
use regex::Regex;
use rust_embed::RustEmbed;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use url::Url;

use crate::util::channel::ChannelError;

const PYTHON_FILE: &str = "youtube.py";
const PYTHON_MODULE: &str = "youtube";
const PYTHON_CLASS: &str = "Youtube";
const PYTHON_RESOLVE_METHOD: &str = "resolve";
const PYTHON_DOWNLOAD_METHOD: &str = "download";
const PYTHON_CLOSE_METHOD: &str = "close";

const HOSTS: &[&str] = &["youtu.be", "youtube.com", "www.youtube.com"];

static PYTHON_CODE: Lazy<String> =
    Lazy::new(|| String::from_utf8(PythonFiles::get(PYTHON_FILE).unwrap().data.to_vec()).unwrap());
static BASE_URL: Lazy<Url> = Lazy::new(|| "https://www.youtube.com/watch".parse().unwrap());
static ERROR_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"^ERROR: \[.*\] (.+)$").unwrap());

pub type Result<T> = core::result::Result<T, Error>;
pub type TrackId = String;

#[derive(Debug, Clone)]
pub struct Youtube {
    tx: RequestTx,
}

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub jobs: usize,
    pub download_timeout: Duration,
}

#[derive(Debug, Clone, Error)]
pub enum Error {
    #[error("Invalid url: {0}")]
    InvalidUrl(Url),
    #[error("Unknown youtube object: {0}")]
    UnknownObject(String),
    #[error("Error for url {0}: {1}")]
    Other(String, String),
    #[error(transparent)]
    Channel(#[from] ChannelError),
}

#[derive(Debug, Clone)]
pub struct Track {
    pub id: TrackId,
    pub title: String,
    pub artist: Vec<String>,
    pub webpage_url: Url,
    pub duration: Duration,
    pub is_music_track: bool,
    pub path: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackUrl(Url);

type RequestRx = mpsc::Receiver<RequestEnvelope>;
type RequestTx = mpsc::Sender<RequestEnvelope>;
type ResponseTx = oneshot::Sender<Result<Response>>;
type StartupTx = oneshot::Sender<AnyResult<()>>;

#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/src/py"]
#[include = "*.py"]
struct PythonFiles;

#[derive(FromPyObject)]
struct TrackRaw {
    track_id: String,
    title: String,
    artist: Vec<String>,
    webpage_url: String,
    duration_s: u64,
    is_music_track: bool,
    path: String,
}

#[derive(FromPyObject)]
struct ErrorRaw {
    kind: i64,
    url: String,
    message: Option<String>,
}

struct RequestEnvelope {
    tx: ResponseTx,
    payload: Request,
}

enum Request {
    Resolve(Vec<Url>),
    Download(Url),
}

enum Response {
    Resolve(Vec<Track>),
    Download(Utf8PathBuf),
}

struct Worker<'a> {
    py: Python<'a>,
    server: &'a PyAny,
}

impl Youtube {
    pub async fn new(download_dir: Utf8PathBuf, settings: Settings) -> AnyResult<Self> {
        let (tx, rx) = mpsc::channel(settings.jobs);
        let (stx, srx) = oneshot::channel();
        thread::spawn(move || Worker::run(stx, rx, &download_dir, settings));
        srx.await??;
        Ok(Self { tx })
    }

    pub async fn resolve(&self, urls: Vec<Url>) -> Result<Vec<Track>> {
        for url in &urls {
            let host = url
                .host_str()
                .ok_or_else(|| Error::InvalidUrl(url.clone()))?;
            if HOSTS.iter().all(|&x| x != host) {
                return Err(Error::InvalidUrl(url.clone()));
            }
        }

        self.request(Request::Resolve(urls)).await.map(|x| match x {
            Response::Resolve(x) => x,
            Response::Download(_) => unreachable!(),
        })
    }

    pub async fn download_by_id(&self, id: &str) -> Result<Utf8PathBuf> {
        self.request(Request::Download(make_youtube_url(id)))
            .await
            .map(|x| match x {
                Response::Download(x) => x,
                Response::Resolve(_) => unreachable!(),
            })
    }

    async fn request(&self, payload: Request) -> Result<Response> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(RequestEnvelope { tx, payload })
            .await
            .map_err(|_| ChannelError)?;
        rx.await.map_err(|_| ChannelError)?
    }
}

impl<'a> Worker<'a> {
    fn serve_forever(self, mut rx: RequestRx) {
        loop {
            let Some(RequestEnvelope { tx, payload }) = rx.blocking_recv() else {
                return;
            };
            tx.send(self.request_handler(payload)).ok();
        }
    }

    fn request_handler(&self, payload: Request) -> Result<Response> {
        Ok(match payload {
            Request::Resolve(urls) => {
                tracing::info!(urls = ?urls, "resolving youtube videos");
                let tracks = self
                    .server
                    .call_method1(
                        intern!(self.py, PYTHON_RESOLVE_METHOD),
                        (urls.into_iter().map(|x| x.to_string()).collect_vec(),),
                    )
                    .map_err(|e| self.map_err(&e))
                    .map(Self::map_resolve)?;
                Response::Resolve(tracks)
            }
            Request::Download(url) => {
                tracing::info!(url = ?url, "downloading youtube video");
                let path = self
                    .server
                    .call_method1(intern!(self.py, PYTHON_DOWNLOAD_METHOD), (url.to_string(),))
                    .map_err(|e| self.map_err(&e))
                    .map(Self::map_download)?;
                Response::Download(path)
            }
        })
    }

    fn map_resolve(value: &PyAny) -> Vec<Track> {
        value
            .extract::<Vec<TrackRaw>>()
            .unwrap()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    fn map_download(value: &PyAny) -> Utf8PathBuf {
        value.extract::<String>().unwrap().parse().unwrap()
    }

    fn map_err(&self, e: &PyErr) -> Error {
        e.value(self.py).extract::<ErrorRaw>().unwrap().into()
    }
}

impl Worker<'_> {
    fn run(stx: StartupTx, rx: RequestRx, download_dir: &Utf8Path, settings: Settings) {
        Python::with_gil(|py| {
            let server = match Self::load_python_code(py, download_dir, settings) {
                Ok(x) => x,
                Err(e) => {
                    stx.send(Err(e.into())).unwrap();
                    return;
                }
            };
            let worker = Worker { py, server };
            stx.send(Ok(())).unwrap();
            worker.serve_forever(rx);
            server.call_method0(intern!(py, PYTHON_CLOSE_METHOD)).ok();
        });
    }

    fn load_python_code<'b>(
        py: Python<'b>,
        download_dir: &Utf8Path,
        settings: Settings,
    ) -> PyResult<&'b PyAny> {
        let module = PyModule::from_code(py, &PYTHON_CODE, PYTHON_FILE, PYTHON_MODULE).unwrap();
        let server_class = module.getattr(intern!(py, PYTHON_CLASS)).unwrap();
        let server = server_class.call1((
            download_dir.as_str(),
            settings.jobs,
            settings.download_timeout.as_secs(),
        ))?;
        Ok(server)
    }
}

impl From<ErrorRaw> for Error {
    fn from(value: ErrorRaw) -> Self {
        match value.kind {
            0 => Self::UnknownObject(value.url),
            1 => {
                let message = value.message.unwrap();
                let message = ERROR_PATTERN
                    .captures(&message)
                    .and_then(|c| c.get(1).map(|x| x.as_str().to_owned()))
                    .unwrap_or(message);
                Self::Other(value.url, message)
            }
            _ => unreachable!(),
        }
    }
}

impl From<TrackRaw> for Track {
    fn from(value: TrackRaw) -> Self {
        Self {
            id: value.track_id,
            title: value.title,
            artist: value.artist,
            webpage_url: value.webpage_url.parse().unwrap(),
            duration: Duration::from_secs(value.duration_s),
            is_music_track: value.is_music_track,
            path: value.path.parse().unwrap(),
        }
    }
}

fn make_youtube_url(id: &str) -> Url {
    let mut result = BASE_URL.clone();
    {
        let mut query = result.query_pairs_mut();
        query.append_pair("v", id);
    }
    result
}
