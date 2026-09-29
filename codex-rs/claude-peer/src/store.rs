use crate::protocol::ReceivedMessage;
use crate::registry::private_directory;
use anyhow::Context;
use anyhow::Result;
use sqlx::Row;
use sqlx::SqlitePool;
use std::path::Path;

pub(crate) struct Inbox {
    pool: SqlitePool,
}

impl Inbox {
    pub async fn open(path: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        private_directory(path.parent().context("peer store needs parent")?)?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        drop(file);
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.is_file()
                && metadata.uid() == crate::registry::uid()
                && metadata.mode() & 0o077 == 0,
            "peer database permissions are invalid"
        );
        let sqlite = codex_state::SqliteConfig::from_sqlite_home(
            codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                path.parent()
                    .context("peer store needs parent")?
                    .canonicalize()?,
            )?,
        );
        let pool = sqlite.open_read_write_pool(path).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS peer_inbox (seq INTEGER PRIMARY KEY AUTOINCREMENT, sender TEXT NOT NULL, msg_id TEXT NOT NULL, state TEXT NOT NULL, payload TEXT NOT NULL, received_at INTEGER NOT NULL, UNIQUE(sender, msg_id))")
            .execute(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn receive(&self, message: &ReceivedMessage, state: &str) -> Result<&'static str> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // 전달한 본문은 삭제하고 중복 방지 키만 일정 기간 보관한다.
        sqlx::query("DELETE FROM peer_inbox WHERE state = 'delivered' AND received_at < ?")
            .bind(chrono::Utc::now().timestamp_millis() - 86_400_000)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM peer_inbox WHERE state = 'delivered' AND seq NOT IN (SELECT seq FROM peer_inbox ORDER BY seq DESC LIMIT 10000)")
            .execute(&mut *tx).await?;
        let sender = &message.sender_session;
        let exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM peer_inbox WHERE sender = ? AND msg_id = ?")
                .bind(sender)
                .bind(&message.id)
                .fetch_one(&mut *tx)
                .await?;
        if exists != 0 {
            return Ok("duplicate");
        }
        let limit = if state == "held" { 100 } else { 50 };
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peer_inbox WHERE state = ? OR (? = 'pending' AND state IN ('released','processing'))")
            .bind(state)
            .bind(state).fetch_one(&mut *tx).await?;
        if count >= limit {
            return Ok("queue-full");
        }
        sqlx::query("INSERT INTO peer_inbox(sender, msg_id, state, payload, received_at) VALUES(?, ?, ?, ?, ?)")
            .bind(sender).bind(&message.id).bind(state).bind(serde_json::to_string(message)?)
            .bind(chrono::Utc::now().timestamp_millis()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok("accepted")
    }

    pub async fn pending(&self) -> Result<Option<(i64, ReceivedMessage)>> {
        let row = sqlx::query("SELECT seq, payload, state FROM peer_inbox WHERE state IN ('pending', 'released') ORDER BY seq LIMIT 1")
            .fetch_optional(&self.pool).await?;
        row.map(|row| {
            let mut message: ReceivedMessage = serde_json::from_str(row.try_get("payload")?)?;
            message.approval_released |= row.try_get::<&str, _>("state")? == "released";
            Ok((row.try_get("seq")?, message))
        })
        .transpose()
    }

    pub async fn delivered(&self, seq: i64) -> Result<()> {
        sqlx::query("UPDATE peer_inbox SET state = 'delivered', payload = '' WHERE seq = ? AND state IN ('pending', 'released', 'processing')")
            .bind(seq).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn held(&self) -> Result<Vec<(i64, ReceivedMessage)>> {
        sqlx::query(
            "SELECT seq, payload FROM peer_inbox WHERE state = 'held' ORDER BY seq LIMIT 100",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get("seq")?,
                serde_json::from_str(row.try_get("payload")?)?,
            ))
        })
        .collect()
    }

    pub async fn claim(&self, seq: i64, message: &ReceivedMessage) -> Result<()> {
        sqlx::query("UPDATE peer_inbox SET state = 'processing', payload = ? WHERE seq = ? AND state IN ('pending','released')")
            .bind(serde_json::to_string(message)?).bind(seq).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn release(&self, seq: i64) -> Result<()> {
        sqlx::query(
            "UPDATE peer_inbox SET state = 'pending' WHERE seq = ? AND state = 'processing'",
        )
        .bind(seq)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn processing(&self) -> Result<Vec<(i64, ReceivedMessage)>> {
        sqlx::query(
            "SELECT seq, payload FROM peer_inbox WHERE state = 'processing' ORDER BY seq LIMIT 50",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get("seq")?,
                serde_json::from_str(row.try_get("payload")?)?,
            ))
        })
        .collect()
    }

    pub async fn hold(&self, seq: i64) -> Result<()> {
        sqlx::query("UPDATE peer_inbox SET state = 'held' WHERE seq = ? AND state = 'pending'")
            .bind(seq)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn decide(&self, seq: i64, approve: bool) -> Result<Option<ReceivedMessage>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if approve {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM peer_inbox WHERE state IN ('pending', 'released', 'processing')",
            )
            .fetch_one(&mut *tx)
            .await?;
            anyhow::ensure!(
                count < 50,
                "peer pending queue is full; retry approval after pending messages are delivered"
            );
        }
        let row = sqlx::query("SELECT payload FROM peer_inbox WHERE seq = ? AND state = 'held'")
            .bind(seq)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let message = serde_json::from_str(row.try_get("payload")?)?;
        sqlx::query("UPDATE peer_inbox SET state = ?, payload = CASE WHEN ? THEN payload ELSE '' END WHERE seq = ?")
            .bind(if approve { "released" } else { "denied" }).bind(true).bind(seq).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Some(message))
    }

    pub async fn denied(&self) -> Result<Vec<(i64, ReceivedMessage)>> {
        sqlx::query(
            "SELECT seq, payload FROM peer_inbox WHERE state = 'denied' ORDER BY seq LIMIT 100",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get("seq")?,
                serde_json::from_str(row.try_get("payload")?)?,
            ))
        })
        .collect()
    }

    pub async fn receipt_sent(&self, seq: i64) -> Result<()> {
        sqlx::query("UPDATE peer_inbox SET state = 'delivered', payload = '' WHERE seq = ? AND state = 'denied'")
            .bind(seq).execute(&self.pool).await?;
        Ok(())
    }
}
