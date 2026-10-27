//! FFI minimale de libVLC 3.0.x, chargée dynamiquement (LAST-504).
//!
//! `libvlc.dll` n'est jamais liée à l'exécutable : il démarre sans DLL VLC. Le chargement se fait à
//! l'exécution avec `libloading`, dans cet ordre :
//! 1. `<dossier de l'exe>\vlc\libvlc.dll` (VLC embarqué, plugins dans `vlc\plugins`) ;
//! 2. le VLC x64 installé (`HKLM\SOFTWARE\VideoLAN\VLC`, valeur `InstallDir`, puis `Program Files`).
//!
//! Toute version qui ne commence pas par `3.0` est refusée. Aucune fonction ne panique : les échecs
//! reviennent en `Result<_, String>`.
//!
//! Ce module n'utilise que `std`, `libloading`, `winreg` : il est inclus tel quel par
//! `examples/vlc_probe.rs`.

#![allow(dead_code)] // Surface FFI complète, partagée avec examples/vlc_probe.rs : tout n'est pas utilisé par l'app.

use libloading::Library;
use std::{
    env,
    ffi::{c_void, CStr, CString},
    os::raw::{c_char, c_float, c_int, c_uint},
    path::{Path, PathBuf},
    sync::Arc,
};

/// `LOAD_WITH_ALTERED_SEARCH_PATH` : `libvlccore.dll` est cherchée à côté de `libvlc.dll`.
const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;

pub const EVENT_MEDIA_PLAYER_OPENING: c_int = 258;
pub const EVENT_MEDIA_PLAYER_BUFFERING: c_int = 259;
pub const EVENT_MEDIA_PLAYER_PLAYING: c_int = 260;
pub const EVENT_MEDIA_PLAYER_PAUSED: c_int = 261;
pub const EVENT_MEDIA_PLAYER_STOPPED: c_int = 262;
pub const EVENT_MEDIA_PLAYER_END_REACHED: c_int = 265;
pub const EVENT_MEDIA_PLAYER_ENCOUNTERED_ERROR: c_int = 266;
pub const EVENT_MEDIA_PLAYER_TIME_CHANGED: c_int = 267;
pub const EVENT_MEDIA_PLAYER_LENGTH_CHANGED: c_int = 273;

/// `libvlc_state_t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VlcState {
    NothingSpecial,
    Opening,
    Buffering,
    Playing,
    Paused,
    Stopped,
    Ended,
    Error,
}

impl VlcState {
    fn from_raw(raw: c_int) -> Self {
        match raw {
            1 => Self::Opening,
            2 => Self::Buffering,
            3 => Self::Playing,
            4 => Self::Paused,
            5 => Self::Stopped,
            6 => Self::Ended,
            7 => Self::Error,
            _ => Self::NothingSpecial,
        }
    }
}

/// Début de `libvlc_event_t` : type, objet émetteur, puis l'union (alignée sur 8 octets).
#[repr(C)]
pub struct VlcEvent {
    pub kind: c_int,
    pub object: *mut c_void,
    pub payload: VlcEventPayload,
}

/// Charges utiles utiles : `new_cache` (Buffering, en %), `new_time` (TimeChanged, en ms).
#[repr(C)]
#[derive(Clone, Copy)]
pub union VlcEventPayload {
    pub new_cache: c_float,
    pub new_time: i64,
}

pub type VlcEventCallback = unsafe extern "C" fn(*const VlcEvent, *mut c_void);

/// `libvlc_track_description_t`.
#[repr(C)]
pub struct RawTrackDescription {
    id: c_int,
    name: *mut c_char,
    next: *mut RawTrackDescription,
}

