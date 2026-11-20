-- In-app assistant conversations (track AS, D-AS-10).
CREATE TABLE assistant_conversations (
  id TEXT PRIMARY KEY,                 -- UUIDv7
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  title TEXT NOT NULL DEFAULT '',      -- '' until the first user message (M3) or a rename
  message_count INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  last_active_at INTEGER NOT NULL
);
CREATE INDEX assistant_conversations_owner ON assistant_conversations(project_id, user_id, last_active_at);
CREATE INDEX assistant_conversations_idle ON assistant_conversations(last_active_at);
CREATE TABLE assistant_messages (
  conversation_id TEXT NOT NULL REFERENCES assistant_conversations(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,                -- 1, 2, … within the conversation
  role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'tool')),
  parts TEXT NOT NULL,                 -- JSON array of message parts (spec 04 § Assistant)
  context TEXT,                        -- JSON page context of a user message (D-AS-14)
  status TEXT NOT NULL DEFAULT 'complete' CHECK (status IN ('complete', 'cancelled', 'failed')),
  feedback TEXT CHECK (feedback IN ('up', 'down')),
  turn_trace_id TEXT,                  -- 32 lowercase hex characters
  stub_after INTEGER,                  -- tool messages: when their results become stubs; NULL once stubbed
  created_at INTEGER NOT NULL,
  PRIMARY KEY (conversation_id, seq)
);
CREATE INDEX assistant_messages_stub ON assistant_messages(stub_after) WHERE stub_after IS NOT NULL;
