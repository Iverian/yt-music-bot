use std::io;
use std::str::FromStr;

use anyhow::{bail, ensure, Result as AnyResult};
use camino::Utf8PathBuf;
use itertools::Itertools;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::controller::{Controller, Event, Receiver as ControllerReceiver};
use crate::util::Handle;

#[derive(Debug)]
pub struct Server {
    token: CancellationToken,
    controller: Controller,
}

type WriterTx = mpsc::UnboundedSender<Option<Vec<String>>>;
type WriterRx = mpsc::UnboundedReceiver<Option<Vec<String>>>;
type Reader = BufReader<OwnedReadHalf>;

struct Connection {
    token: CancellationToken,
    controller: Controller,
    writer: Writer,
}

#[derive(Debug, Clone)]
struct Writer(WriterTx);

struct WriterWorker(BufWriter<OwnedWriteHalf>);

#[derive(Debug, Error)]
#[error("write error")]
struct WriteError;

impl Server {
    pub fn spawn(
        token: CancellationToken,
        controller: Controller,
        path: Utf8PathBuf,
    ) -> AnyResult<Handle> {
        let listener = UnixListener::bind(&path)?;
        tracing::info!(address = ?path, "admin server started");
        let server = Self { token, controller };
        let handle = tokio::spawn(server.serve_forever(path, listener));
        Ok(handle)
    }

    async fn serve_forever(self, path: Utf8PathBuf, listener: UnixListener) -> AnyResult<()> {
        let r = self.serve_forever_impl(listener).await;
        tokio::fs::remove_file(path).await.ok();
        r
    }

    async fn serve_forever_impl(self, listener: UnixListener) -> AnyResult<()> {
        loop {
            tokio::select! {
                r = listener.accept() => {
                    let (stream, _) = r?;
                    self.handle_connection(stream);
                }
                _ = self.token.cancelled() => {
                    tracing::info!("admin server shutdown");
                    return Ok(());
                }
            }
        }
    }

    fn handle_connection(&self, stream: UnixStream) {
        tracing::info!("accepted connection");
        Connection::spawn(self.token.child_token(), self.controller.clone(), stream);
    }
}

impl Connection {
    fn spawn(token: CancellationToken, controller: Controller, stream: UnixStream) {
        let (reader, writer) = stream.into_split();
        let writer = Writer::new(writer);
        tokio::spawn(Connection::serve_notifications(
            controller.subscribe(),
            writer.clone(),
        ));
        let result = Self {
            token,
            controller,
            writer,
        };
        tokio::spawn(result.serve_requests(BufReader::new(reader)));
    }

    async fn serve_requests(mut self, mut rdr: Reader) {
        let mut buf = String::new();

        if self.writer.write("Hello!").is_err() {
            return;
        }
        loop {
            buf.clear();
            tokio::select! {
                r = rdr.read_line(&mut buf) => {
                    if r.is_err() {
                        self.writer.close();
                        break;
                    }
                }
                _ = self.token.cancelled() => {
                    self.writer.write("Bye!").ok();
                    self.writer.close();
                    break;
                }
            }
            match self.handle_request(&buf).await {
                Ok(x) if x => {
                    self.writer.write("Bye!").ok();
                    self.writer.close();
                }
                Err(e) => {
                    self.writer
                        .write_many(vec![format!("[x] error: {e:?}"), "Bye!".to_owned()])
                        .ok();
                    self.writer.close();
                    break;
                }
                _ => {}
            }
        }
    }

    async fn handle_request(&mut self, request: &str) -> AnyResult<bool> {
        let args = shell_words::split(request)?;
        ensure!(!args.is_empty(), "empty request");
        let (cmd, args) = args.split_first().unwrap();
        tracing::debug!(cmd=cmd, args=?args, "admin request");
        match cmd.as_str() {
            "exit" => {
                return Ok(true);
            }
            "play" => {
                self.controller.play().await?;
            }
            "pause" => {
                self.controller.pause().await?;
            }
            "play-toggle" | "p" => {
                self.controller.play_toggle().await?;
            }
            "stop" => {
                self.controller.stop().await?;
            }
            "skip" | "s" => {
                self.controller.skip().await?;
            }
            "queue" | "q" => {
                if args.is_empty() {
                    let view = self.controller.view().await;
                    if view.queue.is_empty() {
                        self.writer.write("> Queue is empty")?;
                    } else {
                        self.writer.write_many(
                            view.iter()
                                .enumerate()
                                .map(|(i, (_, t))| {
                                    let idx = if i == 0 {
                                        "*".to_owned()
                                    } else {
                                        format!("{i}")
                                    };
                                    format!("> ({idx}) {} - {}", t.artist, t.track)
                                })
                                .collect_vec(),
                        )?;
                    }
                } else {
                    let tracks = self
                        .controller
                        .queue(args.iter().map(|x| Url::from_str(x)).try_collect()?)
                        .await?;
                    self.writer
                        .write(format!("> Queued {} tracks", tracks.len()))?;
                }
            }
            _ => {
                bail!("unknown command: {cmd}");
            }
        }
        Ok(false)
    }

    async fn serve_notifications(mut rx: ControllerReceiver, wrt: Writer) {
        while let Some(e) = rx.recv().await {
            if Self::handle_notification(e, &wrt).is_err() {
                break;
            }
        }
    }

    fn handle_notification(e: Event, wrt: &Writer) -> Result<(), WriteError> {
        match e {
            Event::PlaybackStarted => wrt.write("[*] Started"),
            Event::PlaybackPaused => wrt.write("[*] Paused"),
            Event::PlaybackStopped => wrt.write("[*] Stopped"),
            Event::QueueFinished => wrt.write("[*] Queue completed"),
            Event::NowPlaying { queue_id: _, track } => wrt.write(format!(
                "[*] Now playing: {} - {}",
                track.artist, track.track
            )),
            Event::DownloadError { err } => wrt.write(format!("[x] download error: {err:?}")),
        }
    }
}

impl Writer {
    fn new(writer: OwnedWriteHalf) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let result = Self(tx);
        let worker = WriterWorker(BufWriter::new(writer));
        tokio::spawn(worker.serve_forever(rx));
        result
    }

    fn close(&self) {
        self.0.send(None).ok();
    }

    fn write<S>(&self, line: S) -> Result<(), WriteError>
    where
        S: Into<String>,
    {
        self.write_many(vec![line.into()])
    }

    fn write_many(&self, lines: Vec<String>) -> Result<(), WriteError> {
        self.0.send(Some(lines)).map_err(|_| WriteError)
    }
}

impl WriterWorker {
    async fn serve_forever(mut self, mut rx: WriterRx) {
        while let Some(x) = rx.recv().await {
            let Some(x) = x else {
                self.close(rx).await;
                break;
            };
            if self.handle_message(x).await.is_err() {
                self.close(rx).await;
                break;
            }
        }
    }

    async fn handle_message(&mut self, lines: Vec<String>) -> io::Result<()> {
        for i in lines {
            self.0.write_all(i.as_bytes()).await?;
            self.0.write_u8(b'\n').await?;
            self.0.flush().await?;
        }
        Ok(())
    }

    async fn close(self, mut rx: WriterRx) {
        self.0.into_inner().shutdown().await.ok();
        rx.close();
    }
}