macro_rules! libvlc_api {
    ($( $field:ident = $symbol:literal : fn($($arg:ty),*) $(-> $ret:ty)? ;)*) => {
        /// Table des fonctions libVLC résolues ; garde la bibliothèque chargée.
        pub struct LibVlc {
            _library: Library,
            $( $field: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
        }

        impl LibVlc {
            fn resolve(library: Library) -> Result<Self, String> {
                $(
                    // SAFETY: la signature déclarée est celle de l'API publique libVLC 3.0 ; le pointeur
                    // copié reste valide tant que `_library` vit, rangée dans la même structure.
                    let $field = unsafe {
                        *library
                            .get::<unsafe extern "C" fn($($arg),*) $(-> $ret)?>(
                                concat!($symbol, "\0").as_bytes(),
                            )
                            .map_err(|error| format!("libvlc: symbole {} absent: {error}", $symbol))?
                    };
                )*
                Ok(Self { _library: library, $( $field, )* })
            }
        }
    };
}

libvlc_api! {
    new = "libvlc_new": fn(c_int, *const *const c_char) -> *mut c_void;
    release = "libvlc_release": fn(*mut c_void);
    get_version = "libvlc_get_version": fn() -> *const c_char;
    errmsg = "libvlc_errmsg": fn() -> *const c_char;
    media_new_location = "libvlc_media_new_location": fn(*mut c_void, *const c_char) -> *mut c_void;
    media_add_option = "libvlc_media_add_option": fn(*mut c_void, *const c_char);
    media_release = "libvlc_media_release": fn(*mut c_void);
    player_new = "libvlc_media_player_new": fn(*mut c_void) -> *mut c_void;
    player_release = "libvlc_media_player_release": fn(*mut c_void);
    player_set_media = "libvlc_media_player_set_media": fn(*mut c_void, *mut c_void);
    player_set_hwnd = "libvlc_media_player_set_hwnd": fn(*mut c_void, *mut c_void);
    player_play = "libvlc_media_player_play": fn(*mut c_void) -> c_int;
    player_set_pause = "libvlc_media_player_set_pause": fn(*mut c_void, c_int);
    player_stop = "libvlc_media_player_stop": fn(*mut c_void);
    player_get_time = "libvlc_media_player_get_time": fn(*mut c_void) -> i64;
    player_set_time = "libvlc_media_player_set_time": fn(*mut c_void, i64);
    player_get_length = "libvlc_media_player_get_length": fn(*mut c_void) -> i64;
    player_is_playing = "libvlc_media_player_is_playing": fn(*mut c_void) -> c_int;
    player_get_state = "libvlc_media_player_get_state": fn(*mut c_void) -> c_int;
    player_set_rate = "libvlc_media_player_set_rate": fn(*mut c_void, c_float) -> c_int;
    player_event_manager = "libvlc_media_player_event_manager": fn(*mut c_void) -> *mut c_void;
    audio_get_volume = "libvlc_audio_get_volume": fn(*mut c_void) -> c_int;
    audio_set_volume = "libvlc_audio_set_volume": fn(*mut c_void, c_int) -> c_int;
    audio_get_mute = "libvlc_audio_get_mute": fn(*mut c_void) -> c_int;
    audio_set_mute = "libvlc_audio_set_mute": fn(*mut c_void, c_int);
    audio_get_track = "libvlc_audio_get_track": fn(*mut c_void) -> c_int;
    audio_set_track = "libvlc_audio_set_track": fn(*mut c_void, c_int) -> c_int;
    video_get_spu = "libvlc_video_get_spu": fn(*mut c_void) -> c_int;
    video_set_spu = "libvlc_video_set_spu": fn(*mut c_void, c_int) -> c_int;
    audio_get_track_description = "libvlc_audio_get_track_description": fn(*mut c_void) -> *mut RawTrackDescription;
    video_get_spu_description = "libvlc_video_get_spu_description": fn(*mut c_void) -> *mut RawTrackDescription;
    track_description_list_release = "libvlc_track_description_list_release": fn(*mut RawTrackDescription);
    video_get_size = "libvlc_video_get_size": fn(*mut c_void, c_uint, *mut c_uint, *mut c_uint) -> c_int;
    event_attach = "libvlc_event_attach": fn(*mut c_void, c_int, VlcEventCallback, *mut c_void) -> c_int;
    event_detach = "libvlc_event_detach": fn(*mut c_void, c_int, VlcEventCallback, *mut c_void);
    video_set_mouse_input = "libvlc_video_set_mouse_input": fn(*mut c_void, c_uint);
    video_set_key_input = "libvlc_video_set_key_input": fn(*mut c_void, c_uint);
}

/// Vrai pour une version `3.0.x` (la ligne 3.0 est la seule dont l'ABI est déclarée ici).
pub fn is_supported_version(version: &str) -> bool {
    let mut parts = version.split(|c: char| !c.is_ascii_digit());
    parts.next() == Some("3") && parts.next() == Some("0")
}

/// Dossiers candidats, par priorité : VLC embarqué, puis VLC installé.
pub fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(exe_dir) = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        dirs.push(exe_dir.join("vlc"));
    }
    if let Some(install_dir) = registry_install_dir() {
        dirs.push(install_dir);
    }
    for variable in ["ProgramFiles", "ProgramW6432"] {
        if let Some(program_files) = env::var_os(variable) {
            let dir = PathBuf::from(program_files).join(r"VideoLAN\VLC");
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

fn registry_install_dir() -> Option<PathBuf> {
    use winreg::{
        enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY},
        RegKey,
    };
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(r"SOFTWARE\VideoLAN\VLC", KEY_READ | KEY_WOW64_64KEY)
        .ok()?;
    key.get_value::<String, _>("InstallDir")
        .ok()
        .map(PathBuf::from)
}

impl LibVlc {
    /// Cherche libVLC dans `candidate_dirs()` ; renvoie la première installation 3.0.x utilisable.
    pub fn load() -> Result<Arc<Self>, String> {
        let mut errors = Vec::new();
        for dir in candidate_dirs() {
            if !dir.join("libvlc.dll").is_file() {
                continue;
            }
            match Self::load_from_dir(&dir) {
                Ok(api) => return Ok(api),
                Err(error) => errors.push(format!("{}: {error}", dir.display())),
            }
        }
        if errors.is_empty() {
            Err("libvlc.dll introuvable (ni vlc\\ à côté de l'exe, ni VLC installé)".into())
        } else {
            Err(errors.join(" | "))
        }
    }

    /// Charge `<dir>\libvlc.dll`. `<dir>\plugins` est annoncé à libVLC via `VLC_PLUGIN_PATH`.
    pub fn load_from_dir(dir: &Path) -> Result<Arc<Self>, String> {
        let dll = dir.join("libvlc.dll");
        if !dll.is_file() {
            return Err(format!("{} introuvable", dll.display()));
        }
        let plugins = dir.join("plugins");
        if plugins.is_dir() {
            env::set_var("VLC_PLUGIN_PATH", &plugins);
        }
        // SAFETY: charger libvlc.dll exécute son DllMain, comme tout LoadLibrary ; le chemin est absolu
        // et l'altération du chemin de recherche ne sert qu'à trouver libvlccore.dll à côté.
        let library = unsafe {
            libloading::os::windows::Library::load_with_flags(&dll, LOAD_WITH_ALTERED_SEARCH_PATH)
        }
        .map(Library::from)
        .map_err(|error| format!("chargement de {} impossible: {error}", dll.display()))?;
        let api = Self::resolve(library)?;
        let version = api.version();
        if !is_supported_version(&version) {
            return Err(format!(
                "version libVLC non supportée: {version} (3.0.x requis)"
            ));
        }
        Ok(Arc::new(api))
    }

    pub fn version(&self) -> String {
        // SAFETY: libvlc_get_version renvoie une chaîne statique terminée par NUL.
        unsafe { c_string((self.get_version)()) }.unwrap_or_default()
    }

    fn last_error(&self) -> String {
        // SAFETY: libvlc_errmsg renvoie NULL ou une chaîne locale au fil, valide jusqu'à l'appel suivant.
        unsafe { c_string((self.errmsg)()) }.unwrap_or_else(|| "erreur libVLC inconnue".into())
    }
}

/// # Safety
/// `raw` doit être NULL ou pointer une chaîne C terminée par NUL valide.
unsafe fn c_string(raw: *const c_char) -> Option<String> {
    if raw.is_null() {
        None
    } else {
        Some(CStr::from_ptr(raw).to_string_lossy().into_owned())
    }
}

fn to_cstring(value: &str, what: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| format!("{what} contient un octet NUL"))
}

