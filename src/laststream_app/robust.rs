//! Robustesse du shell (LAST-132) : erreurs journalisées au lieu de paniques, fils de service suivis et arrêtés
//! proprement, budget de relance (mpv).
//!
//! Journal : stderr (visible en débogage) et `%APPDATA%\Laststream\shell-errors.log` (borné), car en version finale
//! le shell est une application fenêtrée sans console.
//!
//! Sentry (LAST-172) : `report_error` envoie aussi chaque erreur (`sentry::capture_message`, sans effet si Sentry n'est
//! pas initialisé). Les paniques partent par l'intégration panique de Sentry, chaînée au hook de `install_panic_hook`.
//! Tout texte est passé par `scrub` avant écriture ou envoi : bornage à 200 caractères, requêtes d'URL et jetons masqués.

use std::{
    any::Any,
    collections::VecDeque,
    fmt::Display,
    fs::{self, OpenOptions},
    io::Write,
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, MutexGuard, Once,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const LOG_FILE: &str = "shell-errors.log";
/// Au-delà, le journal repart de zéro.
const LOG_MAX_BYTES: u64 = 256 * 1024;
/// Pas d'attente des boucles qui surveillent l'arrêt.
const POLL: Duration = Duration::from_millis(200);
/// Longueur maximale d'un texte journalisé ou envoyé, hors marque de troncature.
pub const MAX_REPORT_CHARS: usize = 200;
/// Remplacement des valeurs sensibles.
const MASK: &str = "[masqué]";
/// Paramètres dont la valeur est masquée partout (jetons de débrideur, mots de passe), sans distinction de casse.
const SECRET_KEYS: [&str; 6] = [
    "access_token",
    "apikey",
    "api_key",
    "password",
    "secret",
    "token",
];

#[cfg(not(test))]
fn log_path() -> Option<PathBuf> {
    Some(
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("Laststream")
            .join(LOG_FILE),
    )
}

/// En test, le journal va au fichier choisi par le test, jamais à `%APPDATA%`.
#[cfg(test)]
static TEST_LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

#[cfg(test)]
fn log_path() -> Option<PathBuf> {
    TEST_LOG_PATH.lock().ok()?.clone()
}

fn append_to_log(line: &str) {
    let Some(path) = log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::metadata(&path).is_ok_and(|meta| meta.len() > LOG_MAX_BYTES) {
        let _ = fs::remove_file(&path);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
}

/// Écrit une ligne déjà masquée dans stderr et le journal, sans envoi.
fn journal(line: &str) {
    eprintln!("[laststream] {line}");
    append_to_log(line);
}

/// Journalise une erreur de production (contexte court, sans donnée de l'utilisateur) et l'envoie à Sentry si actif.
pub fn report_error(context: &str, error: &dyn Display) {
    let line = scrub(&format!("{context}: {error}"));
    journal(&line);
    sentry::capture_message(&line, sentry::Level::Error);
}

/// Installe le hook de panique (une seule fois) : la panique est journalisée avec son lieu et son message masqué,
/// puis passe au hook précédent, qui inclut l'intégration panique de Sentry si elle est active.
pub fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|location| format!("{}:{}", location.file(), location.line()))
                .unwrap_or_else(|| "?".to_owned());
            let thread = thread::current();
            let message = panic_message(info.payload());
            journal(&scrub(&format!(
                "panic: fil {} à {location}: {message}",
                thread.name().unwrap_or("sans nom")
            )));
            previous(info);
        }));
    });
}

/// Prépare un texte d'erreur ou de panique pour le journal et Sentry : jetons et requêtes d'URL masqués, borné.
pub fn scrub(text: &str) -> String {
    truncate_chars(&mask_secrets(text), MAX_REPORT_CHARS)
}

