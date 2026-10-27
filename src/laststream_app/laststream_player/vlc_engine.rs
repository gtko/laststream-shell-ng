//! Moteur libVLC branché sur le contrat `mpv-*` de l'interface (LAST-504).
//!
//! L'interface continue d'envoyer `mpv-observe-prop`, `mpv-set-prop` et `mpv-command` ; quand le moteur
//! actif est libVLC, `PlayerEngine::handle` les traduit vers `VlcBackend` et un fil de sondage (250 ms)
//! émet `mpv-prop-change`, `mpv-event-video-ready` et `mpv-event-ended`. mpv reste créé mais inactif
//! (jamais de média) tant que libVLC est le moteur choisi ; le changement de moteur n'est accepté que lecteur arrêté.
//!
//! Fils : les objets Win32 (surface vidéo : création, affichage, taille) ne sont touchés que par le fil de
//! l'interface, via `PlayerEngine::on_ui_notice` / `fit` ; les autres fils ne font que demander (atomiques + notice).

use super::{
    communication::{Backend, CmdVal, InMsg, InMsgArgs, InMsgFn, PlayerResponse, PropKey, PropVal},
    player::{ResumeState, SharedVideoReadyState},
    vlc_backend::VlcBackend,
    vlc_ffi::{LibVlc, VlcInstance, VlcState},
    vlc_player::VlcPlayer,
};
use crate::laststream_app::robust::{lock, report_error, SERVICES};
use crate::laststream_app::RPCResponse;
use flume::Sender;
use libmpv2::mpv_end_file_reason;
use native_windows_gui as nwg;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
use winapi::shared::windef::HWND;

const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Après un chargement, une fin ou une erreur « héritée » de l'ancien média est ignorée pendant ce délai.
const LOAD_GRACE: Duration = Duration::from_millis(1000);
/// Lecture sans piste vidéo (audio seul) : prête dès que le temps avance de cette durée.
const AUDIO_READY_MS: i64 = 500;

/// Ce que le moteur reçoit du lecteur mpv au démarrage (canaux et états partagés).
pub(super) struct EngineLinks {
    pub parent: isize,
    pub rpc_response_sender: Sender<String>,
    pub video_ready_state: SharedVideoReadyState,
    pub resume: Arc<Mutex<ResumeState>>,
}

#[derive(Default)]
struct Session {
    /// Propriétés observées et dernière valeur émise.
    observed: Vec<(String, Option<Value>)>,
    media_loaded: bool,
    awaiting_ready: bool,
    loaded_at: Option<Instant>,
}

#[derive(Default)]
pub struct PlayerEngine {
    links: OnceLock<EngineLinks>,
    library: OnceLock<Result<Arc<LibVlc>, String>>,
    /// Vrai quand libVLC est le moteur choisi.
    active: AtomicBool,
    /// Surface vidéo demandée visible (appliquée par le fil de l'interface).
    want_visible: AtomicBool,
    requested: Mutex<Option<Backend>>,
    notifier: Mutex<Option<nwg::NoticeSender>>,
    backend: Mutex<Option<Arc<VlcBackend>>>,
    session: Mutex<Session>,
}

impl PlayerEngine {
    pub(super) fn attach(&self, links: EngineLinks) {
        let _ = self.links.set(links);
    }

