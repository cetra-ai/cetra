-- Track IC M4: issue notifications and links (D-IC-21).
ALTER TABLE issue_sources ADD COLUMN last_notified_at INTEGER;
-- Existing enabled sources start now, so a backlog is never sent (D-IC-21).
UPDATE issue_sources
   SET last_notified_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000000000
 WHERE enabled = 1;
CREATE INDEX issues_source ON issues (source_id);

CREATE TABLE issue_source_channels (
    source_id  TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    channel_id TEXT NOT NULL REFERENCES notification_channels(id) ON DELETE CASCADE,
    PRIMARY KEY (source_id, channel_id)
);
CREATE INDEX issue_source_channels_channel ON issue_source_channels (channel_id);

CREATE TABLE issue_notifications (
    id         TEXT PRIMARY KEY,
    source_id  TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    digest     TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX issue_notifications_created ON issue_notifications (created_at);

-- SQLite cannot drop NOT NULL: rebuild notification_deliveries. No table references it.
CREATE TABLE notification_deliveries_new (
    id                    TEXT PRIMARY KEY,
    event_id              TEXT REFERENCES alert_events(id) ON DELETE CASCADE,
    issue_notification_id TEXT REFERENCES issue_notifications(id) ON DELETE CASCADE,
    channel_id            TEXT NOT NULL REFERENCES notification_channels(id) ON DELETE CASCADE,
    status                TEXT NOT NULL,
    attempts              INTEGER NOT NULL DEFAULT 0,
    next_attempt_at       INTEGER,
    last_error            TEXT,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL,
    CHECK ((event_id IS NULL) <> (issue_notification_id IS NULL))
);
INSERT INTO notification_deliveries_new
    (id, event_id, channel_id, status, attempts, next_attempt_at, last_error, created_at, updated_at)
SELECT id, event_id, channel_id, status, attempts, next_attempt_at, last_error, created_at, updated_at
  FROM notification_deliveries;
DROP TABLE notification_deliveries;
ALTER TABLE notification_deliveries_new RENAME TO notification_deliveries;
CREATE INDEX notification_deliveries_due ON notification_deliveries(status, next_attempt_at);
CREATE INDEX notification_deliveries_event ON notification_deliveries(event_id);
CREATE INDEX notification_deliveries_channel ON notification_deliveries(channel_id, updated_at);
CREATE INDEX notification_deliveries_issue ON notification_deliveries(issue_notification_id);