/// Masque les jetons de chaque mot (espaces conservés) : requête d'URL, `clé=valeur` sensible, valeur après `Bearer`.
fn mask_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut mask_next = false;
    for piece in text.split_inclusive(char::is_whitespace) {
        let word = piece.trim_end_matches(char::is_whitespace);
        let spacing = &piece[word.len()..];
        if word.is_empty() {
            out.push_str(spacing);
            continue;
        }
        if mask_next {
            out.push_str(MASK);
            mask_next = false;
        } else {
            mask_next = word
                .trim_end_matches([':', ','])
                .eq_ignore_ascii_case("bearer");
            out.push_str(&mask_key_values(&mask_query(word)));
        }
        out.push_str(spacing);
    }
    out
}

/// `https://hôte/chemin?agent=…&apikey=…` devient `https://hôte/chemin?[masqué]` ; le délimiteur qui suit reste.
fn mask_query(word: &str) -> String {
    let Some(scheme) = word.find("://") else {
        return word.to_owned();
    };
    let Some(question) = word[scheme..].find('?').map(|at| scheme + at) else {
        return word.to_owned();
    };
    let tail = &word[question + 1..];
    let end = tail
        .find(['"', '\'', '<', '>', ')', ']', '}', ','])
        .unwrap_or(tail.len());
    format!("{}?{MASK}{}", &word[..question], &tail[end..])
}

/// Remplace la valeur de chaque `clé=` sensible (jusqu'à `&` ou `;`) par `[masqué]`.
fn mask_key_values(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
    let mut out = String::with_capacity(word.len());
    let mut at = 0;
    while at < word.len() {
        let key = SECRET_KEYS.iter().find(|key| {
            lower[at..].starts_with(**key)
                && lower[at + key.len()..].starts_with('=')
                && (at == 0 || !is_identifier_byte(lower.as_bytes()[at - 1]))
        });
        match key {
            Some(key) => {
                let value_start = at + key.len() + 1;
                let value_end = word[value_start..]
                    .find(['&', ';'])
                    .map_or(word.len(), |offset| value_start + offset);
                out.push_str(&word[at..value_start]);
                out.push_str(MASK);
                at = value_end;
            }
            None => {
                let Some(ch) = word[at..].chars().next() else {
                    break;
                };
                out.push(ch);
                at += ch.len_utf8();
            }
        }
    }
    out
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Tronque à `max` caractères (et non octets), avec une marque `…`.
fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((cut, _)) => format!("{}…", &text[..cut]),
    }
}

/// Verrouille un mutex même empoisonné par une panique d'un autre fil : la valeur est reprise et l'incident journalisé.
pub fn lock<'a, T>(mutex: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        report_error(what, &"verrou empoisonné, valeur reprise");
        poisoned.into_inner()
    })
}

/// `result.or_report("contexte")?` : journalise l'erreur puis la laisse remonter.
pub trait OrReport<T, E> {
    fn or_report(self, context: &str) -> Result<T, E>;
}

impl<T, E: Display> OrReport<T, E> for Result<T, E> {
    fn or_report(self, context: &str) -> Result<T, E> {
        if let Err(error) = &self {
            report_error(context, error);
        }
        self
    }
}

/// Texte d'une charge de panique.
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panique sans message".to_owned())
}

/// Attend la fin d'un fil au plus `timeout`. `true` si le fil s'est terminé (et a été joint).
pub fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = handle.join();
    true
}

/// Fils de service d'une instance : drapeau d'arrêt partagé et liste des fils à joindre à la fermeture.
pub struct Services {
    shutdown: AtomicBool,
    threads: Mutex<Vec<(String, JoinHandle<()>)>>,
}

