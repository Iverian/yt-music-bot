import functools
import signal
from dataclasses import dataclass
from typing import Any, List, Mapping, Optional

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
    "socket_timeout": 15,  # TODO: extract to config
}
YOUTUBE_DOWNLOAD_TIMEOUT_MSG = "Download timed out"

E_UNKNOWN_OBJECT = 0
E_OTHER = 1

Params = Mapping[str, Any]


@dataclass
class Track:
    track_id: str
    title: str
    artist: List[str]
    webpage_url: str
    duration_s: int
    path: str


class Youtube:
    _yt: YoutubeDL

    def __init__(self, download_dir: str, download_timeout_s: int = 30) -> None:
        self._yt = YoutubeDL(self._make_params(download_dir))
        self._download_timeout_s = download_timeout_s

    def close(self):
        self._yt.close()

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
        artist = []
        if "track" in data and "artist" in data:
            title = str(data["track"])
            # В некоторых треках Youtube отдает список артистов через запятую
            artist = [i.strip() for i in str(data["artist"]).split(",")]
        else:
            title = str(data["title"])

        return Track(
            track_id=str(data["id"]),
            title=title,
            artist=artist,
            webpage_url=str(data["webpage_url"]),
            duration_s=int(data["duration"]),
            path=str(self._yt.prepare_filename(data)),
        )

    @staticmethod
    def _make_params(download_dir: str) -> Params:
        params = YOUTUBE_DL_PARAMS.copy()
        params["cachedir"] = download_dir
        params["outtmpl"] = f"{download_dir}/{YOUTUBE_DL_OUTPUT_TEMPLATE}"
        return params


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
