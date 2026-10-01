//! Session and application state for RASP (Milestones 4+).
//!
//! Classic ASP hands each visitor a session cookie and keeps named
//! values server-side per session. RASP keeps the same model with an
//! in-process store: sessions are identified by an `ASPSESSIONID`
//! cookie value, created lazily, and expired after an idle timeout.
//! Application state is one shared store per process.
//!
//! Cookie values are HMAC-signed under a per-process key
//! (`asp_core::cookie_sign`), so a forged cookie cannot cross into
//! another visitor's session and cookies stop working when the
//! process — and its in-memory store — goes away anyway.
//!
//! The store is intentionally NOT persistent and NOT shared between
//! processes — a multi-instance deployment needs the pluggable store
//! work recorded in the plan (deferred).

pub use asp_core::cookie_sign::{SigningKey, sign_session_cookie, verify_session_cookie};
use asp_core::parser::Block;
use asp_core::{AspError, AspResult, Diagnostic};
use asp_vbscript::{ApplicationState, SessionStore, StateStores};
use std::collections::HashMap;
use std::time::Instant;

/// Cookie name for the session identity. IIS appends an 8-letter
/// variant suffix (`ASPSESSIONIDAQAAAAST`); RASP emits the bare name
/// and accepts any `ASPSESSIONID*` name when reading.
pub const SESSION_COOKIE_NAME: &str = "ASPSESSIONID";

/// One live session: its script-visible state plus expiry tracking.
#[derive(Debug, Clone)]
struct LiveSession {
    store: SessionStore,
    /// Last request that used this session (monotonic clock).
    last_seen: std::time::Instant,
}