impl Services {
    pub const fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            threads: Mutex::new(Vec::new()),
        }
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Lance un fil suivi : une panique est attrapée et journalisée, le fil est joint par `shutdown`.
    pub fn spawn<F: FnOnce() + Send + 'static>(&self, name: &str, body: F) {
        let thread_name = name.to_owned();
        let spawned = thread::Builder::new().name(name.to_owned()).spawn(move || {
            if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(body)) {
                report_error(
                    &format!("fil {thread_name} arrêté par une panique"),
                    &panic_message(payload.as_ref()),
                );
            }
        });
        match spawned {
            Ok(handle) => {
                let mut threads = lock(&self.threads, "registre des fils");
                threads.retain(|(_, handle)| !handle.is_finished());
                threads.push((name.to_owned(), handle));
            }
            Err(error) => report_error(&format!("fil {name} non démarré"), &error),
        }
    }

    /// Attend un message en surveillant l'arrêt. `None` : canal fermé ou arrêt demandé.
    pub fn recv<T>(&self, receiver: &flume::Receiver<T>) -> Option<T> {
        loop {
            if self.is_shutdown() {
                return None;
            }
            match receiver.recv_timeout(POLL) {
                Ok(message) => return Some(message),
                Err(flume::RecvTimeoutError::Timeout) => {}
                Err(flume::RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// Dort `duration` par tranches ; `false` si l'arrêt a été demandé entre-temps.
    pub fn sleep(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        while !self.is_shutdown() {
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            thread::sleep((deadline - now).min(POLL));
        }
        false
    }

    /// Demande l'arrêt : les boucles de `recv` et `sleep` rendent la main.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Joint les fils pendant `timeout` au total. Renvoie le nombre de fils encore actifs.
    pub fn join_all(&self, timeout: Duration) -> usize {
        let threads = std::mem::take(&mut *lock(&self.threads, "registre des fils"));
        let deadline = Instant::now() + timeout;
        let mut stragglers = 0;
        for (name, handle) in threads {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !join_with_timeout(handle, remaining) {
                report_error(&format!("fil {name}"), &"toujours actif à la fermeture");
                stragglers += 1;
            }
        }
        stragglers
    }
}

/// Fils de service du shell.
pub static SERVICES: Services = Services::new();

/// Budget de relance : au plus `max_attempts` relances sur une fenêtre glissante, avec attente exponentielle.
pub struct RestartPolicy {
    max_attempts: usize,
    window: Duration,
    base_delay: Duration,
    failures: VecDeque<Instant>,
}

impl RestartPolicy {
    pub fn new(max_attempts: usize, window: Duration, base_delay: Duration) -> Self {
        Self {
            max_attempts,
            window,
            base_delay,
            failures: VecDeque::new(),
        }
    }

    /// Enregistre un arrêt inattendu à `now`. `Some(attente)` avant de relancer, `None` si le budget est épuisé.
    pub fn on_failure(&mut self, now: Instant) -> Option<Duration> {
        while self
            .failures
            .front()
            .is_some_and(|first| now.saturating_duration_since(*first) > self.window)
        {
            self.failures.pop_front();
        }
        if self.failures.len() >= self.max_attempts {
            return None;
        }
        let exponent = self.failures.len().min(5) as u32;
        self.failures.push_back(now);
        Some(self.base_delay * 2u32.pow(exponent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn restart_budget_allows_three_attempts_then_stops() {
        let mut policy = RestartPolicy::new(3, Duration::from_secs(60), Duration::from_millis(250));
        let start = Instant::now();
        assert_eq!(policy.on_failure(start), Some(Duration::from_millis(250)));
        assert_eq!(
            policy.on_failure(start + Duration::from_secs(1)),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            policy.on_failure(start + Duration::from_secs(2)),
            Some(Duration::from_millis(1000))
        );
        assert_eq!(policy.on_failure(start + Duration::from_secs(3)), None);
    }

    #[test]
    fn restart_budget_recovers_once_the_window_has_passed() {
        let mut policy = RestartPolicy::new(2, Duration::from_secs(60), Duration::from_millis(100));
        let start = Instant::now();
        assert!(policy.on_failure(start).is_some());
        assert!(policy.on_failure(start + Duration::from_secs(1)).is_some());
        assert!(policy.on_failure(start + Duration::from_secs(2)).is_none());
        // La première panne sort de la fenêtre : une relance redevient possible, avec l'attente initiale.
        assert_eq!(
            policy.on_failure(start + Duration::from_secs(61)),
            Some(Duration::from_millis(200))
        );
    }

    #[test]
    fn poisoned_mutex_is_recovered_instead_of_panicking() {
        let mutex = Arc::new(Mutex::new(41));
        let poisoner = Arc::clone(&mutex);
        let _ = thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("empoisonne le verrou");
        })
        .join();
        assert!(mutex.is_poisoned());
        *lock(&mutex, "test") += 1;
        assert_eq!(*lock(&mutex, "test"), 42);
    }

    #[test]
    fn spawned_service_panic_is_caught_and_shutdown_joins_everything() {
        let services = Services::new();
        services.spawn("panique", || panic!("boum"));
        let (tx, rx) = flume::unbounded::<u8>();
        let services = Arc::new(services);
        let waiter = Arc::clone(&services);
        services.spawn("attente", move || {
            // Sort quand l'arrêt est demandé, sans qu'aucun message n'arrive.
            assert_eq!(waiter.recv(&rx), None);
        });
        services.request_shutdown();
        assert_eq!(services.join_all(Duration::from_secs(5)), 0);
        drop(tx);
    }

    #[test]
    fn shutdown_reports_a_thread_that_does_not_stop() {
        let services = Services::new();
        let (release, blocked) = flume::bounded::<()>(1);
        services.spawn("bloque", move || {
            let _ = blocked.recv();
        });
        services.request_shutdown();
        assert_eq!(services.join_all(Duration::from_millis(100)), 1);
        release.send(()).unwrap();
    }

    #[test]
    fn sleep_returns_early_on_shutdown() {
        let services = Arc::new(Services::new());
        let sleeper = Arc::clone(&services);
        let handle = thread::spawn(move || sleeper.sleep(Duration::from_secs(30)));
        thread::sleep(Duration::from_millis(50));
        services.request_shutdown();
        assert!(!handle.join().unwrap());
    }

    #[test]
    fn panic_message_reads_str_and_string_payloads() {
        let from_str = panic::catch_unwind(|| panic!("texte")).unwrap_err();
        assert_eq!(panic_message(from_str.as_ref()), "texte");
        let from_string =
            panic::catch_unwind(|| panic!("{}", String::from("formaté"))).unwrap_err();
        assert_eq!(panic_message(from_string.as_ref()), "formaté");
    }

    #[test]
    fn scrub_masks_the_query_of_an_alldebrid_url() {
        assert_eq!(
            scrub("https://api.alldebrid.com/v4/magnet/upload?agent=laststream&apikey=SECRET123"),
            "https://api.alldebrid.com/v4/magnet/upload?[masqué]"
        );
    }

    #[test]
    fn scrub_keeps_the_text_around_a_masked_url() {
        assert_eq!(
            scrub("Échec (https://api.alldebrid.com/v4/user?apikey=SECRET123), nouvel essai"),
            "Échec (https://api.alldebrid.com/v4/user?[masqué]), nouvel essai"
        );
    }

    #[test]
    fn scrub_masks_token_keys_and_bearer_values_outside_urls() {
        assert_eq!(
            scrub("reçu token=SECRET123&x=1 et Authorization: Bearer SECRET456 refusé"),
            "reçu token=[masqué]&x=1 et Authorization: Bearer [masqué] refusé"
        );
    }

    #[test]
    fn scrub_truncates_long_text_to_about_200_characters() {
        let scrubbed = scrub(&"é".repeat(500));
        assert_eq!(scrubbed.chars().count(), MAX_REPORT_CHARS + 1);
        assert!(scrubbed.ends_with('…'));
    }

    #[test]
    fn scrub_leaves_plain_text_without_url_or_secret_unchanged() {
        assert_eq!(
            scrub("fil lecteur à src/x.rs:12 : pourquoi ?"),
            "fil lecteur à src/x.rs:12 : pourquoi ?"
        );
    }

    #[test]
    fn early_panic_hook_journals_the_panic_with_masked_text() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("panic-hook-{}.log", std::process::id()));
        let _ = fs::remove_file(&file);
        *TEST_LOG_PATH.lock().unwrap() = Some(file.clone());
        install_panic_hook();
        let _ = panic::catch_unwind(|| {
            panic!("échec https://api.alldebrid.com/v4/user?apikey=SECRET789")
        });
        let journal = fs::read_to_string(&file).unwrap();
        assert!(journal.contains("panic"), "{journal}");
        assert!(journal.contains("?[masqué]"), "{journal}");
        assert!(!journal.contains("SECRET789"), "{journal}");
    }
}