    /// Notice du fil de l'interface (`on_ui_notice` y est appelée).
    pub fn set_notifier(&self, sender: nwg::NoticeSender) {
        *lock(&self.notifier, "notice du moteur") = Some(sender);
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// libVLC 3.0.x chargeable ? Le résultat (succès ou échec) est mémorisé.
    pub fn libvlc_available(&self) -> bool {
        self.library().is_some()
    }

    fn library(&self) -> Option<&Arc<LibVlc>> {
        self.library.get_or_init(LibVlc::load).as_ref().ok()
    }

    /// Texte du champ `playerBackends` de la réponse `init` : tableau JSON des moteurs.
    pub fn backends_json(&self) -> String {
        json!(super::communication::PlayerCapabilities::new(self.libvlc_available()).backends)
            .to_string()
    }

    /// Envoie `player-capabilities` (fil dédié : le chargement de la DLL ne bloque pas l'IPC).
    pub fn send_capabilities(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        SERVICES.spawn("player-capabilities", move || {
            let response = PlayerResponse::capabilities(engine.libvlc_available());
            engine.send(&response);
        });
    }

    fn send(&self, response: &PlayerResponse<'_>) {
        if let Some(links) = self.links.get() {
            let _ = links
                .rpc_response_sender
                .send(RPCResponse::response_message(response.to_value()));
        }
    }

    fn notify_ui(&self) {
        if let Some(sender) = *lock(&self.notifier, "notice du moteur") {
            sender.notice();
        }
    }

    /// Lecteur arrêté : aucun média ni côté mpv ni côté libVLC.
    pub fn is_stopped(&self) -> bool {
        let vlc_idle = !lock(&self.session, "session libVLC").media_loaded;
        let mpv_idle = self
            .links
            .get()
            .is_none_or(|links| !lock(&links.resume, "reprise du lecteur").has_media());
        vlc_idle && mpv_idle
    }

    /// `player-set-backend` (fil IPC). Refus journalisés : lecteur en cours, libVLC absente.
    pub fn request_backend(&self, backend: Backend) {
        if !self.is_stopped() {
            eprintln!("player-set-backend ignoré : lecteur en cours de lecture");
            return;
        }
        if backend == Backend::Libvlc && !self.libvlc_available() {
            eprintln!("player-set-backend ignoré : libVLC indisponible, mpv conservé");
            return;
        }
        if self.is_active() == (backend == Backend::Libvlc) {
            return;
        }
        *lock(&self.requested, "moteur demandé") = Some(backend);
        self.notify_ui();
    }

    /// Fil de l'interface : applique le changement de moteur et la visibilité de la surface.
    pub fn on_ui_notice(self: &Arc<Self>) {
        let requested = lock(&self.requested, "moteur demandé").take();
        match requested {
            Some(Backend::Libvlc) => {
                if let Err(error) = self.activate() {
                    report_error("lecteur: libVLC non activée, mpv conservé", &error);
                }
            }
            Some(Backend::Mpv) => self.deactivate(),
            None => {}
        }
        if let Some(backend) = lock(&self.backend, "lecteur libVLC").clone() {
            backend
                .player()
                .surface
                .set_visible(self.want_visible.load(Ordering::SeqCst) && self.is_active());
        }
    }

    /// Fil de l'interface, à chaque redimensionnement : recale la surface.
    pub fn fit(&self) {
        if !self.want_visible.load(Ordering::SeqCst) {
            return;
        }
        if let Some(backend) = lock(&self.backend, "lecteur libVLC").clone() {
            backend.player().surface.fit_to_parent();
        }
    }

    fn activate(self: &Arc<Self>) -> Result<(), String> {
        let links = self.links.get().ok_or("moteur non attaché")?;
        let api = self.library().ok_or("libVLC indisponible")?.clone();
        let mut slot = lock(&self.backend, "lecteur libVLC");
        if slot.is_none() {
            let instance = VlcInstance::new(api, VlcInstance::DEFAULT_ARGS)?;
            let player = instance.new_player()?;
            let vlc_player = VlcPlayer::new(player, links.parent as HWND)?;
            let backend = Arc::new(VlcBackend::new(vlc_player)?);
            backend.player().surface.set_visible(false);
            *slot = Some(backend);
            let engine = Arc::clone(self);
            SERVICES.spawn("libvlc-poll", move || engine.poll_loop());
        }
        drop(slot);
        self.want_visible.store(false, Ordering::SeqCst);
        // Les réglages et observations déjà faits côté mpv (volume, props observées) passent à libVLC.
        let settings: Vec<InMsg> = lock(&links.resume, "reprise du lecteur")
            .replay_messages()
            .into_iter()
            .filter(|message| message.0 != InMsgFn::MpvCommand)
            .collect();
        self.active.store(true, Ordering::SeqCst);
        for message in settings {
            self.handle(message);
        }
        Ok(())
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::SeqCst);
        self.want_visible.store(false, Ordering::SeqCst);
        if let Some(backend) = lock(&self.backend, "lecteur libVLC").clone() {
            backend.stop();
        }
        let mut session = lock(&self.session, "session libVLC");
        session.media_loaded = false;
        session.awaiting_ready = false;
        session.observed.clear();
    }

