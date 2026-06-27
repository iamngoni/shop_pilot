-- Shop Pilot — initial schema.
-- Identity model: an internal user can be reached through one or more channel
-- identities (telegram/whatsapp). Store sessions are held per user and never
-- include the user's store password.

CREATE TABLE users (
  id          TEXT PRIMARY KEY,        -- internal uuid
  created_at  INTEGER NOT NULL          -- unix seconds
);

-- Maps a channel-scoped user (e.g. a Telegram chat id) to an internal user.
CREATE TABLE channel_identities (
  channel          TEXT NOT NULL,       -- 'telegram' | 'whatsapp'
  channel_user_id  TEXT NOT NULL,       -- chat id / wa id
  user_id          TEXT NOT NULL REFERENCES users(id),
  created_at       INTEGER NOT NULL,
  PRIMARY KEY (channel, channel_user_id)
);

-- A store session. status drives re-auth prompts.
CREATE TABLE store_sessions (
  id              TEXT PRIMARY KEY,
  user_id         TEXT NOT NULL REFERENCES users(id),
  store           TEXT NOT NULL,        -- 'sixty60'
  session_token   BLOB NOT NULL,        -- session credential
  status          TEXT NOT NULL,        -- 'active' | 'expired' | 'revoked'
  expires_at      INTEGER,              -- unix seconds, if known
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);

CREATE INDEX idx_store_sessions_user ON store_sessions(user_id, store);
