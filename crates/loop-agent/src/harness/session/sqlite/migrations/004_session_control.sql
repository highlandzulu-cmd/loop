CREATE TABLE IF NOT EXISTS session_control (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL DEFAULT 0,
    branch_revision INTEGER NOT NULL DEFAULT 0,
    pending TEXT
);
CREATE TABLE IF NOT EXISTS session_destinations (
    request_key TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    result TEXT NOT NULL
);