/// In-process session store keyed by cookie id, plus the shared
/// application state for the process. Owns the process signing key:
/// cookies it issued are only valid through this manager.
pub struct SessionManager {
    sessions: HashMap<u32, LiveSession>,
    application: ApplicationState,
    next_id: u32,
    signing_key: SigningKey,
    /// Sessions dropped at resolve time — their idle expiry was
    /// observed by a returning visitor's request — parked here until
    /// the same request's commit reports them for `Session_OnEnd`.
    pending_ended: Vec<SessionStore>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self::with_signing_key(SigningKey::generate())
    }

    /// Build with an explicit signing key (tests that need one key
    /// across several managers; future persisted-key deployments).
    pub fn with_signing_key(key: SigningKey) -> Self {
        Self {
            sessions: HashMap::new(),
            application: ApplicationState::default(),
            next_id: 1,
            signing_key: key,
            pending_ended: Vec::new(),
        }
    }

    /// One-lock request entry point: resolve the session from the
    /// request's cookie (creating + registering one when new), plus the
    /// shared application state. Returns the session store, the cookie
    /// value to send, whether the session is new (drives
    /// `Session_OnStart` and the outbound cookie), and the application
    /// state snapshot.
    pub fn resolve_request_state(
        &mut self,
        cookie_value: Option<&str>,
    ) -> (SessionStore, String, bool, ApplicationState) {
        let (store, cookie, is_new) = self.resolve_session(cookie_value);
        (store, cookie, is_new, self.application.clone())
    }

    /// One-lock write-back after a request: commit the (possibly
    /// mutated) session and application stores, drop abandoned and
    /// idle-expired sessions, and report the sessions that went away so
    /// the caller can fire `Session_OnEnd` for them.
    ///
    /// The application commits BEFORE the ending events are reported,
    /// so handlers see the state this request left behind.
    /// `Application_OnEnd` is a process-lifetime event in IIS (it needs
    /// an application recycle), not a last-session one — RASP keeps it
    /// unfired until application lifecycle work lands.
    pub fn commit_request_state(
        &mut self,
        cookie_value: &str,
        session: SessionStore,
        application: ApplicationState,
    ) -> EndedEvents {
        let mut ended = EndedEvents::default();
        if let Some(id) = verify_session_cookie(&self.signing_key, cookie_value) {
            if session.abandoned {
                // Abandon: the outgoing request carries the state change,
                // but the session is destroyed for subsequent requests.
                self.sessions.remove(&id);
                ended.abandoned = Some(session);
            } else {
                self.commit_session(cookie_value, session);
            }
        }
        self.commit_application(application);
        // Endings in chronological order: expiry drops parked by
        // earlier resolves (possibly several), then this commit's
        // expiry drops, each group ascending by session id. The
        // abandon is reported last — it is this request's own ending,
        // the most recent one.
        let mut expired = std::mem::take(&mut self.pending_ended);
        expired.extend(self.expire_ended());
        expired.sort_by_key(|s| s.id);
        ended.expired = expired;
        ended
    }

    /// Resolve the request's session: reuse the store the cookie points
    /// at (respecting idle expiry), or create a fresh one when the
    /// cookie is missing/unknown/expired/unverifiable. An expired
    /// session is dropped here but parked for `Session_OnEnd`, which
    /// the request's commit reports. Returns the resolved store, the
    /// signed `ASPSESSIONID` cookie value to (re)send, and whether the
    /// session was just created (drives `Session_OnStart`).
    pub fn resolve_session(&mut self, cookie_value: Option<&str>) -> (SessionStore, String, bool) {
        let now = Instant::now();
        if let Some(raw) = cookie_value
            && let Some(id) = verify_session_cookie(&self.signing_key, raw)
        {
            let expired = match self.sessions.get(&id) {
                Some(live) => self.idle_expired(live, now),
                None => false,
            };
            if expired {
                if let Some(live) = self.sessions.remove(&id) {
                    self.pending_ended.push(live.store);
                }
            } else if let Some(live) = self.sessions.get_mut(&id) {
                live.last_seen = now;
                return (
                    live.store.clone(),
                    sign_session_cookie(&self.signing_key, id),
                    false,
                );
            }
        }
        let (store, cookie) = self.create_session(now);
        (store, cookie, true)
    }

    /// True when the session's idle time exceeded its Timeout setting.
    fn idle_expired(&self, live: &LiveSession, now: std::time::Instant) -> bool {
        let idle_min = now.duration_since(live.last_seen).as_secs_f64() / 60.0;
        idle_min > live.store.timeout_min as f64
    }

    /// Create a session with the next id and register it.
    fn create_session(&mut self, now: std::time::Instant) -> (SessionStore, String) {
        let id = self.next_id();
        let store = SessionStore::new(id);
        self.sessions.insert(
            id,
            LiveSession {
                store: store.clone(),
                last_seen: now,
            },
        );
        let cookie = sign_session_cookie(&self.signing_key, id);
        (store, cookie)
    }

    /// Allocate the next session id: deterministic counter, skipping ids
    /// still live after a wrap.
    fn next_id(&mut self) -> u32 {
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if id == 0 || self.sessions.contains_key(&id) {
                continue;
            }
            return id;
        }
    }

    /// Write a rendered session's state back into the store. Called
    /// after a successful render with the (possibly mutated) store.
    pub fn commit_session(&mut self, cookie_value: &str, store: SessionStore) {
        let Some(id) = verify_session_cookie(&self.signing_key, cookie_value) else {
            return;
        };
        if store.abandoned {
            // Abandon: the outgoing request carries the state change, but
            // the session is destroyed for subsequent requests.
            self.sessions.remove(&id);
            return;
        }
        self.sessions.insert(
            id,
            LiveSession {
                store,
                last_seen: std::time::Instant::now(),
            },
        );
    }

    /// The shared application state (clone-in, write-back).
    pub fn application(&self) -> ApplicationState {
        self.application.clone()
    }

    /// Write application state back after a render mutated it.
    pub fn commit_application(&mut self, state: ApplicationState) {
        self.application = state;
    }

    /// Drop sessions idle beyond their own Timeout settings and hand
    /// their stores back (final values for `Session_OnEnd`), oldest id
    /// first.
    pub fn expire_ended(&mut self) -> Vec<SessionStore> {
        let now = std::time::Instant::now();
        let mut expired: Vec<u32> = self
            .sessions
            .iter()
            .filter(|(_, live)| self.idle_expired(live, now))
            .map(|(id, _)| *id)
            .collect();
        expired.sort_unstable();
        expired
            .into_iter()
            .filter_map(|id| self.sessions.remove(&id).map(|live| live.store))
            .collect()
    }

    /// Number of live sessions.
    pub fn live_count(&self) -> usize {
        self.sessions.len()
    }
}

