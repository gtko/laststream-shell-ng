//! Surface de rendu libVLC : fenêtre enfant du HWND principal, sous la WebView (LAST-504).
//!
//! mpv reçoit `wid` = HWND principal et y crée sa propre fenêtre enfant ; la WebView2 (fond
//! transparent, créée ensuite) se superpose au-dessus. Ici la fenêtre enfant est créée par nous,
//! puis envoyée tout en bas de l'ordre Z des enfants (`HWND_BOTTOM`) : la WebView reste donc
//! visible et cliquable par-dessus quel que soit l'ordre de création.
//!
//! La surface doit être créée, redimensionnée et détruite sur le fil qui possède le HWND parent
//! (le fil de l'interface, qui pompe les messages).
//!
//! Ce module n'utilise que `std`, `winapi` et `super::vlc_ffi` : il est inclus tel quel par
//! `examples/vlc_probe.rs`.
#![allow(dead_code)] // Branché par le lot suivant (IPC player-set-backend).

use super::vlc_ffi::VlcMediaPlayer;
use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
use winapi::{
    shared::windef::{HWND, RECT},
    um::winuser::{
        CreateWindowExW, DestroyWindow, GetClientRect, SetWindowPos, ShowWindow, HWND_BOTTOM,
        SWP_NOACTIVATE, SW_HIDE, SW_SHOWNA, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE,
    },
};

/// Style `SS_BLACKRECT` de la classe prédéfinie `STATIC` : fond noir, sans procédure de fenêtre à écrire.
const SS_BLACKRECT: u32 = 0x0000_0004;

fn wide(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Fenêtre enfant qui reçoit la vidéo de libVLC.
pub struct VideoSurface {
    hwnd: HWND,
    parent: HWND,
}

impl VideoSurface {
    /// Crée la surface (visible) sur tout le rectangle client de `parent`, sous ses autres enfants.
    pub fn create(parent: HWND) -> Result<Self, String> {
        if parent.is_null() {
            return Err("surface vidéo: HWND parent nul".into());
        }
        let class = wide("STATIC");
        let title = wide("Laststream VLC");
        // SAFETY: `parent` est un HWND fourni par l'appelant ; les chaînes UTF-16 terminées par NUL vivent pendant l'appel.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN | SS_BLACKRECT,
                0,
                0,
                1,
                1,
                parent,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            return Err("surface vidéo: CreateWindowExW a échoué".into());
        }
        let surface = Self { hwnd, parent };
        surface.fit_to_parent();
        Ok(surface)
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Recale la surface sur le rectangle client du parent et la renvoie tout en bas de l'ordre Z.
    /// À appeler à chaque redimensionnement de la fenêtre principale.
    pub fn fit_to_parent(&self) {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: `parent` et `hwnd` sont des fenêtres de ce fil ; `rect` est une variable locale.
        unsafe {
            if GetClientRect(self.parent, &mut rect) == 0 {
                return;
            }
            SetWindowPos(
                self.hwnd,
                HWND_BOTTOM,
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOACTIVATE,
            );
        }
    }

    /// Affiche ou cache la surface (cachée tant que VLC n'est pas le moteur actif).
    pub fn set_visible(&self, visible: bool) {
        // SAFETY: `hwnd` est une fenêtre vivante de ce fil.
        unsafe { ShowWindow(self.hwnd, if visible { SW_SHOWNA } else { SW_HIDE }) };
        if visible {
            self.fit_to_parent();
        }
    }
}

impl Drop for VideoSurface {
    fn drop(&mut self) {
        // SAFETY: DestroyWindow n'agit que depuis le fil propriétaire ; sinon il échoue sans effet de bord.
        unsafe { DestroyWindow(self.hwnd) };
    }
}

/// Lecteur libVLC lié à sa surface. Le lecteur est libéré avant la fenêtre (ordre des champs).
pub struct VlcPlayer {
    pub player: VlcMediaPlayer,
    pub surface: VideoSurface,
}

impl VlcPlayer {
    /// Crée la surface sous la WebView, désactive les entrées libVLC et branche `set_hwnd`.
    pub fn new(player: VlcMediaPlayer, parent: HWND) -> Result<Self, String> {
        let surface = VideoSurface::create(parent)?;
        player.disable_input();
        player.set_hwnd(surface.hwnd() as *mut c_void);
        Ok(Self { player, surface })
    }
}