/// Instance libVLC (`libvlc_instance_t`).
pub struct VlcInstance {
    api: Arc<LibVlc>,
    raw: *mut c_void,
}

// SAFETY: libVLC documente ses instances comme utilisables depuis plusieurs fils.
unsafe impl Send for VlcInstance {}
unsafe impl Sync for VlcInstance {}

impl VlcInstance {
    /// Options par défaut : sans OSD ni titre, sans prévisualisation ni statistiques.
    pub const DEFAULT_ARGS: &'static [&'static str] = &[
        "--quiet",
        "--no-video-title-show",
        "--no-osd",
        "--no-snapshot-preview",
        "--no-stats",
    ];

    pub fn new(api: Arc<LibVlc>, args: &[&str]) -> Result<Arc<Self>, String> {
        let c_args = args
            .iter()
            .map(|arg| to_cstring(arg, "argument libVLC"))
            .collect::<Result<Vec<_>, _>>()?;
        let pointers: Vec<*const c_char> = c_args.iter().map(|arg| arg.as_ptr()).collect();
        // SAFETY: `pointers` référence des chaînes C vivantes pendant l'appel ; argc correspond à sa longueur.
        let raw = unsafe { (api.new)(pointers.len() as c_int, pointers.as_ptr()) };
        if raw.is_null() {
            return Err(format!("libvlc_new a échoué: {}", api.last_error()));
        }
        Ok(Arc::new(Self { api, raw }))
    }

    pub fn api(&self) -> &Arc<LibVlc> {
        &self.api
    }

    pub fn new_player(self: &Arc<Self>) -> Result<VlcMediaPlayer, String> {
        // SAFETY: `self.raw` est une instance valide tant que `self` vit.
        let raw = unsafe { (self.api.player_new)(self.raw) };
        if raw.is_null() {
            return Err(format!(
                "libvlc_media_player_new a échoué: {}",
                self.api.last_error()
            ));
        }
        Ok(VlcMediaPlayer {
            api: self.api.clone(),
            raw,
            instance: self.clone(),
        })
    }
}