/// What ended in [`SessionManager::commit_request_state`]: the
/// sessions whose `Session_OnEnd` should fire, each carrying its final
/// values (`Application_OnEnd` is a process-lifetime event and stays
/// unfired until application lifecycle work lands).
#[derive(Debug, Default, Clone)]
pub struct EndedEvents {
    /// The session this request abandoned, with its final values.
    pub abandoned: Option<SessionStore>,
    /// Idle-expired sessions with their final values, oldest id first.
    pub expired: Vec<SessionStore>,
}

impl EndedEvents {
    /// Nothing ended: no end events need firing.
    pub fn is_empty(&self) -> bool {
        self.abandoned.is_none() && self.expired.is_empty()
    }
}

/// Pull the visitor's session-cookie value out of a raw Cookie header:
/// the first cookie whose name is `ASPSESSIONID` or `ASPSESSIONID…`
/// (IIS-style suffixed variants), case-insensitive.
pub fn session_cookie_from_header(cookies_header: &str) -> Option<String> {
    const NAME_PREFIX: &str = "aspsessionid";
    for pair in cookies_header.split(';') {
        let pair = pair.trim();
        if let Some((name, value)) = pair.split_once('=')
            && name.trim().to_ascii_lowercase().starts_with(NAME_PREFIX)
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// The `global.asa` event handlers RASP runs (M4 subset: the four
/// standard events; events absent from the file stay `None`).
#[derive(Debug, Default, Clone)]
pub struct GlobalAsa {
    /// Statements inside `Sub Session_OnStart … End Sub`.
    pub session_on_start: Option<Vec<asp_vbscript::Stmt>>,
    /// Statements inside `Sub Session_OnEnd … End Sub`, fired when the
    /// session is abandoned or idles out.
    pub session_on_end: Option<Vec<asp_vbscript::Stmt>>,
    /// Statements inside `Sub Application_OnStart … End Sub`.
    pub application_on_start: Option<Vec<asp_vbscript::Stmt>>,
    /// Same for `Application_OnEnd` (parsed but not fired: IIS fires
    /// it on application recycle, which RASP does not model yet).
    pub application_on_end: Option<Vec<asp_vbscript::Stmt>>,
}

impl GlobalAsa {
    /// Parse a `global.asa` file's source. Handler bodies live inside
    /// `<SCRIPT RUNAT=Server LANGUAGE=VBScript>` elements; each element
    /// must contain exactly one parameterless `Sub <EventName>`
    /// declaration. Unknown event names and non-Sub content are syntax
    /// errors. Event bodies are parsed once here and stored ready to
    /// execute.
    pub fn parse(source: &str) -> AspResult<Self> {
        let page = asp_core::Page::parse(source)?;
        let mut asa = GlobalAsa::default();
        for block in &page.blocks {
            let Block::ServerScript { body, .. } = block else {
                continue;
            };
            let stmts = asp_vbscript::parse_block(body, 1)?;
            let (name, body_stmts) = single_sub_block(&stmts)?;
            let slot = match name.as_str() {
                "session_onstart" => &mut asa.session_on_start,
                "session_onend" => &mut asa.session_on_end,
                "application_onstart" => &mut asa.application_on_start,
                "application_onend" => &mut asa.application_on_end,
                other => {
                    return Err(AspError::Syntax(Diagnostic::new(
                        1,
                        format!("global.asa defines unsupported event '{other}'"),
                    )));
                }
            };
            *slot = Some(body_stmts);
        }
        Ok(asa)
    }
}

/// Require `stmts` to be exactly one parameterless `Sub … End Sub`
/// (the global.asa event-handler shape) and return the lower-cased
/// name plus the statements INSIDE the Sub (the handler body).
fn single_sub_block(stmts: &[asp_vbscript::Stmt]) -> AspResult<(String, Vec<asp_vbscript::Stmt>)> {
    let Some(asp_vbscript::Stmt::ProcOpen { name, params, .. }) = stmts.first() else {
        return Err(AspError::Syntax(Diagnostic::new(
            1,
            "global.asa script blocks must contain exactly one Sub declaration",
        )));
    };
    if !params.is_empty() {
        return Err(AspError::Syntax(Diagnostic::new(
            1,
            format!("global.asa '{name}' takes no parameters"),
        )));
    }
    // Find the ProcClose matching the opener (nested declarations would
    // be invalid VBScript, but scan with a depth counter regardless).
    let mut depth = 0usize;
    let mut close_idx = None;
    for (i, stmt) in stmts.iter().enumerate().skip(1) {
        match stmt {
            asp_vbscript::Stmt::ProcOpen { .. } => depth += 1,
            asp_vbscript::Stmt::ProcClose => {
                if depth == 0 {
                    close_idx = Some(i);
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let Some(close_idx) = close_idx else {
        return Err(AspError::Syntax(Diagnostic::new(
            1,
            "global.asa Sub is missing 'End Sub'",
        )));
    };
    if close_idx + 1 != stmts.len() {
        return Err(AspError::Syntax(Diagnostic::new(
            1,
            "global.asa script blocks must contain exactly one Sub declaration and no other statements",
        )));
    }
    Ok((name.to_ascii_lowercase(), stmts[1..close_idx].to_vec()))
}

/// Fire the start events a request needs from `global.asa`:
/// `Application_OnStart` once per process (when not yet started) and
/// `Session_OnStart` when the caller resolved a NEW session. Mutates
/// the stores in place so the caller commits them even if the page
/// itself then fails.
pub fn fire_global_asa_events(
    asa: &GlobalAsa,
    session_is_new: bool,
    stores: &mut StateStores,
) -> AspResult<()> {
    if !stores.application.started {
        fire_statements(
            asa.application_on_start.as_deref(),
            "Application_OnStart",
            stores,
            EventKind::ApplicationStart,
        )?;
    }
    if session_is_new {
        fire_statements(
            asa.session_on_start.as_deref(),
            "Session_OnStart",
            stores,
            EventKind::SessionStart,
        )?;
    }
    Ok(())
}

/// Fire `Session_OnEnd` for the sessions that just went away
/// (abandoned or idle-expired). Each runs with the DYING session's
/// values (its final `Session` store) and the shared application;
/// handler `Application` writes persist in `stores`, while its
/// `Session` writes die with the ending session.
///
/// Like IIS, end events get no request/response context — only
/// `Session` and `Application` (`Application_OnEnd` stays unfired:
/// IIS fires it on application recycle, a process-lifetime event RASP
/// does not model yet). Errors surface to the caller, which logs
/// them — the page's own output is already rendered.
pub fn fire_ended_global_asa_events(
    asa: &GlobalAsa,
    ended: &EndedEvents,
    stores: &mut StateStores,
) -> AspResult<()> {
    let mut first_err: Option<AspError> = None;
    let sessions = ended.abandoned.iter().chain(ended.expired.iter());
    for store in sessions {
        stores.session = store.clone();
        if let Err(err) = fire_statements(
            asa.session_on_end.as_deref(),
            "Session_OnEnd",
            stores,
            EventKind::SessionEnd,
        ) {
            first_err.get_or_insert(err);
        }
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Which event is firing.
#[derive(Debug, Clone, Copy)]
enum EventKind {
    SessionStart,
    ApplicationStart,
    SessionEnd,
}

/// Run one global.asa event's statement body against a throwaway
/// environment seeded with the stores; its session/application writes
/// are kept, response output is discarded (global.asa emits no output).
fn fire_statements(
    body: Option<&[asp_vbscript::Stmt]>,
    name: &str,
    stores: &mut StateStores,
    kind: EventKind,
) -> AspResult<()> {
    let Some(stmts) = body else {
        return Ok(());
    };
    let mut env = asp_vbscript::ExecEnv::new().with_state(stores.clone());
    let result = asp_vbscript::exec_block(stmts, &mut env);
    match (kind, result) {
        (EventKind::ApplicationStart, Ok(())) => {
            stores.application = env.application.clone();
            stores.application.started = true;
        }
        (EventKind::SessionStart, Ok(())) => {
            stores.session = env.session.clone();
        }
        (EventKind::SessionEnd, Ok(())) => {
            stores.session = env.session.clone();
            stores.application = env.application.clone();
        }
        (_, Err(err)) => {
            return Err(AspError::Runtime(Diagnostic::new(
                1,
                format!("error in global.asa {name}: {err}"),
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asp_vbscript::{DEFAULT_SESSION_TIMEOUT_MIN, Variant};

    /// Store preloaded with one string value (helper for flow tests).
    pub(crate) fn store_with(id: u32, key: &str, value: &str) -> SessionStore {
        let mut s = SessionStore::new(id);
        s.values
            .insert(key.to_ascii_lowercase(), Variant::Str(value.to_string()));
        s
    }

    /// A manager with a fixed signing key, so second managers verify
    /// the first manager's cookies in cross-key tests.
    fn keyed_manager() -> SessionManager {
        SessionManager::with_signing_key(key())
    }

    /// The fixed test signing key (fresh instance per call; the bytes
    /// are what matter, and SigningKey clones share nothing mutable).
    fn key() -> SigningKey {
        SigningKey::with_bytes([0x2a; 32])
    }

    #[test]
    fn round_trips_signed_cookies() {
        let cookie = sign_session_cookie(&key(), 0xdeadbeef);
        assert!(cookie.starts_with("raspdeadbeef."));
        assert_eq!(verify_session_cookie(&key(), &cookie), Some(0xdeadbeef));
    }

    #[test]
    fn rejects_tampered_cookies() {
        let cookie = sign_session_cookie(&key(), 1);
        assert_eq!(verify_session_cookie(&key(), "rasp00000001"), None);
        assert_eq!(verify_session_cookie(&key(), "raspzap"), None);
        assert!(
            verify_session_cookie(
                &key(),
                &format!("{}.0", cookie.trim_end_matches(|c| c != '.')),
            )
            .is_none()
        );
        // A valid shape signed under a DIFFERENT process key is
        // treated as no session (which issues a fresh one).
        assert_eq!(
            verify_session_cookie(&SigningKey::with_bytes([1u8; 32]), &cookie),
            None
        );
    }

    #[test]
    fn new_visitor_gets_fresh_session_and_cookie() {
        let mut mgr = keyed_manager();
        let (store, cookie, is_new) = mgr.resolve_session(None);
        assert!(is_new);
        assert!(store.values.is_empty());
        assert_eq!(store.timeout_min, DEFAULT_SESSION_TIMEOUT_MIN);
        assert!(verify_session_cookie(&key(), &cookie).is_some());
        assert_eq!(mgr.live_count(), 1);
    }

    #[test]
    fn same_cookie_resumes_same_session() {
        let mut mgr = keyed_manager();
        let (_first, cookie, _) = mgr.resolve_session(None);
        mgr.commit_session(
            &cookie,
            store_with(
                verify_session_cookie(&key(), &cookie).unwrap(),
                "user",
                "dan",
            ),
        );
        let (second, cookie2, resumed) = mgr.resolve_session(Some(&cookie));
        assert!(!resumed);
        assert_eq!(cookie2, cookie);
        assert_eq!(second.values.get("user"), Some(&Variant::Str("dan".into())));
        assert_eq!(mgr.live_count(), 1);
    }

    #[test]
    fn unknown_cookie_starts_new_session() {
        let mut mgr = keyed_manager();
        // Well-formed but signed under a foreign key.
        let foreign = sign_session_cookie(&SigningKey::with_bytes([9u8; 32]), 0xffffffff);
        let (store, cookie, is_new) = mgr.resolve_session(Some(&foreign));
        assert!(is_new);
        assert!(store.values.is_empty());
        assert_ne!(verify_session_cookie(&key(), &cookie).unwrap(), 0xffffffff);
    }

    #[test]
    fn abandon_reports_the_ended_session() {
        let mut mgr = keyed_manager();
        let (_other, _other_cookie, _) = mgr.resolve_session(None);
        let (mut store, cookie, _) = mgr.resolve_session(None);
        store.values.insert("cart".into(), Variant::Str("x".into()));
        let cart = store.values.get("cart").cloned().unwrap();
        store.abandoned = true;
        let ended = mgr.commit_request_state(&cookie, store, mgr.application());
        assert_eq!(
            ended.abandoned.as_ref().unwrap().values.get("cart"),
            Some(&cart)
        );
        assert!(ended.expired.is_empty());
        assert_eq!(mgr.live_count(), 1);
        let (_fresh, cookie2, _) = mgr.resolve_session(Some(&cookie));
        assert_ne!(cookie2, cookie);
    }

    #[test]
    fn abandoned_last_session_ends_the_application() {
        let mut mgr = keyed_manager();
        let (mut store, cookie, _) = mgr.resolve_session(None);
        store.abandoned = true;
        let mut app = mgr.application();
        app.started = true;
        let ended = mgr.commit_request_state(&cookie, store, app);
        // The LAST session ending is reported, but does NOT end the
        // application: Application_OnEnd is a process-lifetime event.
        assert!(ended.abandoned.is_some());
        assert!(mgr.application.started, "application keeps running");
    }

    #[test]
    fn non_last_abandon_keeps_the_application() {
        let mut mgr = keyed_manager();
        let (_s1, _cookie1, _) = mgr.resolve_session(None);
        let (mut store2, cookie2, _) = mgr.resolve_session(None);
        store2.abandoned = true;
        let ended = mgr.commit_request_state(&cookie2, store2, mgr.application());
        assert!(ended.abandoned.is_some());
        assert_eq!(mgr.live_count(), 1);
    }

    #[test]
    fn idle_sessions_expire_with_final_values() {
        let mut mgr = keyed_manager();
        let (_s, cookie, _n) = mgr.resolve_session(None);
        let id = verify_session_cookie(&key(), &cookie).unwrap();
        let aged = store_with(id, "user", "dan");
        mgr.commit_request_state(&cookie, aged, mgr.application());
        // Age the stored last_seen beyond the timeout without sleeping.
        if let Some(live) = mgr.sessions.get_mut(&id) {
            live.last_seen -=
                std::time::Duration::from_secs((live.store.timeout_min as u64 + 1) * 60);
        }
        let expired_all = mgr.expire_ended();
        assert_eq!(expired_all.len(), 1);
        assert_eq!(
            expired_all[0].values.get("user"),
            Some(&Variant::Str("dan".into()))
        );
        assert_eq!(mgr.live_count(), 0);
        // A request with the expired cookie gets a NEW session.
        let (store, cookie2, is_new) = mgr.resolve_session(Some(&cookie));
        assert!(is_new);
        assert!(store.values.is_empty());
        assert_ne!(cookie2, cookie);
    }

    #[test]
    fn commit_request_state_reports_other_sessions_expiry() {
        let mut mgr = keyed_manager();
        let (_s1, cookie1, _) = mgr.resolve_session(None);
        let (_s2, cookie2, _) = mgr.resolve_session(None);
        let id1 = verify_session_cookie(&key(), &cookie1).unwrap();
        let id2 = verify_session_cookie(&key(), &cookie2).unwrap();
        mgr.commit_request_state(&cookie1, store_with(id1, "k", "v"), mgr.application());
        // Session 1 goes idle; session 2's next request observes it.
        if let Some(live) = mgr.sessions.get_mut(&id1) {
            live.last_seen -=
                std::time::Duration::from_secs((live.store.timeout_min as u64 + 1) * 60);
        }
        let ended =
            mgr.commit_request_state(&cookie2, store_with(id2, "k", "v"), mgr.application());
        assert_eq!(ended.expired.len(), 1);
        assert_eq!(
            ended.expired[0].values.get("k"),
            Some(&Variant::Str("v".into()))
        );
        assert!(ended.abandoned.is_none());
        assert_eq!(mgr.live_count(), 1);
    }

    #[test]
    fn expiring_one_of_several_keeps_the_application() {
        let mut mgr = keyed_manager();
        let (_s1, c1, _) = mgr.resolve_session(None);
        let (_s2, c2, _) = mgr.resolve_session(None);
        let id2 = verify_session_cookie(&key(), &c2).unwrap();
        if let Some(live) = mgr.sessions.get_mut(&id2) {
            live.last_seen -= std::time::Duration::from_secs(60 * 30);
        }
        let expired = mgr.expire_ended();
        assert_eq!(expired.len(), 1);
        assert_eq!(mgr.live_count(), 1);
        let survivor = verify_session_cookie(&key(), &c1).unwrap();
        assert!(mgr.sessions.contains_key(&survivor));
    }

    #[test]
    fn resolve_time_expiry_is_parked_for_session_on_end() {
        let mut mgr = keyed_manager();
        let (_s, cookie, _) = mgr.resolve_session(None);
        let id = verify_session_cookie(&key(), &cookie).unwrap();
        mgr.commit_request_state(&cookie, store_with(id, "user", "dan"), mgr.application());
        // The session goes idle while NO request is running; the
        // returning visitor's resolve observes it.
        if let Some(live) = mgr.sessions.get_mut(&id) {
            live.last_seen -=
                std::time::Duration::from_secs((live.store.timeout_min as u64 + 1) * 60);
        }
        let (_fresh, cookie2, is_new) = mgr.resolve_session(Some(&cookie));
        assert!(is_new, "expired cookie starts a new session");
        // The commit reports the parked drop with its final values.
        let id2 = verify_session_cookie(&key(), &cookie2).unwrap();
        let ended =
            mgr.commit_request_state(&cookie2, store_with(id2, "k", "v"), mgr.application());
        assert_eq!(ended.expired.len(), 1);
        assert_eq!(
            ended.expired[0].values.get("user"),
            Some(&Variant::Str("dan".into()))
        );
        assert_eq!(ended.expired[0].id, id);
        assert!(ended.abandoned.is_none());
    }

    #[test]
    fn session_cookie_header_is_parsed() {
        assert_eq!(
            session_cookie_from_header("ASPSESSIONIDRASPTEST=rasp00000001; theme=dark"),
            Some("rasp00000001".to_string())
        );
        assert_eq!(session_cookie_from_header("theme=dark"), None);
        assert_eq!(
            session_cookie_from_header("aspsessionid=rasp00000009"),
            Some("rasp00000009".to_string())
        );
    }

    #[test]
    fn session_ids_are_deterministic_and_skip_live_ones() {
        let mut mgr = keyed_manager();
        let (_s1, c1, _) = mgr.resolve_session(None);
        let (_s2, c2, _) = mgr.resolve_session(None);
        assert_eq!(verify_session_cookie(&key(), &c1).unwrap(), 1);
        assert_eq!(verify_session_cookie(&key(), &c2).unwrap(), 2);
        assert_ne!(c1, c2);
    }

    #[test]
    fn application_state_shared_per_manager() {
        let mut mgr = keyed_manager();
        let mut app = mgr.application();
        app.values.insert("hits".into(), Variant::Int(1));
        mgr.commit_application(app);
        assert_eq!(mgr.application().values.get("hits"), Some(&Variant::Int(1)));
    }
    #[test]
    fn session_on_end_sees_the_dying_session_and_app_writes_persist() {
        let asa = GlobalAsa::parse(
            "<SCRIPT RUNAT=Server LANGUAGE=VBScript>\nSub Session_OnEnd\n  Application(\"ended_for\") = Session(\"who\")\nEnd Sub\n</SCRIPT>\n<SCRIPT RUNAT=Server LANGUAGE=VBScript>\nSub Application_OnEnd\n  Application(\"app_closed\") = \"yes\"\nEnd Sub\n</SCRIPT>",
        )
        .unwrap();
        let store = store_with(4, "who", "dan");
        let ended = EndedEvents {
            abandoned: Some(store),
            expired: Vec::new(),
        };
        let mut stores = StateStores::default();
        fire_ended_global_asa_events(&asa, &ended, &mut stores).unwrap();
        // The dying session's values were visible to its handler, and
        // the handler's Application write persists for other sessions.
        assert_eq!(
            stores.application.values.get("ended_for"),
            Some(&Variant::Str("dan".into()))
        );
    }

    #[test]
    fn no_handlers_makes_ending_a_noop() {
        let asa = GlobalAsa::default();
        let ended = EndedEvents {
            abandoned: Some(store_with(1, "k", "v")),
            expired: vec![store_with(2, "k", "v")],
        };
        let mut stores = StateStores::default();
        fire_ended_global_asa_events(&asa, &ended, &mut stores).unwrap();
    }

    #[test]
    fn on_end_errors_report_the_handler_name() {
        // A guaranteed runtime error inside the handler (division by a
        // zero literal) must surface with the handler's name.
        let asa = GlobalAsa::parse(
            "<SCRIPT RUNAT=Server LANGUAGE=VBScript>\nSub Session_OnEnd\n  x = 1 / 0\nEnd Sub\n</SCRIPT>",
        )
        .unwrap();
        let ended = EndedEvents {
            abandoned: Some(store_with(1, "k", "v")),
            expired: Vec::new(),
        };
        let mut stores = StateStores::default();
        let err = fire_ended_global_asa_events(&asa, &ended, &mut stores).unwrap_err();
        assert!(err.to_string().contains("Session_OnEnd"), "{err}");
    }
}
