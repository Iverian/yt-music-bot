use std::fmt::Display;
use std::time::Duration;

use teloxide::utils::markdown;

use crate::controller::Status;
use crate::youtube::Track;

pub struct TrackFmt<'a>(pub &'a Track);

pub struct DurationFmt<'a>(pub &'a Duration);

pub struct StatusFmt<'a>(pub &'a Status);

impl<'a> Display for TrackFmt<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let webpage_url = markdown::escape_link_url(self.0.webpage_url.as_str());

        if let Some(artist) = self.0.artist.first() {
            let title = markdown::escape(&format!("{} - {}", artist, self.0.title));
            write!(
                f,
                "[{}]({}) {}",
                title,
                webpage_url,
                DurationFmt(&self.0.duration)
            )
        } else {
            let title = markdown::escape(&self.0.title);
            write!(
                f,
                "[{}]({}) {}",
                title,
                webpage_url,
                DurationFmt(&self.0.duration)
            )
        }
    }
}

impl<'a> Display for DurationFmt<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total = self.0.as_secs();
        let minutes = total / 60;
        let seconds = total % 60;
        write!(f, "{minutes}:{seconds:02}")
    }
}

impl<'a> Display for StatusFmt<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:1} : {:1} {:2} : {} треков в очереди",
            if self.0.is_paused { "⏸️" } else { "▶️" },
            if self.0.is_muted { "🔇" } else { "🔊" },
            self.0.volume_level,
            self.0.length
        )?;
        if let Some(track) = &self.0.now_playing {
            write!(f, "\n🎷 {}", TrackFmt(track))?;
        }
        Ok(())
    }
}