impl Drop for VlcInstance {
    fn drop(&mut self) {
        // SAFETY: `raw` n'est libérée qu'ici ; les lecteurs détiennent un Arc sur l'instance.
        unsafe { (self.api.release)(self.raw) };
    }
}

/// Lecteur libVLC (`libvlc_media_player_t`).
pub struct VlcMediaPlayer {
    api: Arc<LibVlc>,
    raw: *mut c_void,
    instance: Arc<VlcInstance>,
}

// SAFETY: l'API libVLC des lecteurs est thread-safe (verrou interne).
unsafe impl Send for VlcMediaPlayer {}
unsafe impl Sync for VlcMediaPlayer {}

/// Piste audio ou sous-titre telle que décrite par libVLC (identifiant + libellé).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VlcTrack {
    pub id: i32,
    pub name: String,
}

impl VlcMediaPlayer {
    /// Associe `url` (MRL) avec ses `options` (ex. `:network-caching=1500`) au lecteur.
    pub fn set_location(&self, url: &str, options: &[&str]) -> Result<(), String> {
        let url = to_cstring(url, "URL")?;
        let options = options
            .iter()
            .map(|option| to_cstring(option, "option média"))
            .collect::<Result<Vec<_>, _>>()?;
        // SAFETY: l'instance est vivante (Arc) ; le média est libéré après `set_media`, qui garde sa propre référence.
        unsafe {
            let media = (self.api.media_new_location)(self.instance.raw, url.as_ptr());
            if media.is_null() {
                return Err(format!(
                    "libvlc_media_new_location a échoué: {}",
                    self.api.last_error()
                ));
            }
            for option in &options {
                (self.api.media_add_option)(media, option.as_ptr());
            }
            (self.api.player_set_media)(self.raw, media);
            (self.api.media_release)(media);
        }
        Ok(())
    }

