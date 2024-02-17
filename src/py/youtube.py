import functools
import signal
from dataclasses import dataclass
from multiprocessing import Pipe, Process, Queue
from multiprocessing.connection import Connection
from typing import Any, Iterable, List, Mapping, Optional, Union

from yt_dlp import YoutubeDL

YOUTUBE_DL_OUTPUT_TEMPLATE = "%(id)s.%(ext)s"
YOUTUBE_DL_FORMATS = [
    "ba[acodec=mp4a.40.2]",
    "ba[ext=mp4][protocol=m3u8_native]",
    "ba[ext=mp4][protocol=m3u8]",
    "ba[acodec=mp4a.40.5]",
]
YOUTUBE_DL_PARAMS = {
    "format": "/".join(YOUTUBE_DL_FORMATS),
    "prefer_free_formats": True,
    "restrictfilenames": True,
    "noplaylist": False,
    "nocheckcertificate": True,
    "ignoreerrors": False,
    "logtostderr": False,
    "quiet": True,
    "no_warnings": True,
    "default_search": "auto",
    "source_address": "0.0.0.0",
    "consoletitle": False,
    "color": "never",
}
YOUTUBE_DOWNLOAD_TIMEOUT_MSG = "Download timed out"

E_UNKNOWN_OBJECT = 0
E_OTHER = 1

R_RESOLVE = 0
R_DOWNLOAD = 1

Params = Mapping[str, Any]
RequestPayload = Union[List[str], str]
ResponsePayload = Union[List["Track"], str]


@dataclass
class Track:
    track_id: str
    title: str
    artist: List[str]
    webpage_url: str
    duration_s: int
    is_music_track: bool
    path: str


@dataclass
class Request:
    kind: int
    payload: RequestPayload


@dataclass
class RequestWrapper:
    tx: Connection
    request: Request


class Youtube:
    _queue: Queue
    _jobs: List[Process]

    def __init__(
        self,
        download_dir: str,
        jobs: int,
        download_timeout_s: int,
    ):
        params = self._make_params(download_dir, download_timeout_s)
        self._queue = Queue(maxsize=jobs)
        self._jobs = [_Worker.spawn(self._queue, params) for _ in range(jobs)]

    def close(self):
        for _ in self._jobs:
            self._queue.put(None)
        for process in self._jobs:
            try:
                process.join()
                process.close()
            except Exception:  # pylint: disable=broad-exception-caught
                pass

    def resolve(self, urls: List[str]) -> List[Track]:
        return self._request(R_RESOLVE, urls)  # type: ignore

    def download(self, url: str) -> str:
        return self._request(R_DOWNLOAD, url)  # type: ignore

    def _request(self, kind: int, payload: RequestPayload):
        rx, tx = Pipe(duplex=False)
        self._queue.put(RequestWrapper(tx, Request(kind, payload)))
        r = rx.recv()
        if isinstance(r, Exception):
            raise r
        return r

    @staticmethod
    def _make_params(download_dir: str, download_timeout_s: int) -> Params:
        params = YOUTUBE_DL_PARAMS.copy()
        params["cachedir"] = download_dir
        params["outtmpl"] = f"{download_dir}/{YOUTUBE_DL_OUTPUT_TEMPLATE}"
        params["socket_timeout"] = download_timeout_s
        return params


class _Worker:
    _yt: YoutubeDL
    _download_timeout_s: int

    def __init__(self, params: Params):
        self._yt = YoutubeDL(params)
        self._download_timeout_s = int(params.get("socket_timeout", 15))

    @staticmethod
    def spawn(queue: Queue, params: Params) -> Process:
        process = Process(target=_Worker.run, args=(queue, params))
        process.start()
        return process

    @staticmethod
    def run(queue: Queue, params: Params):
        worker = _Worker(params)
        try:
            while True:
                r: Optional[RequestWrapper] = queue.get()
                if not r:
                    break
                try:
                    response = worker.handle(r.request.kind, r.request.payload)
                    r.tx.send(response)
                except Exception as e:  # pylint: disable=broad-exception-caught
                    r.tx.send(e)
        finally:
            worker.close()

    def close(self):
        try:
            self._yt.close()
        except Exception:  # pylint: disable=broad-exception-caught
            pass

    def handle(self, kind: int, payload: Union[List[str], str]) -> ResponsePayload:
        if kind == R_RESOLVE:
            return self.resolve(payload)  # type: ignore
        if kind == R_DOWNLOAD:
            return self.download(payload)  # type: ignore
        raise NotImplementedError()

    def resolve(self, urls: List[str]) -> List[Track]:
        result = []
        for i in urls:
            try:
                result.extend(self._resolve_url(i))
            except Error:
                raise
            except Exception as e:
                raise Error.other(i, e) from e

        return result

    def download(self, url: str) -> str:
        try:
            return timeout(self._download_timeout_s, YOUTUBE_DOWNLOAD_TIMEOUT_MSG)(
                self._download_impl
            )(url)
        except Error:
            raise
        except Exception as e:
            raise Error.other(url, e) from e

    def _download_impl(self, url: str) -> str:
        data = self._yt.extract_info(url, download=True)
        if not isinstance(data, dict):
            raise Error.unknown_object(url)
        return str(self._yt.prepare_filename(data))

    def _resolve_url(self, url: str) -> List[Track]:
        result = None

        data = self._yt.extract_info(url, download=False)
        if not isinstance(data, dict):
            raise Error.unknown_object(url)

        data_type = data.get("_type", "track")
        if data_type == "playlist":
            result = [self._get_track(i) for i in data["entries"]]
        elif data_type == "track":
            result = [self._get_track(data)]
        else:
            raise Error.unknown_object(url)
        return result

    def _get_track(self, data: Mapping[str, Any]) -> Track:
        title = None
        is_music_track = False
        artist = []
        if "track" in data and "artist" in data:
            title = str(data["track"])
            # В некоторых треках Youtube отдает список артистов через запятую
            artist = unique(i.strip() for i in str(data["artist"]).split(","))
            is_music_track = True
        else:
            title = str(data["title"])

        return Track(
            track_id=str(data["id"]),
            title=title,
            artist=artist,
            webpage_url=str(data["webpage_url"]),
            duration_s=int(data["duration"]),
            is_music_track=is_music_track,
            path=str(self._yt.prepare_filename(data)),
        )


class Error(Exception):
    kind: int
    url: str
    message: Optional[str]

    def __init__(self, kind: int, url: str, message: Optional[str] = None) -> None:
        self.kind = kind
        self.url = url
        self.message = message

    @staticmethod
    def unknown_object(url: str) -> "Error":
        return Error(E_UNKNOWN_OBJECT, url)

    @staticmethod
    def other(url: str, cause: Exception) -> "Error":
        return Error(E_OTHER, url, str(cause))


def unique(value: Iterable[str]) -> List[str]:
    result = []
    for i in value:
        if i not in result:
            result.append(i)
    return result


def timeout(seconds: int, error_message: str):
    def decorator(func):
        def _handle_timeout(signum, frame):
            raise TimeoutError(error_message)

        @functools.wraps(func)
        def wrapper(*args, **kwargs):
            signal.signal(signal.SIGALRM, _handle_timeout)
            signal.alarm(seconds)
            try:
                result = func(*args, **kwargs)
            finally:
                signal.alarm(0)
            return result

        return wrapper

    return decorator
