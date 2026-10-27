//! Moteur libVLC branché sur le contrat `mpv-*` de l'interface (LAST-504).
//!
//! L'interface continue d'envoyer `mpv-observe-prop`, `mpv-set-prop` et `mpv-command` ; quand le moteur
//! actif est libVLC, `PlayerEngine::handle` les traduit vers `VlcBackend` et un fil de sondage (250 ms)
//! émet `mpv-prop-change`, `mpv-event-video-ready` et `mpv-event-ended`. mpv reste créé mais inactif
//! (jamais de média) tant que libVLC est le moteur choisi ; le changement de moteur n'est accepté que lecteur arrêté.
//!
//! Ordre : `player-set-backend` voyage dans le même canal que les `mpv-*` et est traité dans l'ordre par la
//! boucle de messages du lecteur (`PlayerEngine::route`) : un `stop` puis `player-set-backend` puis `loadfile`
//! est vu dans cet ordre. Le fil du lecteur crée l'instance et le lecteur libVLC, demande au fil de l'interface
//! de créer la surface (réponse attendue au plus `UI_TIMEOUT`), puis seulement active le moteur et acquitte par
//! `player-backend` (moteur réellement actif, repli sur mpv compris).
//!
//! Fils : les objets Win32 (surface vidéo : création, affichage, taille) ne sont touchés que par le fil de
//! l'interface, via `PlayerEngine::on_ui_notice` / `fit` ; les autres fils ne font que demander (atomiques + notice).
//! Aucun appel libVLC bloquant (`stop`...) n'est fait sur le fil de l'interface.