    /// Fenêtre de rendu. `hwnd` doit rester valide tant que la lecture dure.
    pub fn set_hwnd(&self, hwnd: *mut c_void) {
        // SAFETY: libVLC mémorise le HWND ; l'appelant garantit sa validité.
        unsafe { (self.api.player_set_hwnd)(self.raw, hwnd) };
    }

    pub fn play(&self) -> Result<(), String> {
        // SAFETY: `raw` valide.
        if unsafe { (self.api.player_play)(self.raw) } == 0 {
            Ok(())
        } else {
            Err(format!("lecture impossible: {}", self.api.last_error()))
        }
    }

    pub fn set_pause(&self, pause: bool) {
        // SAFETY: `raw` valide.
        unsafe { (self.api.player_set_pause)(self.raw, pause as c_int) };
    }

    pub fn stop(&self) {
        // SAFETY: `raw` valide.
        unsafe { (self.api.player_stop)(self.raw) };
    }

    /// Position en millisecondes (`-1` si inconnue).
    pub fn time_ms(&self) -> i64 {
        // SAFETY: `raw` valide.
        unsafe { (self.api.player_get_time)(self.raw) }
    }

    pub fn set_time_ms(&self, ms: i64) {
        // SAFETY: `raw` valide.
        unsafe { (self.api.player_set_time)(self.raw, ms) };
    }

    /// Durée en millisecondes (`-1` si inconnue).
    pub fn length_ms(&self) -> i64 {
        // SAFETY: `raw` valide.
        unsafe { (self.api.player_get_length)(self.raw) }
    }

    pub fn is_playing(&self) -> bool {
        // SAFETY: `raw` valide.
        unsafe { (self.api.player_is_playing)(self.raw) != 0 }
    }

    pub fn state(&self) -> VlcState {
        // SAFETY: `raw` valide.
        VlcState::from_raw(unsafe { (self.api.player_get_state)(self.raw) })
    }

    /// Volume en % (100 = nominal, peut dépasser 100) ; `-1` si indisponible.
    pub fn volume(&self) -> i32 {
        // SAFETY: `raw` valide.
        unsafe { (self.api.audio_get_volume)(self.raw) }
    }

    pub fn set_volume(&self, volume: i32) -> bool {
        // SAFETY: `raw` valide.
        unsafe { (self.api.audio_set_volume)(self.raw, volume) == 0 }
    }

    pub fn is_muted(&self) -> bool {
        // SAFETY: `raw` valide.
        unsafe { (self.api.audio_get_mute)(self.raw) > 0 }
    }

    pub fn set_mute(&self, mute: bool) {
        // SAFETY: `raw` valide.
        unsafe { (self.api.audio_set_mute)(self.raw, mute as c_int) };
    }

    pub fn set_rate(&self, rate: f32) -> bool {
        // SAFETY: `raw` valide.
        unsafe { (self.api.player_set_rate)(self.raw, rate) == 0 }
    }

    pub fn audio_track(&self) -> i32 {
        // SAFETY: `raw` valide.
        unsafe { (self.api.audio_get_track)(self.raw) }
    }

    pub fn set_audio_track(&self, id: i32) -> bool {
        // SAFETY: `raw` valide.
        unsafe { (self.api.audio_set_track)(self.raw, id) == 0 }
    }

    pub fn subtitle_track(&self) -> i32 {
        // SAFETY: `raw` valide.
        unsafe { (self.api.video_get_spu)(self.raw) }
    }

    pub fn set_subtitle_track(&self, id: i32) -> bool {
        // SAFETY: `raw` valide.
        unsafe { (self.api.video_set_spu)(self.raw, id) == 0 }
    }

    /// Pistes audio (l'entrée « Désactivé », id `-1`, est incluse).
    pub fn audio_tracks(&self) -> Vec<VlcTrack> {
        // SAFETY: `raw` valide ; la liste est libérée par `collect_tracks`.
        self.collect_tracks(unsafe { (self.api.audio_get_track_description)(self.raw) })
    }

    /// Pistes de sous-titres (l'entrée « Désactivé », id `-1`, est incluse).
    pub fn subtitle_tracks(&self) -> Vec<VlcTrack> {
        // SAFETY: `raw` valide ; la liste est libérée par `collect_tracks`.
        self.collect_tracks(unsafe { (self.api.video_get_spu_description)(self.raw) })
    }

