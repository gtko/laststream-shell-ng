//! Sonde libVLC (LAST-504) : `LASTSTREAM_LIBVLC_DIR=<dossier de libvlc.dll> cargo run --example vlc_probe`
//!
//! Charge libVLC dynamiquement, affiche sa version, ouvre une fenêtre, lit ~9 s au plus d'une URL HTTP
//! (`VLC_PROBE_URL`, sinon une vidéo de test publique ; `VLC_PROBE_ARGS` ajoute des options libVLC).
//! Remarque : le clip public par défaut, sans piste audio, se fige à ~250 ms avec libVLC 3.0.21 ;
//! préférer une URL avec audio (vérifié : HTTP local avec Range, `+faststart`) dans une surface enfant, puis s'arrête.
//! Contrôle aussi l'ordre Z : une fenêtre blanche créée AVANT la surface doit rester visible au-dessus
//! (pixel lu à l'écran). Sans `LASTSTREAM_LIBVLC_DIR`, la recherche par défaut est utilisée.
#![cfg(windows)]
#![allow(dead_code)] // Modules partagés avec l'app : seule une partie sert à la sonde.

#[path = "../src/laststream_app/laststream_player/vlc_backend.rs"]
mod vlc_backend;
#[path = "../src/laststream_app/laststream_player/vlc_ffi.rs"]
mod vlc_ffi;
#[path = "../src/laststream_app/laststream_player/vlc_player.rs"]
mod vlc_player;

use std::{
    env,
    os::windows::ffi::OsStrExt,
    path::PathBuf,
    process::ExitCode,
    ptr,
    time::{Duration, Instant},
};
use vlc_backend::VlcBackend;
use vlc_ffi::{LibVlc, VlcInstance};
use vlc_player::VlcPlayer;
use winapi::{
    shared::windef::HWND,
    um::{
        wingdi::GetPixel,
        winuser::{
            ClientToScreen, CreateWindowExW, DestroyWindow, DispatchMessageW, GetDC, GetWindow,
            PeekMessageW, ReleaseDC, TranslateMessage, GW_HWNDNEXT, MSG, PM_REMOVE, WS_CHILD,
            WS_OVERLAPPEDWINDOW, WS_VISIBLE,
        },
    },
};

const DEFAULT_URL: &str =
    "https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/360/Big_Buck_Bunny_360_10s_1MB.mp4";
const SS_WHITERECT: u32 = 0x0000_0006;

fn wide(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn child(parent: HWND, style: u32, x: i32, y: i32, w: i32, h: i32) -> HWND {
    let class = wide("STATIC");
    let title = wide("");
    // SAFETY: fenêtre STATIC enfant de `parent`, chaînes vivantes pendant l'appel.
    unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            x,
            y,
            w,
            h,
            parent,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    }
}

fn pump_for(duration: Duration, mut tick: impl FnMut()) {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        // SAFETY: boucle de messages standard sur le fil qui possède les fenêtres.
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        tick();
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn screen_pixel(window: HWND, x: i32, y: i32) -> u32 {
    let mut point = winapi::shared::windef::POINT { x, y };
    // SAFETY: lecture d'un pixel de l'écran ; le DC est libéré aussitôt.
    unsafe {
        ClientToScreen(window, &mut point);
        let dc = GetDC(ptr::null_mut());
        let pixel = GetPixel(dc, point.x, point.y);
        ReleaseDC(ptr::null_mut(), dc);
        pixel
    }
}

fn run() -> Result<(), String> {
    let api = match env::var_os("LASTSTREAM_LIBVLC_DIR") {
        Some(dir) => LibVlc::load_from_dir(&PathBuf::from(dir))?,
        None => LibVlc::load()?,
    };
    println!("libVLC chargée, version {}", api.version());
    let extra = env::var("VLC_PROBE_ARGS").unwrap_or_default();
    let mut args: Vec<&str> = VlcInstance::DEFAULT_ARGS.to_vec();
    args.extend(extra.split_whitespace());
    let instance = VlcInstance::new(api, &args)?;
    let player = instance.new_player()?;

    let class = wide("STATIC");
    let title = wide("vlc_probe");
    // SAFETY: fenêtre de premier niveau créée et détruite sur ce fil.
    let top = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            200,
            200,
            656,
            399,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if top.is_null() {
        return Err("fenêtre de test non créée".into());
    }
    // Créé AVANT la surface, comme la WebView : doit finir au-dessus.
    let overlay = child(top, SS_WHITERECT, 10, 10, 80, 40);
    let backend = VlcBackend::new(VlcPlayer::new(player, top)?)?;
    let vlc = backend.player();
    // SAFETY: GetWindow lit l'ordre Z de fenêtres vivantes.
    let below_surface = unsafe { GetWindow(vlc.surface.hwnd(), GW_HWNDNEXT) };
    println!(
        "ordre Z : surface en bas = {} ; recouvrement creé avant = au-dessus",
        below_surface.is_null()
    );

    let url = env::var("VLC_PROBE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    backend.load(&url, &[":network-caching=1000"])?;

    let mut max_time = -1;
    let mut size = None;
    let mut sampled = None;
    pump_for(Duration::from_secs(9), || {
        max_time = max_time.max(vlc.player.time_ms());
        if size.is_none() {
            size = vlc.player.video_size();
        }
        if max_time > 1500 && sampled.is_none() {
            sampled = Some((screen_pixel(top, 50, 30), screen_pixel(top, 400, 250)));
        }
    });
    println!(
        "etat={:?} temps_max={max_time} ms duree={} ms taille={:?} volume={}",
        vlc.player.state(),
        vlc.player.length_ms(),
        size,
        vlc.player.volume()
    );
    println!("pistes audio : {:?}", vlc.player.audio_tracks());
    println!("pistes sous-titres : {:?}", vlc.player.subtitle_tracks());
    for name in [
        "time-pos",
        "duration",
        "pause",
        "volume",
        "mute",
        "speed",
        "eof-reached",
        "seeking",
        "paused-for-cache",
        "track-list",
        "aid",
        "sid",
        "video-params",
    ] {
        println!("  {name} = {:?}", backend.property(name));
    }
    backend.set_property("pause", &serde_json::json!(true));
    println!("  apres pause : pause = {:?}", backend.property("pause"));
    let result = match sampled {
        Some((over, video)) => {
            println!("pixel recouvrement = {over:#08x} (blanc attendu 0xffffff) ; pixel vidéo = {video:#08x}");
            if over == 0x00ff_ffff && max_time > 1500 && below_surface.is_null() {
                Ok(())
            } else {
                Err("le recouvrement n'est pas au-dessus de la vidéo".to_string())
            }
        }
        None => Err(format!("la lecture n'a pas avancé (temps_max={max_time})")),
    };
    backend.stop();
    drop(backend);
    // SAFETY: fenêtres de ce fil.
    unsafe {
        DestroyWindow(overlay);
        DestroyWindow(top);
    }
    result
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("OK");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("ECHEC : {error}");
            ExitCode::FAILURE
        }
    }
}