use super::{
    communication::{Backend, CmdVal, InMsg, InMsgArgs, InMsgFn, PlayerResponse, PropKey, PropVal},
    player::{ResumeState, SharedVideoReadyState},
    vlc_backend::VlcBackend,
    vlc_ffi::{LibVlc, VlcInstance, VlcState},
    vlc_player::{VideoSurface, VlcPlayer},
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
/// Temps laissé au fil de l'interface pour créer la surface vidéo.
const UI_TIMEOUT: Duration = Duration::from_secs(2);
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

/// Doublure des dépendances natives pour tester l'ordonnancement sans DLL.
#[cfg(test)]
struct Stub {
    available: bool,
    build: Result<(), String>,
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
    /// Demande de surface au fil de l'interface : canal de sa réponse.
    surface_wanted: Mutex<Option<Sender<Result<(), String>>>>,
    surface: Mutex<Option<Arc<VideoSurface>>>,
    notifier: Mutex<Option<nwg::NoticeSender>>,
    #[cfg(test)]
    stub: Mutex<Option<Stub>>,
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
        #[cfg(test)]
        {
            if let Some(stub) = lock(&self.stub, "doublure").as_ref() {
                return stub.available;
            }
        }
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

    fn current(&self) -> Backend {
        if self.is_active() {
            Backend::Libvlc
        } else {
            Backend::Mpv
        }
    }

    /// Envoie `player-backend` avec le moteur actif (demande illisible : l'interface sait où on en est).
    pub fn acknowledge_current(&self) {
        self.send(&PlayerResponse::backend(self.current()));
    }

    /// Message `mpv-*` ou `player-set-backend` dans l'ordre d'arrivée (fil du lecteur). Renvoie le message
    /// que mpv doit traiter ; `None` : déjà traité (libVLC actif, ou changement de moteur).
    pub(super) fn route(
        self: &Arc<Self>,
        resume: &Mutex<ResumeState>,
        message: InMsg,
    ) -> Option<InMsg> {
        if let InMsg(InMsgFn::PlayerSetBackend, InMsgArgs::Backend(backend)) = message {
            self.set_backend(backend);
            return None;
        }
        if self.is_active() {
            // libVLC joue : seuls réglages et observations sont retenus, jamais le média (mpv reste inactif).
            if !matches!(message, InMsg(InMsgFn::MpvCommand, _)) {
                lock(resume, "reprise du lecteur").record_message(&message);
            }
            self.handle(message);
            return None;
        }
        lock(resume, "reprise du lecteur").record_message(&message);
        Some(message)
    }

    /// `player-set-backend`, sur le fil du lecteur, après les messages qui le précèdent. Refus journalisés :
    /// lecteur en cours, libVLC absente ou en échec ; dans tous les cas le moteur réellement actif est acquitté.
    fn set_backend(self: &Arc<Self>, wanted: Backend) {
        let effective = self.switch(wanted);
        self.send(&PlayerResponse::backend(effective));
    }

    fn switch(self: &Arc<Self>, wanted: Backend) -> Backend {
        if wanted == self.current() {
            return wanted;
        }
        if !self.is_stopped() {
            eprintln!("player-set-backend ignoré : lecteur en cours de lecture");
            return self.current();
        }
        match wanted {
            Backend::Mpv => {
                self.deactivate();
                Backend::Mpv
            }
            Backend::Libvlc => {
                if !self.libvlc_available() {
                    eprintln!("player-set-backend ignoré : libVLC indisponible, mpv conservé");
                    return Backend::Mpv;
                }
                match self.activate() {
                    Ok(()) => Backend::Libvlc,
                    Err(error) => {
                        report_error("lecteur: libVLC non activée, mpv conservé", &error);
                        Backend::Mpv
                    }
                }
            }
        }
    }

    /// Fil de l'interface : crée la surface demandée par le fil du lecteur et applique sa visibilité.
    pub fn on_ui_notice(&self) {
        if let Some(reply) = lock(&self.surface_wanted, "surface demandée").take() {
            let _ = reply.send(self.create_surface());
        }
        let visible = self.want_visible.load(Ordering::SeqCst) && self.is_active();
        if let Some(surface) = lock(&self.surface, "surface vidéo").as_ref() {
            surface.set_visible(visible);
        }
    }

    /// Fil de l'interface, à chaque redimensionnement : recale la surface.
    pub fn fit(&self) {
        if !self.want_visible.load(Ordering::SeqCst) {
            return;
        }
        if let Some(surface) = lock(&self.surface, "surface vidéo").as_ref() {
            surface.fit_to_parent();
        }
    }

    /// Fil de l'interface : crée la surface (cachée) si elle n'existe pas encore.
    fn create_surface(&self) -> Result<(), String> {
        let mut slot = lock(&self.surface, "surface vidéo");
        if slot.is_none() {
            let links = self.links.get().ok_or("moteur non attaché")?;
            let surface = VideoSurface::create(links.parent as HWND)?;
            surface.set_visible(false);
            *slot = Some(Arc::new(surface));
        }
        Ok(())
    }

    /// Fil du lecteur : demande la surface au fil de l'interface et attend sa réponse (au plus `UI_TIMEOUT`).
    fn ensure_surface(&self) -> Result<Arc<VideoSurface>, String> {
        let existing = || {
            lock(&self.surface, "surface vidéo")
                .as_ref()
                .map(Arc::clone)
        };
        if let Some(surface) = existing() {
            return Ok(surface);
        }
        let (reply, answer) = flume::bounded(1);
        *lock(&self.surface_wanted, "surface demandée") = Some(reply);
        self.notify_ui();
        let result = answer.recv_timeout(UI_TIMEOUT);
        lock(&self.surface_wanted, "surface demandée").take();
        match result {
            Ok(Err(error)) => Err(error),
            // Réponse tardive (surface créée juste après le délai) : elle est déjà en place.
            _ => existing().ok_or_else(|| "surface vidéo: fil de l'interface sans réponse".into()),
        }
    }

    /// Fil du lecteur : instance, lecteur et surface (créée par le fil de l'interface) pour libVLC.
    fn build_backend(self: &Arc<Self>) -> Result<(), String> {
        #[cfg(test)]
        {
            if let Some(stub) = lock(&self.stub, "doublure").as_ref() {
                return stub.build.clone();
            }
        }
        if lock(&self.backend, "lecteur libVLC").is_some() {
            return Ok(());
        }
        let api = self.library().ok_or("libVLC indisponible")?.clone();
        let instance = VlcInstance::new(api, VlcInstance::DEFAULT_ARGS)?;
        let player = instance.new_player()?;
        let surface = self.ensure_surface()?;
        let backend = Arc::new(VlcBackend::new(VlcPlayer::with_surface(player, surface))?);
        *lock(&self.backend, "lecteur libVLC") = Some(backend);
        let engine = Arc::clone(self);
        SERVICES.spawn("libvlc-poll", move || engine.poll_loop());
        Ok(())
    }

    fn activate(self: &Arc<Self>) -> Result<(), String> {
        let links = self.links.get().ok_or("moteur non attaché")?;
        self.build_backend()?;
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

    /// Fil du lecteur : libVLC est arrêtée ici (jamais sur le fil de l'interface, risque d'interblocage vout).
    fn deactivate(&self) {
        self.active.store(false, Ordering::SeqCst);
        self.want_visible.store(false, Ordering::SeqCst);
        if let Some(backend) = lock(&self.backend, "lecteur libVLC").clone() {
            backend.stop();
        }
        {
            let mut session = lock(&self.session, "session libVLC");
            session.media_loaded = false;
            session.awaiting_ready = false;
            session.observed.clear();
        }
        self.notify_ui();
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
                let Some((url, options)) = self.parse_load(&command) else {
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

    /// URL et options d'un chargement ; refus (schéma, commande) : journal et fin en erreur, jamais de silence.
    fn parse_load<'a>(&self, command: &'a CmdVal) -> Option<(&'a str, Vec<String>)> {
        match command.vlc_load() {
            Ok(load) => Some(load),
            Err(error) => {
                report_error("lecteur: chargement libVLC refusé", &error);
                self.send(&PlayerResponse::ended(mpv_end_file_reason::Error));
                None
            }
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

    fn engine_with(
        stub: Option<Stub>,
    ) -> (
        Arc<PlayerEngine>,
        Arc<Mutex<ResumeState>>,
        flume::Receiver<String>,
    ) {
        let engine = Arc::new(PlayerEngine::default());
        let (sender, receiver) = flume::unbounded();
        let resume = Arc::new(Mutex::new(ResumeState::default()));
        engine.attach(EngineLinks {
            parent: 0,
            rpc_response_sender: sender,
            video_ready_state: Arc::default(),
            resume: Arc::clone(&resume),
        });
        *lock(&engine.stub, "test") = stub;
        (engine, resume, receiver)
    }

    fn command(command: CmdVal) -> InMsg {
        InMsg(InMsgFn::MpvCommand, InMsgArgs::Cmd(command))
    }

    fn set_backend(backend: Backend) -> InMsg {
        InMsg(InMsgFn::PlayerSetBackend, InMsgArgs::Backend(backend))
    }

    fn acks(receiver: &flume::Receiver<String>) -> Vec<String> {
        receiver
            .try_iter()
            .filter(|text| text.contains("player-backend"))
            .map(|text| {
                if text.contains("libvlc") {
                    "libvlc"
                } else {
                    "mpv"
                }
                .to_owned()
            })
            .collect()
    }

    #[test]
    fn stop_puis_set_backend_puis_loadfile_sont_traites_dans_l_ordre() {
        let (engine, resume, receiver) = engine_with(Some(Stub {
            available: true,
            build: Ok(()),
        }));
        let load = command(CmdVal::Loadfile("http://h/a.mp4".into()));
        // Un premier film tourne sous mpv, puis l'interface l'arrête et demande libVLC avant de relancer.
        assert!(engine.route(&resume, load.clone()).is_some());
        assert!(!engine.is_stopped());
        assert!(engine.route(&resume, command(CmdVal::Stop)).is_some());
        assert!(engine
            .route(&resume, set_backend(Backend::Libvlc))
            .is_none());
        assert!(engine.is_active());
        // Le loadfile suivant est vu par libVLC, jamais par mpv.
        assert!(engine.route(&resume, load.clone()).is_none());
        assert_eq!(acks(&receiver), vec!["libvlc"]);
        // Retour à mpv : arrêt libVLC ordonné avant le loadfile suivant, que mpv reçoit.
        assert!(engine.route(&resume, command(CmdVal::Stop)).is_none());
        assert!(engine.route(&resume, set_backend(Backend::Mpv)).is_none());
        assert!(!engine.is_active());
        assert_eq!(engine.route(&resume, load.clone()), Some(load));
        assert_eq!(acks(&receiver), vec!["mpv"]);
    }

    #[test]
    fn set_backend_avant_l_arret_est_refuse_et_acquitte_le_moteur_reel() {
        let (engine, resume, receiver) = engine_with(Some(Stub {
            available: true,
            build: Ok(()),
        }));
        let load = command(CmdVal::Loadfile("http://h/a.mp4".into()));
        assert!(engine.route(&resume, load).is_some());
        assert!(engine
            .route(&resume, set_backend(Backend::Libvlc))
            .is_none());
        assert!(!engine.is_active());
        assert_eq!(acks(&receiver), vec!["mpv"]);
    }

    #[test]
    fn libvlc_absente_ou_en_echec_replie_sur_mpv_avec_accuse() {
        for stub in [
            Stub {
                available: false,
                build: Ok(()),
            },
            Stub {
                available: true,
                build: Err("surface vidéo: fil de l'interface sans réponse".into()),
            },
        ] {
            let (engine, resume, receiver) = engine_with(Some(stub));
            assert!(engine
                .route(&resume, set_backend(Backend::Libvlc))
                .is_none());
            assert!(!engine.is_active());
            assert_eq!(acks(&receiver), vec!["mpv"]);
            // Le loadfile suivant reste pour mpv.
            let load = command(CmdVal::Loadfile("http://h/a.mp4".into()));
            assert_eq!(engine.route(&resume, load.clone()), Some(load));
        }
    }

    #[test]
    fn demande_de_moteur_deja_actif_est_acquittee_sans_effet() {
        let (engine, resume, receiver) = engine_with(None);
        assert!(engine.route(&resume, set_backend(Backend::Mpv)).is_none());
        assert_eq!(acks(&receiver), vec!["mpv"]);
    }

    #[test]
    fn libvlc_refuse_un_schema_inconnu_par_une_fin_en_erreur() {
        let (engine, _resume, receiver) = engine_with(None);
        let accepted = CmdVal::Loadfile("https://h/a.mp4".into());
        assert!(engine.parse_load(&accepted).is_some());
        assert!(receiver.try_recv().is_err());
        let rejected = CmdVal::Loadfile("dvd:///D:".into());
        assert!(engine.parse_load(&rejected).is_none());
        let text = receiver.try_recv().expect("fin en erreur émise");
        assert!(
            text.contains("mpv-event-ended") && text.contains("error"),
            "{}",
            text
        );
    }
}
