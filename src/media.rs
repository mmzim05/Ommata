//! Media keys and the desktop's player controls, over MPRIS (D-Bus): play,
//! pause, next and previous reach ommata the way they reach a music player,
//! so the keyboard's media keys, `playerctl` and a bar's player widget work.

use eframe::egui;
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

/// What a media key asks ommata to do.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Request {
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    /// Seek by this many seconds.
    SeekBy(f64),
    /// Seek to this time in seconds.
    SeekTo(f64),
}

/// What the desktop is shown: the video and whether it plays.
#[derive(Clone, PartialEq, Debug)]
pub struct Now {
    pub title: String,
    pub duration: f64,
    pub playing: bool,
    pub position: f64,
}

/// The MPRIS player ommata shows while a video is open.
pub struct MediaKeys {
    controls: MediaControls,
    requests: Receiver<Request>,
    /// What was last sent (title, playing, length in ms), so unchanged
    /// state isn't sent every frame.
    shown: Option<(String, bool, u64)>,
}

impl MediaKeys {
    /// Register with D-Bus; `None` (with a note on stderr) without a session
    /// bus. Each request repaints `ctx`.
    pub fn new(ctx: &egui::Context) -> Option<Self> {
        // One name per window, as MPRIS asks of players with several.
        let dbus_name = format!("ommata.instance{}", std::process::id());
        let config = PlatformConfig {
            display_name: "Ommata",
            dbus_name: &dbus_name,
            hwnd: None,
        };
        let (sender, requests) = channel();
        let ctx = ctx.clone();
        let started = MediaControls::new(config).and_then(|mut controls| {
            controls.attach(move |event| {
                if let Some(request) = request(event) {
                    // The receiver lives as long as the app.
                    let _ = sender.send(request);
                    ctx.request_repaint();
                }
            })?;
            Ok(controls)
        });
        match started {
            Ok(controls) => Some(Self {
                controls,
                requests,
                shown: None,
            }),
            Err(error) => {
                eprintln!("ommata: media keys unavailable: {error:?}");
                None
            }
        }
    }

    /// The requests that came in since the last call.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.try_iter().collect()
    }

    /// Show `now` (or nothing playing) to the desktop.
    pub fn show(&mut self, now: Option<&Now>) {
        // The length arrives a moment after the video opens.
        let key = now.map(|now| {
            (
                now.title.clone(),
                now.playing,
                (now.duration * 1000.0) as u64,
            )
        });
        if key == self.shown {
            return;
        }
        self.shown = key;
        let result = match now {
            Some(now) => {
                let metadata = MediaMetadata {
                    title: Some(&now.title),
                    duration: Some(Duration::from_secs_f64(now.duration.max(0.0))),
                    ..Default::default()
                };
                let progress = Some(MediaPosition(Duration::from_secs_f64(
                    now.position.max(0.0),
                )));
                self.controls.set_metadata(metadata).and_then(|()| {
                    self.controls.set_playback(if now.playing {
                        MediaPlayback::Playing { progress }
                    } else {
                        MediaPlayback::Paused { progress }
                    })
                })
            }
            None => self.controls.set_playback(MediaPlayback::Stopped),
        };
        if let Err(error) = result {
            eprintln!("ommata: media keys: {error:?}");
        }
    }
}

/// The request behind a desktop event, if ommata handles it.
fn request(event: MediaControlEvent) -> Option<Request> {
    let seconds = |direction: SeekDirection, amount: f64| match direction {
        SeekDirection::Forward => amount,
        SeekDirection::Backward => -amount,
    };
    Some(match event {
        MediaControlEvent::Play => Request::Play,
        MediaControlEvent::Pause | MediaControlEvent::Stop => Request::Pause,
        MediaControlEvent::Toggle => Request::PlayPause,
        MediaControlEvent::Next => Request::Next,
        MediaControlEvent::Previous => Request::Previous,
        MediaControlEvent::Seek(direction) => Request::SeekBy(seconds(direction, 5.0)),
        MediaControlEvent::SeekBy(direction, amount) => {
            Request::SeekBy(seconds(direction, amount.as_secs_f64()))
        }
        MediaControlEvent::SetPosition(MediaPosition(time)) => Request::SeekTo(time.as_secs_f64()),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_desktop_events() {
        assert_eq!(request(MediaControlEvent::Toggle), Some(Request::PlayPause));
        assert_eq!(request(MediaControlEvent::Stop), Some(Request::Pause));
        assert_eq!(
            request(MediaControlEvent::SeekBy(
                SeekDirection::Backward,
                Duration::from_secs(10)
            )),
            Some(Request::SeekBy(-10.0))
        );
        assert_eq!(request(MediaControlEvent::Raise), None);
    }
}
