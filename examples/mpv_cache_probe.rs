//! Sonde du tampon mpv (LAST-146) : libmpv sans fenêtre (vo=null, ao=null) qui lit une URL pendant N secondes
//! avec les options de tampon passées en arguments. Mesurée par scripts/measure-mpv-memory.ps1.
//! Usage : cargo build --example mpv_cache_probe -- puis
//!         mpv_cache_probe <url> <secondes> [nom=valeur ...]
use libmpv2::{events::Event, Mpv};
use std::time::{Duration, Instant};

fn main() {
    let usage = "usage: mpv_cache_probe <url> <secondes> [nom=valeur ...]";
    let mut args = std::env::args().skip(1);
    let url = args.next().expect(usage);
    let seconds: u64 = args
        .next()
        .expect(usage)
        .parse()
        .expect("secondes : entier");
    let options: Vec<(String, String)> = args
        .map(|pair| {
            let (name, value) = pair.split_once('=').expect("option attendue : nom=valeur");
            (name.to_string(), value.to_string())
        })
        .collect();

    let mpv = Mpv::with_initializer(|init| {
        init.set_property("vo", "null")?;
        init.set_property("ao", "null")?;
        init.set_property("force-window", "no")?;
        init.set_property("quiet", "yes")?;
        init.set_property("msg-level", "all=no")?;
        for (name, value) in &options {
            init.set_property(name, value.as_str())?;
        }
        Ok(())
    })
    .expect("cannot build mpv");
    println!(
        "mpv-version {}",
        mpv.get_property::<String>("mpv-version")
            .unwrap_or_default()
    );
    for (name, value) in &options {
        println!("option {name}={value}");
    }
    mpv.command("loadfile", &[url.as_str()])
        .expect("loadfile failed");

    let start = Instant::now();
    let end = start + Duration::from_secs(seconds);
    let mut next_report = start;
    while Instant::now() < end {
        if let Some(Ok(Event::EndFile(reason))) = mpv.wait_event(0.5) {
            println!("end-file {reason:?}");
            break;
        }
        if Instant::now() >= next_report {
            next_report += Duration::from_secs(10);
            let used = mpv
                .get_property::<i64>("cache-used")
                .map(|kib| format!("{kib} KiB"))
                .unwrap_or_else(|_| "n/a".to_string());
            let pos = mpv.get_property::<f64>("time-pos").unwrap_or(-1.0);
            println!(
                "t={}s time-pos={pos:.1}s cache-used={used}",
                start.elapsed().as_secs()
            );
        }
    }
    println!("done {}s", start.elapsed().as_secs());
}
