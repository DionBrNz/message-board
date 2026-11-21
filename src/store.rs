//! SQLite-backed storage for agents, channels and messages.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum StoreError {
    Conflict(String),
    NotFound(String),
    Invalid(String),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Db(e)
    }
}

type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub name: String,
    pub description: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Channel {
    pub name: String,
    pub description: String,
    pub created_by: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: i64,
    /// Channel name; `None` for direct messages.
    pub channel: Option<String>,
    pub sender: String,
    /// Recipient agent; `Some` only for direct messages.
    pub recipient: Option<String>,
    pub reply_to: Option<i64>,
    pub body: String,
    pub mentions: Vec<String>,
    pub created_at: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Names for agents and channels: 1-32 chars of `[A-Za-z0-9_-]`.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Extract `@name` tokens from a message body.
pub fn extract_mentions(body: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut chars = body.char_indices().peekable();
    let mut prev_ok = true; // start of string or after a non-name char
    while let Some((i, c)) = chars.next() {
        if c == '@' && prev_ok {
            let rest = &body[i + 1..];
            let end = rest
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
                .unwrap_or(rest.len());
            if end > 0 {
                out.insert(rest[..end].to_string());
            }
        }
        prev_ok = !(c.is_ascii_alphanumeric() || c == '_' || c == '-');
    }
    out
}

const MSG_COLS: &str = "m.id, m.channel, m.sender, m.recipient, m.reply_to, m.body, m.created_at";

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS agents (
               name TEXT PRIMARY KEY, description TEXT NOT NULL,
               token TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS channels (
               name TEXT PRIMARY KEY, description TEXT NOT NULL,
               created_by TEXT NOT NULL REFERENCES agents(name), created_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS messages (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               channel TEXT REFERENCES channels(name),
               sender TEXT NOT NULL REFERENCES agents(name),
               recipient TEXT REFERENCES agents(name),
               reply_to INTEGER REFERENCES messages(id),
               body TEXT NOT NULL, created_at INTEGER NOT NULL,
               CHECK ((channel IS NULL) != (recipient IS NULL)));
             CREATE INDEX IF NOT EXISTS idx_msg_channel ON messages(channel, id);
             CREATE INDEX IF NOT EXISTS idx_msg_recipient ON messages(recipient, id);
             CREATE TABLE IF NOT EXISTS mentions (
               message_id INTEGER NOT NULL REFERENCES messages(id),
               agent TEXT NOT NULL REFERENCES agents(name),
               PRIMARY KEY (message_id, agent));",
        )?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register an agent, returning its record and a fresh bearer token.
    pub fn register_agent(&self, name: &str, description: &str) -> Result<(Agent, String)> {
        if !valid_name(name) {
            return Err(StoreError::Invalid("name must be 1-32 chars of [A-Za-z0-9_-]".into()));
        }
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let created_at = now();
        let res = self.conn().execute(
            "INSERT INTO agents (name, description, token, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, description, token, created_at],
        );
        match res {
            Ok(_) => Ok((Agent { name: name.into(), description: description.into(), created_at }, token)),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                Err(StoreError::Conflict(format!("agent '{name}' already exists")))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn agent_by_token(&self, token: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT name FROM agents WHERE token = ?1", [token], |r| r.get(0))
            .optional()?)
    }

    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT name, description, created_at FROM agents ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(Agent { name: r.get(0)?, description: r.get(1)?, created_at: r.get(2)? })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn create_channel(&self, name: &str, description: &str, by: &str) -> Result<Channel> {
        if !valid_name(name) {
            return Err(StoreError::Invalid("channel name must be 1-32 chars of [A-Za-z0-9_-]".into()));
        }
        let created_at = now();
        let res = self.conn().execute(
            "INSERT INTO channels (name, description, created_by, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, description, by, created_at],
        );
        match res {
            Ok(_) => Ok(Channel { name: name.into(), description: description.into(), created_by: by.into(), created_at }),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                Err(StoreError::Conflict(format!("channel '{name}' already exists")))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_channels(&self) -> Result<Vec<Channel>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT name, description, created_by, created_at FROM channels ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(Channel { name: r.get(0)?, description: r.get(1)?, created_by: r.get(2)?, created_at: r.get(3)? })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Post to a channel (`channel`) or directly to an agent (`recipient`); exactly one must be set.
    pub fn post_message(
        &self,
        sender: &str,
        channel: Option<&str>,
        recipient: Option<&str>,
        reply_to: Option<i64>,
        body: &str,
    ) -> Result<Message> {
        if body.trim().is_empty() {
            return Err(StoreError::Invalid("body must not be empty".into()));
        }
        if body.len() > 64 * 1024 {
            return Err(StoreError::Invalid("body exceeds 64 KiB".into()));
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if let Some(c) = channel {
            let ok: Option<i64> = tx.query_row("SELECT 1 FROM channels WHERE name = ?1", [c], |r| r.get(0)).optional()?;
            if ok.is_none() {
                return Err(StoreError::NotFound(format!("channel '{c}' not found")));
            }
        }
        if let Some(r) = recipient {
            let ok: Option<i64> = tx.query_row("SELECT 1 FROM agents WHERE name = ?1", [r], |r| r.get(0)).optional()?;
            if ok.is_none() {
                return Err(StoreError::NotFound(format!("agent '{r}' not found")));
            }
        }
        if let Some(rt) = reply_to {
            let parent: Option<(Option<String>, String, Option<String>)> = tx
                .query_row("SELECT channel, sender, recipient FROM messages WHERE id = ?1", [rt], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            let Some((pc, ps, pr)) = parent else {
                return Err(StoreError::NotFound(format!("message {rt} not found")));
            };
            let same_thread = match (channel, recipient) {
                (Some(c), None) => pc.as_deref() == Some(c),
                (None, Some(r)) => pr.is_some() && ((ps == sender && pr.as_deref() == Some(r)) || (ps == r && pr.as_deref() == Some(sender))),
                _ => false,
            };
            if !same_thread {
                return Err(StoreError::Invalid("reply_to must reference a message in the same channel/conversation".into()));
            }
        }
        let created_at = now();
        tx.execute(
            "INSERT INTO messages (channel, sender, recipient, reply_to, body, created_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![channel, sender, recipient, reply_to, body, created_at],
        )?;
        let id = tx.last_insert_rowid();
        let mut mentions = Vec::new();
        for name in extract_mentions(body) {
            let n = tx.execute(
                "INSERT INTO mentions (message_id, agent) SELECT ?1, name FROM agents WHERE name = ?2",
                params![id, name],
            )?;
            if n > 0 {
                mentions.push(name);
            }
        }
        tx.commit()?;
        Ok(Message {
            id,
            channel: channel.map(Into::into),
            sender: sender.into(),
            recipient: recipient.map(Into::into),
            reply_to,
            body: body.into(),
            mentions,
            created_at,
        })
    }

    fn with_mentions(conn: &Connection, mut msgs: Vec<Message>) -> Result<Vec<Message>> {
        let mut stmt = conn.prepare("SELECT agent FROM mentions WHERE message_id = ?1 ORDER BY agent")?;
        for m in &mut msgs {
            m.mentions = stmt.query_map([m.id], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        }
        Ok(msgs)
    }

    fn row_to_msg(r: &rusqlite::Row) -> rusqlite::Result<Message> {
        Ok(Message {
            id: r.get(0)?,
            channel: r.get(1)?,
            sender: r.get(2)?,
            recipient: r.get(3)?,
            reply_to: r.get(4)?,
            body: r.get(5)?,
            mentions: vec![],
            created_at: r.get(6)?,
        })
    }

    /// Channel messages with id > `after`, oldest first.
    pub fn channel_messages(&self, channel: &str, after: i64, limit: i64) -> Result<Vec<Message>> {
        let conn = self.conn();
        let exists: Option<i64> = conn.query_row("SELECT 1 FROM channels WHERE name = ?1", [channel], |r| r.get(0)).optional()?;
        if exists.is_none() {
            return Err(StoreError::NotFound(format!("channel '{channel}' not found")));
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT {MSG_COLS} FROM messages m WHERE m.channel = ?1 AND m.id > ?2 ORDER BY m.id LIMIT ?3"
        ))?;
        let msgs = stmt.query_map(params![channel, after, limit], Self::row_to_msg)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Self::with_mentions(&conn, msgs)
    }

    /// Everything addressed to `agent` (DMs and @mentions) with id > `after`, oldest first.
    pub fn inbox(&self, agent: &str, after: i64, limit: i64) -> Result<Vec<Message>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MSG_COLS} FROM messages m
             WHERE m.id > ?2 AND m.sender != ?1
               AND (m.recipient = ?1 OR EXISTS (SELECT 1 FROM mentions x WHERE x.message_id = m.id AND x.agent = ?1))
             ORDER BY m.id LIMIT ?3"
        ))?;
        let msgs = stmt.query_map(params![agent, after, limit], Self::row_to_msg)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Self::with_mentions(&conn, msgs)
    }

    /// A message plus all its transitive replies, oldest first.
    pub fn thread(&self, root: i64) -> Result<Vec<Message>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "WITH RECURSIVE t(id) AS (
               SELECT id FROM messages WHERE id = ?1
               UNION SELECT m.id FROM messages m JOIN t ON m.reply_to = t.id)
             SELECT {MSG_COLS} FROM messages m JOIN t ON t.id = m.id ORDER BY m.id"
        ))?;
        let msgs = stmt.query_map([root], Self::row_to_msg)?.collect::<std::result::Result<Vec<_>, _>>()?;
        if msgs.is_empty() {
            return Err(StoreError::NotFound(format!("message {root} not found")));
        }
        Self::with_mentions(&conn, msgs)
    }
}
