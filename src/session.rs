use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const SESSION_TIMEOUT_SECS: u64 = 3600;

pub struct SessionStore {
    sessions: HashMap<String, Session>,
}

struct Session {
    data: HashMap<String, String>,
    last_access: u64,
}

#[allow(dead_code)]
impl SessionStore {
    pub fn new() -> Self {
        SessionStore {
            sessions: HashMap::new(),
        }
    }

    pub fn create_session(&mut self) -> String {
        let id = generate_session_id();
        self.sessions.insert(
            id.clone(),
            Session {
                data: HashMap::new(),
                last_access: now_secs(),
            },
        );
        id
    }

    pub fn get(&mut self, id: &str) -> Option<&HashMap<String, String>> {
        let now = now_secs();
        if let Some(session) = self.sessions.get_mut(id) {
            if now - session.last_access > SESSION_TIMEOUT_SECS {
                self.sessions.remove(id);
                return None;
            }
            session.last_access = now;
            Some(&self.sessions[id].data)
        } else {
            None
        }
    }

    pub fn set(&mut self, id: &str, key: &str, value: &str) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.data.insert(key.to_string(), value.to_string());
            session.last_access = now_secs();
        }
    }

    pub fn destroy(&mut self, id: &str) {
        self.sessions.remove(id);
    }

    pub fn purge_expired(&mut self) {
        let now = now_secs();
        self.sessions
            .retain(|_, s| now - s.last_access <= SESSION_TIMEOUT_SECS);
    }
}

pub fn parse_session_id(cookie_header: &str) -> Option<String> {
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("session_id=") {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn generate_session_id() -> String {
    let t = now_secs();
    let pseudo = t ^ (t << 17) ^ (t >> 3);
    format!("{:016x}{:016x}", pseudo, t.wrapping_mul(6364136223846793005))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
