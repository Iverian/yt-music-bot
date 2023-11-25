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
use rust_embed::RustEmbed;
use thiserror::Error;
use tokio::sync::oneshot;
use url::Url;

static SERVER_CODE: Lazy<String> =
    Lazy::new(|| String::from_utf8(PythonFiles::get("server.py").unwrap().data.to_vec()).unwrap());
static YOUTUBE_URL: Lazy<Url> = Lazy::new(|| "https://www.youtube.com/watch".parse().unwrap());
const YOUTUBE_HOSTS: &[&str] = &["youtu.be", "youtube.com", "www.youtube.com"];

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Youtube {
    tx: RequestTx,
}

#[derive(Debug, Clone, Error)]
pub enum Error {
    #[error("channel error")]
    Channel,
    #[error("invalid url: {0}")]
    InvalidUrl(Url),
    #[error("unknown youtube object: {0}")]
    UnknownObject(String),
    #[error("error for url {0}: {1}")]
    Other(String, String),
}

pub type TrackId = String;

#[derive(Debug, Clone)]
pub struct Track {
    pub id: TrackId,
    pub track: String,
    pub artist: String,
    pub webpage_url: Url,
    pub duration: Duration,
    pub path: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackUrl(Url);

type RequestRx = async_channel::Receiver<RequestEnvelope>;
type RequestTx = async_channel::Sender<RequestEnvelope>;
type ResponseTx = oneshot::Sender<Result<Response>>;
type StartupTx = oneshot::Sender<AnyResult<()>>;

#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/src/youtube_dl"]
#[include = "*.py"]
struct PythonFiles;

#[derive(FromPyObject)]
struct TrackRaw {
    track_id: String,
    track: String,
    artist: String,
    webpage_url: String,
    duration_s: u64,
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
    pub async fn new(download_dir: &Utf8Path, jobs: usize) -> AnyResult<Self> {
        let (tx, rx) = async_channel::unbounded();

        for id in 1..=jobs {
            let rx = rx.clone();
            let download_dir = download_dir.to_owned();
            let (stx, srx) = oneshot::channel();
            thread::spawn(move || Worker::run(stx, id, &rx, &download_dir));
            srx.await??;
        }

        Ok(Self { tx })
    }

    pub async fn resolve(&self, urls: Vec<Url>) -> Result<Vec<Track>> {
        for url in &urls {
            let host = url
                .host_str()
                .ok_or_else(|| Error::InvalidUrl(url.clone()))?;
            if YOUTUBE_HOSTS.iter().all(|&x| x != host) {
                return Err(Error::InvalidUrl(url.clone()));
            }
        }

        self.request(Request::Resolve(urls)).await.map(|x| match x {
            Response::Resolve(x) => x,
            Response::Download(_) => unreachable!(),
        })
    }

    pub async fn download_by_id(&self, id: &str) -> Result<Utf8PathBuf> {
        self.download_url(make_youtube_url(id)).await
    }

    async fn download_url(&self, track: Url) -> Result<Utf8PathBuf> {
        self.request(Request::Download(track))
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
            .map_err(|_| Error::Channel)?;
        rx.await.map_err(|_| Error::Channel)?
    }
}

impl<'a> Worker<'a> {
    fn serve_forever(self, rx: &RequestRx) {
        loop {
            let Ok(RequestEnvelope { tx, payload }) = rx.recv_blocking() else {
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
                        intern!(self.py, "resolve"),
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
                    .call_method1(intern!(self.py, "download"), (url.to_string(),))
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
    fn run(stx: StartupTx, id: usize, rx: &RequestRx, download_dir: &Utf8Path) {
        Python::with_gil(|py| {
            let server = match Self::load_python_code(py, download_dir) {
                Ok(x) => x,
                Err(e) => {
                    stx.send(Err(e.into())).unwrap();
                    return;
                }
            };
            let worker = Worker { py, server };
            stx.send(Ok(())).unwrap();
            worker.serve_forever(rx);
            server.call_method0(intern!(py, "close")).ok();
            tracing::info!("downloader {id} finished");
        });
    }

    fn load_python_code<'b>(py: Python<'b>, download_dir: &Utf8Path) -> PyResult<&'b PyAny> {
        let module = PyModule::from_code(py, &SERVER_CODE, "server.py", "server").unwrap();
        let server_class = module.getattr(intern!(py, "Server")).unwrap();
        let server = server_class.call1((download_dir.as_str(),))?;
        Ok(server)
    }
}

impl From<ErrorRaw> for Error {
    fn from(value: ErrorRaw) -> Self {
        match value.kind {
            0 => Self::UnknownObject(value.url),
            1 => Self::Other(value.url, value.message.unwrap()),
            _ => unreachable!(),
        }
    }
}

impl From<TrackRaw> for Track {
    fn from(value: TrackRaw) -> Self {
        Self {
            id: value.track_id,
            track: value.track,
            artist: value.artist,
            webpage_url: value.webpage_url.parse().unwrap(),
            duration: Duration::from_secs(value.duration_s),
            path: value.path.parse().unwrap(),
        }
    }
}

fn make_youtube_url(id: &str) -> Url {
    let mut result = YOUTUBE_URL.clone();
    {
        let mut query = result.query_pairs_mut();
        query.append_pair("v", id);
    }
    result
}