    fn collect_tracks(&self, head: *mut RawTrackDescription) -> Vec<VlcTrack> {
        let mut tracks = Vec::new();
        let mut node = head;
        while !node.is_null() {
            // SAFETY: `node` appartient à la liste chaînée renvoyée par libVLC, vivante jusqu'à la libération ci-dessous.
            unsafe {
                tracks.push(VlcTrack {
                    id: (*node).id,
                    name: c_string((*node).name).unwrap_or_default(),
                });
                node = (*node).next;
            }
        }
        if !head.is_null() {
            // SAFETY: `head` est la tête de liste obtenue de libVLC, libérée une seule fois.
            unsafe { (self.api.track_description_list_release)(head) };
        }
        tracks
    }

    /// Dimensions de la vidéo décodée, si une sortie vidéo existe.
    pub fn video_size(&self) -> Option<(u32, u32)> {
        let (mut width, mut height): (c_uint, c_uint) = (0, 0);
        // SAFETY: `raw` valide ; les deux pointeurs visent des variables locales.
        let status = unsafe { (self.api.video_get_size)(self.raw, 0, &mut width, &mut height) };
        (status == 0 && width > 0 && height > 0).then_some((width, height))
    }

    /// Désactive les entrées souris/clavier gérées par libVLC (la WebView garde la main).
    pub fn disable_input(&self) {
        // SAFETY: `raw` valide.
        unsafe {
            (self.api.video_set_mouse_input)(self.raw, 0);
            (self.api.video_set_key_input)(self.raw, 0);
        }
    }

    /// Abonne `callback` à l'événement `event_type`.
    ///
    /// # Safety
    /// `callback` s'exécute sur un fil libVLC ; `user` doit rester valide jusqu'à `detach`.
    pub unsafe fn attach(
        &self,
        event_type: c_int,
        callback: VlcEventCallback,
        user: *mut c_void,
    ) -> Result<(), String> {
        let manager = (self.api.player_event_manager)(self.raw);
        if manager.is_null() {
            return Err("gestionnaire d'événements indisponible".into());
        }
        if (self.api.event_attach)(manager, event_type, callback, user) == 0 {
            Ok(())
        } else {
            Err(format!(
                "libvlc_event_attach a échoué: {}",
                self.api.last_error()
            ))
        }
    }

    /// # Safety
    /// Mêmes arguments que ceux passés à `attach`.
    pub unsafe fn detach(&self, event_type: c_int, callback: VlcEventCallback, user: *mut c_void) {
        let manager = (self.api.player_event_manager)(self.raw);
        if !manager.is_null() {
            (self.api.event_detach)(manager, event_type, callback, user);
        }
    }
}

impl Drop for VlcMediaPlayer {
    fn drop(&mut self) {
        // SAFETY: `raw` n'est libéré qu'ici. `stop` d'abord : libVLC attend l'arrêt de la sortie vidéo.
        unsafe {
            (self.api.player_stop)(self.raw);
            (self.api.player_release)(self.raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_3_0_acceptee() {
        assert!(is_supported_version("3.0.21 Vetinari"));
        assert!(is_supported_version("3.0.0"));
    }

    #[test]
    fn autres_versions_refusees() {
        for version in ["4.0.0-dev", "3.1.0", "2.2.8", "30.0", "", "3", "abc"] {
            assert!(!is_supported_version(version), "{}", version);
        }
    }

    #[test]
    fn chargement_sans_dll_renvoie_une_erreur() {
        let dir = env::temp_dir().join("laststream-vlc-absent-LAST-504");
        assert!(LibVlc::load_from_dir(&dir).is_err());
    }

    #[test]
    fn dossier_embarque_est_prioritaire() {
        let dirs = candidate_dirs();
        let exe_dir = env::current_exe().unwrap().parent().unwrap().join("vlc");
        assert_eq!(dirs.first(), Some(&exe_dir));
    }

    #[test]
    fn octet_nul_refuse() {
        assert!(to_cstring("a\0b", "x").is_err());
    }
}
