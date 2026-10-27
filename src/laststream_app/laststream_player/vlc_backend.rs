//! Adaptateur libVLC vers le vocabulaire des propriétés mpv (LAST-504).
//!
//! `VlcBackend` expose, pour chaque propriété que l'interface observe sur mpv, la valeur JSON
//! équivalente côté libVLC : `time-pos`, `duration` (secondes), `pause`, `volume`, `mute`, `speed`,
//! `eof-reached`, `seeking`, `paused-for-cache`, `track-list` (`{id,type,lang,title,selected}`),
//! `aid`, `sid`, `video-params` (`{w,h}`). Branché à l'IPC par `vlc_engine.rs`.
//!
//! Limites connues de libVLC 3.0 : les pistes n'ont qu'un libellé (« Piste 1 - [Français] »), pas de
//! code de langue ISO ; `lang` reçoit le texte entre crochets s'il existe, `title` le libellé complet.

use super::{
    vlc_ffi::{
        VlcEvent, VlcEventCallback, VlcState, VlcTrack, EVENT_MEDIA_PLAYER_BUFFERING,
        EVENT_MEDIA_PLAYER_ENCOUNTERED_ERROR, EVENT_MEDIA_PLAYER_END_REACHED,
        EVENT_MEDIA_PLAYER_PLAYING,
    },
    vlc_player::VlcPlayer,
};
use serde_json::{json, Value};
use std::{
    convert::TryFrom,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// Un saut de position est vu comme « seeking » pendant ce délai.
const SEEK_WINDOW: Duration = Duration::from_millis(600);

const WATCHED_EVENTS: [i32; 4] = [
    EVENT_MEDIA_PLAYER_BUFFERING,
    EVENT_MEDIA_PLAYER_PLAYING,
    EVENT_MEDIA_PLAYER_END_REACHED,
    EVENT_MEDIA_PLAYER_ENCOUNTERED_ERROR,
];

/// État alimenté par les callbacks libVLC (fil libVLC) : uniquement des atomiques.
struct SharedState {
    ended: AtomicBool,
    errored: AtomicBool,
    /// Remplissage du tampon en % (100 = lecture normale).
    buffering: AtomicU32,
}

unsafe extern "C" fn on_event(event: *const VlcEvent, user: *mut c_void) {
    if event.is_null() || user.is_null() {
        return;
    }
    // SAFETY: `user` est le pointeur `Arc<SharedState>` passé à `attach`, valide jusqu'à `detach` (Drop).
    let state = &*(user as *const SharedState);
    let event = &*event;
    match event.kind {
        EVENT_MEDIA_PLAYER_END_REACHED => state.ended.store(true, Ordering::Relaxed),
        EVENT_MEDIA_PLAYER_ENCOUNTERED_ERROR => state.errored.store(true, Ordering::Relaxed),
        EVENT_MEDIA_PLAYER_BUFFERING => {
            let percent = event.payload.new_cache;
            state
                .buffering
                .store(percent.clamp(0.0, 100.0) as u32, Ordering::Relaxed);
        }
        EVENT_MEDIA_PLAYER_PLAYING => {
            state.ended.store(false, Ordering::Relaxed);
            state.errored.store(false, Ordering::Relaxed);
        }
        _ => {}
    }
}

pub struct VlcBackend {
    player: VlcPlayer,
    shared: Arc<SharedState>,
    /// Pointeur donné à libVLC (une référence de `shared`), repris dans `Drop`.
    user: *const SharedState,
    pause_requested: AtomicBool,
    speed: Mutex<f64>,
    last_seek: Mutex<Option<Instant>>,
}

// SAFETY: `user` n'est qu'une référence comptée sur `SharedState` (atomiques) ; le reste est Send/Sync.
unsafe impl Send for VlcBackend {}
unsafe impl Sync for VlcBackend {}

impl VlcBackend {
    pub fn new(player: VlcPlayer) -> Result<Self, String> {
        let shared = Arc::new(SharedState {
            ended: AtomicBool::new(false),
            errored: AtomicBool::new(false),
            buffering: AtomicU32::new(100),
        });
        let user = Arc::into_raw(shared.clone());
        let callback: VlcEventCallback = on_event;
        for (index, event) in WATCHED_EVENTS.iter().enumerate() {
            // SAFETY: `user` reste valide jusqu'au `detach` de `Drop` ; le callback ne touche que des atomiques.
            let attached = unsafe { player.player.attach(*event, callback, user as *mut c_void) };
            if let Err(error) = attached {
                for attached_event in &WATCHED_EVENTS[..index] {
                    // SAFETY: mêmes arguments que l'`attach` correspondant.
                    unsafe {
                        player
                            .player
                            .detach(*attached_event, callback, user as *mut c_void)
                    };
                }
                // SAFETY: reprise de la référence créée par `into_raw` ci-dessus, plus aucun callback actif.
                drop(unsafe { Arc::from_raw(user) });
                return Err(error);
            }
        }
        Ok(Self {
            player,
            shared,
            user,
            pause_requested: AtomicBool::new(false),
            speed: Mutex::new(1.0),
            last_seek: Mutex::new(None),
        })
    }

    pub fn player(&self) -> &VlcPlayer {
        &self.player
    }

    /// Charge `url` et lance la lecture.
    pub fn load(&self, url: &str, options: &[&str]) -> Result<(), String> {
        self.shared.ended.store(false, Ordering::Relaxed);
        self.shared.errored.store(false, Ordering::Relaxed);
        self.pause_requested.store(false, Ordering::Relaxed);
        self.player.player.set_location(url, options)?;
        self.player.player.play()
    }

    pub fn stop(&self) {
        self.player.player.stop();
    }

    /// Valeur de la propriété mpv `name`, ou `None` si elle n'a pas d'équivalent.
    pub fn property(&self, name: &str) -> Option<Value> {
        let p = &self.player.player;
        Some(match name {
            "time-pos" => seconds(p.time_ms()),
            "duration" => seconds(p.length_ms()),
            "pause" => {
                json!(self.pause_requested.load(Ordering::Relaxed) || p.state() == VlcState::Paused)
            }
            "volume" => json!(f64::from(p.volume().max(0))),
            "mute" => json!(p.is_muted()),
            "speed" => json!(*self.speed.lock().ok()?),
            "eof-reached" => {
                json!(self.shared.ended.load(Ordering::Relaxed) || p.state() == VlcState::Ended)
            }
            "seeking" => json!(self.is_seeking()),
            "paused-for-cache" => json!(
                self.shared.buffering.load(Ordering::Relaxed) < 100
                    && p.state() == VlcState::Buffering
            ),
            "track-list" => self.track_list(),
            "aid" => track_selection(p.audio_track()),
            "sid" => track_selection(p.subtitle_track()),
            "video-params" => match p.video_size() {
                Some((w, h)) => json!({ "w": w, "h": h }),
                None => Value::Null,
            },
            _ => return None,
        })
    }

    /// Applique une écriture de propriété mpv. Renvoie `false` si inconnue ou refusée.
    pub fn set_property(&self, name: &str, value: &Value) -> bool {
        let p = &self.player.player;
        match name {
            "pause" => value.as_bool().is_some_and(|pause| {
                self.pause_requested.store(pause, Ordering::Relaxed);
                p.set_pause(pause);
                true
            }),
            "time-pos" => value.as_f64().is_some_and(|secs| {
                self.seek_to(secs);
                true
            }),
            "volume" => value
                .as_f64()
                .is_some_and(|volume| p.set_volume(volume.round() as i32)),
            "mute" => value.as_bool().is_some_and(|mute| {
                p.set_mute(mute);
                true
            }),
            "speed" => value.as_f64().is_some_and(|speed| {
                let applied = p.set_rate(speed as f32);
                if applied {
                    if let Ok(mut current) = self.speed.lock() {
                        *current = speed;
                    }
                }
                applied
            }),
            "aid" => select_track(value, |id| p.set_audio_track(id)),
            "sid" => select_track(value, |id| p.set_subtitle_track(id)),
            _ => false,
        }
    }

    pub fn seek_to(&self, secs: f64) {
        if let Ok(mut last) = self.last_seek.lock() {
            *last = Some(Instant::now());
        }
        self.player
            .player
            .set_time_ms((secs.max(0.0) * 1000.0) as i64);
    }

    fn is_seeking(&self) -> bool {
        let recent = self
            .last_seek
            .lock()
            .ok()
            .and_then(|last| *last)
            .is_some_and(|at| at.elapsed() < SEEK_WINDOW);
        recent || self.player.player.state() == VlcState::Opening
    }

    fn track_list(&self) -> Value {
        let p = &self.player.player;
        build_track_list(
            &p.audio_tracks(),
            &p.subtitle_tracks(),
            p.audio_track(),
            p.subtitle_track(),
            p.video_size().is_some(),
        )
    }

    /// Vrai si libVLC a signalé une erreur de lecture (à traduire en `mpv-event-ended` avec erreur).
    pub fn has_error(&self) -> bool {
        self.shared.errored.load(Ordering::Relaxed) || self.player.player.state() == VlcState::Error
    }
}

impl Drop for VlcBackend {
    fn drop(&mut self) {
        let callback: VlcEventCallback = on_event;
        for event in WATCHED_EVENTS {
            // SAFETY: mêmes arguments que l'`attach` de `new`.
            unsafe {
                self.player
                    .player
                    .detach(event, callback, self.user as *mut c_void)
            };
        }
        // SAFETY: référence créée par `into_raw` dans `new` ; plus aucun callback n'est attaché.
        drop(unsafe { Arc::from_raw(self.user) });
    }
}

fn seconds(ms: i64) -> Value {
    if ms < 0 {
        Value::Null
    } else {
        json!(ms as f64 / 1000.0)
    }
}

/// `aid`/`sid` : l'identifiant libVLC, ou `false` (équivalent du « no » de mpv) si désactivé.
fn track_selection(id: i32) -> Value {
    if id < 0 {
        json!(false)
    } else {
        json!(id)
    }
}

/// `false`/`"no"` désactive la piste (id libVLC `-1`), un entier la sélectionne.
fn select_track(value: &Value, apply: impl FnOnce(i32) -> bool) -> bool {
    let id = match value {
        Value::Bool(false) => -1,
        Value::String(text) if text == "no" => -1,
        other => match other.as_i64().and_then(|id| i32::try_from(id).ok()) {
            Some(id) => id,
            None => return false,
        },
    };
    apply(id)
}

/// Langue lue entre crochets dans un libellé libVLC (« Track 1 - [Français] »).
fn language_from_label(label: &str) -> Option<&str> {
    let start = label.rfind('[')?;
    let end = label[start..].find(']')? + start;
    let lang = label[start + 1..end].trim();
    (!lang.is_empty()).then_some(lang)
}

/// `track-list` au format mpv. Les entrées « Désactivé » (id négatif) sont écartées.
fn build_track_list(
    audio: &[VlcTrack],
    subtitles: &[VlcTrack],
    selected_audio: i32,
    selected_subtitle: i32,
    has_video: bool,
) -> Value {
    let mut tracks = Vec::new();
    if has_video {
        tracks.push(json!({ "id": 1, "type": "video", "lang": "", "title": "", "selected": true }));
    }
    let mut push = |list: &[VlcTrack], kind: &str, selected: i32| {
        for track in list.iter().filter(|track| track.id >= 0) {
            tracks.push(json!({
                "id": track.id,
                "type": kind,
                "lang": language_from_label(&track.name).unwrap_or(""),
                "title": track.name,
                "selected": track.id == selected,
            }));
        }
    };
    push(audio, "audio", selected_audio);
    push(subtitles, "sub", selected_subtitle);
    Value::Array(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: i32, name: &str) -> VlcTrack {
        VlcTrack {
            id,
            name: name.into(),
        }
    }

    #[test]
    fn la_liste_de_pistes_suit_le_format_mpv() {
        let audio = [track(-1, "Disable"), track(2, "Piste 1 - [Français]")];
        let subs = [track(-1, "Disable"), track(5, "Piste 1")];
        let list = build_track_list(&audio, &subs, 2, -1, true);
        assert_eq!(
            list,
            json!([
                { "id": 1, "type": "video", "lang": "", "title": "", "selected": true },
                { "id": 2, "type": "audio", "lang": "Français", "title": "Piste 1 - [Français]", "selected": true },
                { "id": 5, "type": "sub", "lang": "", "title": "Piste 1", "selected": false },
            ])
        );
    }

    #[test]
    fn sans_video_pas_de_piste_video() {
        assert_eq!(build_track_list(&[], &[], -1, -1, false), json!([]));
    }

    #[test]
    fn langue_entre_crochets() {
        assert_eq!(language_from_label("Track 2 - [English]"), Some("English"));
        assert_eq!(language_from_label("Track 2"), None);
        assert_eq!(language_from_label("Track [ ]"), None);
    }

    #[test]
    fn selection_des_pistes() {
        assert_eq!(track_selection(-1), json!(false));
        assert_eq!(track_selection(3), json!(3));
        let mut applied = None;
        assert!(select_track(&json!("no"), |id| {
            applied = Some(id);
            true
        }));
        assert_eq!(applied, Some(-1));
        assert!(select_track(&json!(4), |id| id == 4));
        assert!(!select_track(&json!("abc"), |_| true));
    }

    #[test]
    fn temps_inconnu_donne_null() {
        assert_eq!(seconds(-1), Value::Null);
        assert_eq!(seconds(1500), json!(1.5));
    }
}