    /// Valeur d'une propriété mpv côté libVLC (hors `VlcBackend` : `mpv-version`).
    fn property(&self, backend: &VlcBackend, name: &str) -> Option<Value> {
        if name == "mpv-version" {
            let version = self.library().map(|api| api.version()).unwrap_or_default();
            let number = version.split_whitespace().next().unwrap_or_default();
            return Some(json!(format!("libvlc {number}")));
        }
        backend.property(name).map(|value| normalize(name, value))
    }

    /// Message `mpv-*` de l'interface pendant que libVLC est le moteur actif.
    pub fn handle(&self, message: InMsg) {
        let Some(backend) = lock(&self.backend, "lecteur libVLC").clone() else {
            return;
        };
        match message {
            InMsg(InMsgFn::MpvObserveProp, InMsgArgs::ObProp(key)) => {
                let name = key.to_string();
                let mut session = lock(&self.session, "session libVLC");
                if !session.observed.iter().any(|(known, _)| *known == name) {
                    session.observed.push((name, None));
                }
            }
            InMsg(InMsgFn::MpvSetProp, InMsgArgs::StProp(key, value)) => {
                // Propriétés sans équivalent libVLC (secondary-sid, marges, tampon...) : refus silencieux.
                let _ = backend.set_property(&key.to_string(), &prop_value(&key, &value));
            }
            InMsg(InMsgFn::MpvCommand, InMsgArgs::Cmd(command)) => self.command(&backend, command),
            // `mpv-set-gpu-video-processing` : sans objet pour libVLC.
            _ => {}
        }
    }

    fn command(&self, backend: &Arc<VlcBackend>, command: CmdVal) {
        let Some(links) = self.links.get() else {
            return;
        };
        match &command {
            CmdVal::Stop => {
                backend.stop();
                self.want_visible.store(false, Ordering::SeqCst);
                {
                    let mut session = lock(&self.session, "session libVLC");
                    session.media_loaded = false;
                    session.awaiting_ready = false;
                }
                let load_id =
                    lock(&links.video_ready_state, "état du lecteur").begin_transition(false);
                self.send(&PlayerResponse::video_ready(load_id, false));
                self.notify_ui();
            }
            CmdVal::Loadfile(_) | CmdVal::LoadfileAt(_, _) => {
                let Some((url, options)) = command.vlc_load() else {
                    return;
                };
                let load_id = {
                    let mut ready = lock(&links.video_ready_state, "état du lecteur");
                    let load_id = ready.begin_transition(true);
                    ready.start_file();
                    ready.file_loaded();
                    load_id
                };
                self.send(&PlayerResponse::video_ready(load_id, false));
                let options: Vec<&str> = options.iter().map(String::as_str).collect();
                match backend.load(url, &options) {
                    Ok(()) => {
                        let mut session = lock(&self.session, "session libVLC");
                        session.media_loaded = true;
                        session.awaiting_ready = true;
                        session.loaded_at = Some(Instant::now());
                        for (_, last) in session.observed.iter_mut() {
                            *last = None;
                        }
                        drop(session);
                        self.want_visible.store(true, Ordering::SeqCst);
                        self.notify_ui();
                    }
                    Err(error) => {
                        report_error("lecteur: chargement libVLC", &error);
                        self.send(&PlayerResponse::ended(mpv_end_file_reason::Error));
                    }
                }
            }
            // Sous-titres externes : non pris en charge par libVLC ici.
            CmdVal::SubAdd(..) | CmdVal::SubRemove(_) => {}
        }
    }

