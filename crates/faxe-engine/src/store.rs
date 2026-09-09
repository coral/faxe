use std::{
    fs::{self, File},
    path::Path,
    time::Duration,
};

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::{
    Destination, Error, FaxRequest, Job, JobState, Result, SipProfile,
    model::{nonempty, validate_destination},
};

pub struct Store {
    connection: Connection,
    _lease: File,
}

impl Store {
    /// A single process owns the queue, including recovery of interrupted calls.
    pub fn open(directory: &Path) -> Result<Self> {
        fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        let lease = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("engine.lock"))?;
        lease.try_lock().map_err(|_| {
            Error::Invalid("This Faxe data directory is already in use by another engine".into())
        })?;
        let connection = Connection::open(directory.join("faxe.sqlite3"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;",
        )?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => connection.execute_batch(
                "BEGIN IMMEDIATE;
                CREATE TABLE profiles (id TEXT PRIMARY KEY, data TEXT NOT NULL);
                CREATE TABLE destinations (id TEXT PRIMARY KEY, profile_id TEXT NOT NULL REFERENCES profiles(id), data TEXT NOT NULL);
                CREATE TABLE jobs (id TEXT PRIMARY KEY, created TEXT NOT NULL, status TEXT NOT NULL, data TEXT NOT NULL);
                CREATE INDEX jobs_status_created ON jobs(status, created);
                PRAGMA user_version=1;
                COMMIT;"
            )?,
            1 | 2 => (),
            other => return Err(Error::Invalid(format!("Database schema {other} is newer than this application supports"))),
        }
        if version < 2 {
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE received_faxes (id TEXT PRIMARY KEY, arrived TEXT NOT NULL, data TEXT NOT NULL);
                CREATE TABLE preferences (key TEXT PRIMARY KEY, data TEXT NOT NULL);
                PRAGMA user_version=2; COMMIT;")?;
        }
        let mut store = Self {
            connection,
            _lease: lease,
        };
        let mut receiving = store.receive_settings()?;
        if receiving.folder.is_none() {
            // Respect redirected Windows Known Folders and Linux XDG user dirs.
            // Resolve once and persist; never replace a user's selected folder,
            // even when it is temporarily unavailable.
            receiving.folder = Some(match directories::UserDirs::new() {
                Some(dirs) => dirs.document_dir().unwrap_or(dirs.home_dir()).to_owned(),
                // Package-local storage is already absolute. Canonicalizing it
                // can require access to ancestors outside an AppContainer.
                None if directory.is_absolute() => directory.to_owned(),
                None => fs::canonicalize(directory)?,
            });
            store.save_receive_settings(&receiving)?;
        }
        for mut job in store.jobs()? {
            if job.state.is_active() {
                job.state = JobState::Interrupted;
                store.update_job(&mut job)?;
            }
        }
        for mut fax in store.received_faxes()? {
            if fax.outcome.is_active() {
                fax.outcome = crate::ReceptionOutcome::Interrupted;
                fax.result = Some(crate::ReceptionResult::Interrupted);
                fax.finished_at = Some(Utc::now());
                store.save_received(&fax)?;
            }
        }
        Ok(store)
    }

    pub fn receive_settings(&self) -> Result<crate::ReceiveSettings> {
        let data: Option<String> = self
            .connection
            .query_row(
                "SELECT data FROM preferences WHERE key='receive'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        data.map(|data| serde_json::from_str(&data).map_err(Into::into))
            .unwrap_or_else(|| Ok(Default::default()))
    }
    pub fn save_receive_settings(&mut self, settings: &crate::ReceiveSettings) -> Result<()> {
        settings.validate(&self.profiles()?)?;
        self.connection.execute("INSERT INTO preferences(key,data) VALUES ('receive',?1) ON CONFLICT(key) DO UPDATE SET data=excluded.data", [serde_json::to_string(settings)?])?;
        Ok(())
    }
    pub fn received_faxes(&self) -> Result<Vec<crate::ReceivedFax>> {
        self.read_list("SELECT data FROM received_faxes ORDER BY arrived DESC, rowid DESC")
    }
    pub fn received_fax(&self, id: Uuid) -> Result<crate::ReceivedFax> {
        self.read_one("SELECT data FROM received_faxes WHERE id=?1", id)
    }
    pub(crate) fn save_received(&mut self, fax: &crate::ReceivedFax) -> Result<()> {
        self.connection.execute("INSERT INTO received_faxes(id,arrived,data) VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET data=excluded.data", params![fax.id.to_string(), fax.arrived_at.to_rfc3339(), serde_json::to_string(fax)?])?;
        Ok(())
    }

    pub fn profiles(&self) -> Result<Vec<SipProfile>> {
        self.read_list("SELECT data FROM profiles ORDER BY json_extract(data, '$.name')")
    }

    pub fn profile(&self, id: Uuid) -> Result<SipProfile> {
        self.read_one("SELECT data FROM profiles WHERE id=?1", id)
    }

    pub fn save_profile(&mut self, profile: &SipProfile) -> Result<()> {
        profile.validate()?;
        if !profile.register && self.receive_settings()?.profile == Some(profile.id) {
            return Err(Error::Invalid(
                "Disable receiving before disabling registration".into(),
            ));
        }
        self.connection.execute("INSERT INTO profiles (id,data) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            params![profile.id.to_string(), serde_json::to_string(profile)?])?;
        Ok(())
    }

    pub fn destinations(&self) -> Result<Vec<Destination>> {
        self.read_list("SELECT data FROM destinations ORDER BY json_extract(data, '$.name')")
    }

    pub fn save_destination(&mut self, destination: &Destination) -> Result<()> {
        nonempty("Destination name", &destination.name)?;
        validate_destination(&destination.address)?;
        self.connection.execute(
            "INSERT INTO destinations (id,profile_id,data) VALUES (?1,?2,?3)
            ON CONFLICT(id) DO UPDATE SET profile_id=excluded.profile_id,data=excluded.data",
            params![
                destination.id.to_string(),
                destination.profile_id.to_string(),
                serde_json::to_string(destination)?
            ],
        )?;
        Ok(())
    }

    pub fn jobs(&self) -> Result<Vec<Job>> {
        self.read_list("SELECT data FROM jobs ORDER BY created DESC, rowid DESC")
    }

    pub(crate) fn clear_queue(&mut self) -> Result<usize> {
        // Keep the active row until the worker records its cancellation.
        Ok(self
            .connection
            .execute("DELETE FROM jobs WHERE status != 'active'", [])?)
    }

    pub(crate) fn remove_job(&mut self, id: Uuid) -> Result<()> {
        self.connection
            .execute("DELETE FROM jobs WHERE id=?1", [id.to_string()])?;
        Ok(())
    }

    pub fn job(&self, id: Uuid) -> Result<Job> {
        self.read_one("SELECT data FROM jobs WHERE id=?1", id)
    }

    pub(crate) fn enqueue(&mut self, request: FaxRequest, retry_of: Option<Uuid>) -> Result<Job> {
        validate_destination(&request.destination)?;
        let profile = self.profile(request.profile_id)?;
        profile.validate()?;
        let now = Utc::now();
        let job = Job {
            id: Uuid::new_v4(),
            created_at: now,
            updated_at: now,
            request,
            profile,
            state: JobState::Queued,
            retry_of,
            steps: Vec::new(),
            transmitted_bytes: 0,
            page_progress: None,
        };
        self.connection.execute(
            "INSERT INTO jobs (id,created,status,data) VALUES (?1,?2,'queued',?3)",
            params![
                job.id.to_string(),
                job.created_at.to_rfc3339(),
                serde_json::to_string(&job)?
            ],
        )?;
        Ok(job)
    }

    pub(crate) fn claim_next(&mut self) -> Result<Option<Job>> {
        let transaction = self.connection.transaction()?;
        let data: Option<String> = transaction
            .query_row(
                "SELECT data FROM jobs WHERE status='queued' ORDER BY created, rowid LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let mut job: Job = match data {
            Some(data) => serde_json::from_str(&data)?,
            None => return Ok(None),
        };
        job.state = JobState::Connecting;
        job.updated_at = Utc::now();
        transaction.execute(
            "UPDATE jobs SET status='active',data=?2 WHERE id=?1",
            params![job.id.to_string(), serde_json::to_string(&job)?],
        )?;
        transaction.commit()?;
        Ok(Some(job))
    }

    pub(crate) fn update_job(&mut self, job: &mut Job) -> Result<()> {
        job.updated_at = Utc::now();
        let status = match job.state {
            JobState::Queued => "queued",
            ref state if state.is_active() => "active",
            _ => "finished",
        };
        let updated = self.connection.execute(
            "UPDATE jobs SET status=?2,data=?3 WHERE id=?1",
            params![job.id.to_string(), status, serde_json::to_string(job)?],
        )?;
        match updated {
            1 => Ok(()),
            _ => Err(Error::NotFound(job.id.to_string())),
        }
    }

    fn read_one<T: serde::de::DeserializeOwned>(&self, sql: &str, id: Uuid) -> Result<T> {
        let data: Option<String> = self
            .connection
            .query_row(sql, [id.to_string()], |row| row.get(0))
            .optional()?;
        serde_json::from_str(&data.ok_or_else(|| Error::NotFound(id.to_string()))?)
            .map_err(Into::into)
    }

    fn read_list<T: serde::de::DeserializeOwned>(&self, sql: &str) -> Result<Vec<T>> {
        self.connection
            .prepare(sql)?
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect()
    }
}