    fn poll_loop(&self) {
        while SERVICES.sleep(POLL_INTERVAL) {
            if self.is_active() {
                self.poll_once();
            }
        }
    }

    fn poll_once(&self) {
        let Some(backend) = lock(&self.backend, "lecteur libVLC").clone() else {
            return;
        };
        let Some(links) = self.links.get() else {
            return;
        };
        let (names, loaded_at) = {
            let session = lock(&self.session, "session libVLC");
            if !session.media_loaded {
                return;
            }
            let names: Vec<String> = session.observed.iter().map(|(n, _)| n.clone()).collect();
            (names, session.loaded_at)
        };
        for name in names {
            let Some(value) = self.property(&backend, &name) else {
                continue;
            };
            let changed = {
                let mut session = lock(&self.session, "session libVLC");
                match session
                    .observed
                    .iter_mut()
                    .find(|(known, _)| *known == name)
                {
                    Some((_, last)) if last.as_ref() != Some(&value) => {
                        *last = Some(value.clone());
                        true
                    }
                    _ => false,
                }
            };
            if changed {
                self.send(&PlayerResponse::prop_change(&name, value));
            }
        }

        let player = &backend.player().player;
        let awaiting = lock(&self.session, "session libVLC").awaiting_ready;
        if awaiting {
            let started = player.video_size().is_some()
                || (player.state() == VlcState::Playing && player.time_ms() >= AUDIO_READY_MS);
            if started {
                lock(&self.session, "session libVLC").awaiting_ready = false;
                let ready = lock(&links.video_ready_state, "état du lecteur").playback_restarted();
                if let Some(load_id) = ready {
                    self.send(&PlayerResponse::video_ready(load_id, true));
                }
            }
        }

        if loaded_at.is_some_and(|at| at.elapsed() < LOAD_GRACE) {
            return;
        }
        let reason = if backend.has_error() {
            Some(mpv_end_file_reason::Error)
        } else if backend.property("eof-reached") == Some(json!(true)) {
            Some(mpv_end_file_reason::Eof)
        } else {
            None
        };
        if let Some(reason) = reason {
            {
                let mut session = lock(&self.session, "session libVLC");
                session.media_loaded = false;
                session.awaiting_ready = false;
            }
            self.send(&PlayerResponse::ended(reason));
            self.want_visible.store(false, Ordering::SeqCst);
            self.notify_ui();
        }
    }
}

/// Une piste désactivée est `"no"` côté mpv (`false` côté `VlcBackend`).
fn normalize(name: &str, value: Value) -> Value {
    if matches!(name, "sid" | "aid") && value == json!(false) {
        json!("no")
    } else {
        value
    }
}

fn prop_value(_key: &PropKey, value: &PropVal) -> Value {
    match value {
        PropVal::Bool(flag) => json!(flag),
        PropVal::Str(text) => json!(text),
        PropVal::Num(number) => json!(number),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piste_desactivee_donne_no_comme_mpv() {
        assert_eq!(normalize("sid", json!(false)), json!("no"));
        assert_eq!(normalize("aid", json!(false)), json!("no"));
        assert_eq!(normalize("sid", json!(3)), json!(3));
        assert_eq!(normalize("pause", json!(false)), json!(false));
    }

    #[test]
    fn valeurs_de_propriete_vers_json() {
        let key = PropKey::Bool(super::super::communication::BoolProp::Pause);
        assert_eq!(prop_value(&key, &PropVal::Bool(true)), json!(true));
        assert_eq!(prop_value(&key, &PropVal::Num(1.5)), json!(1.5));
        assert_eq!(prop_value(&key, &PropVal::Str("no".into())), json!("no"));
    }

    #[test]
    fn changement_de_moteur_refuse_sans_libvlc_attachee() {
        // Sans liens ni DLL : rien n'est demandé, mpv reste le moteur.
        let engine = PlayerEngine::default();
        engine.request_backend(Backend::Mpv);
        assert!(!engine.is_active());
        assert!(engine.is_stopped());
        assert!(lock(&engine.requested, "test").is_none());
    }
}
