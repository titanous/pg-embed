//!
//! Postgresql server
//!
//! Start, stop, initialize the postgresql server.
//! Create database clusters and databases.
//!
use futures::{TryFutureExt};
use std::process::{Command, Stdio, ExitStatus};
use crate::pg_fetch;
use crate::errors::errors_common::PgEmbedError;
#[cfg(any(feature = "rt_tokio", feature = "rt_tokio_migrate"))]
use tokio::io::AsyncWriteExt;
#[cfg(feature = "rt_tokio_migrate")]
use sqlx_tokio::Postgres;
#[cfg(feature = "rt_tokio_migrate")]
use sqlx_tokio::postgres::PgPoolOptions;
use std::time::Duration;
#[cfg(feature = "rt_tokio_migrate")]
use sqlx_tokio::migrate::{Migrator, MigrateDatabase};
use tokio::time::timeout;
use std::path::PathBuf;
use std::io;
use io::{Error, ErrorKind};
use log::{info, error};
use crate::pg_access::PgAccess;
use tokio::time::error::Elapsed;
use tokio::io::{BufReader, AsyncBufReadExt};
use tokio::process::Child;

// these cfg feature settings for PgEmbedError are really convoluted, but getting syntax errors otherwise
#[cfg(not(any(feature = "rt_tokio_migrate", feature = "rt_async_std", feature = "rt_async_std_migrate", feature = "rt_actix", feature = "rt_actix_migrate")))]
use crate::errors::errors_tokio::PgEmbedErrorExt;
#[cfg(feature = "rt_tokio_migrate")]
use crate::errors::errors_tokio_migrate::PgEmbedErrorExt;
#[cfg(not(any(feature = "rt_tokio", feature = "rt_tokio_migrate", feature = "rt_async_std_migrate", feature = "rt_actix", feature = "rt_actix_migrate")))]
use crate::errors::errors_async_std::PgEmbedErrorExt;
#[cfg(not(any(feature = "rt_tokio", feature = "rt_tokio_migrate", feature = "rt_async_std", feature = "rt_actix", feature = "rt_actix_migrate")))]
use crate::errors::errors_async_std_migrate::PgEmbedErrorExt;
#[cfg(not(any(feature = "rt_tokio", feature = "rt_tokio_migrate", feature = "rt_async_std", feature = "rt_async_std_migrate", feature = "rt_actix_migrate")))]
use crate::errors::errors_actix::PgEmbedErrorExt;
#[cfg(not(any(feature = "rt_tokio", feature = "rt_tokio_migrate", feature = "rt_async_std", feature = "rt_async_std_migrate", feature = "rt_actix")))]
use crate::errors::errors_actix_migrate::PgEmbedErrorExt;

///
/// Database settings
///
pub struct PgSettings {
    /// postgresql database directory
    pub database_dir: PathBuf,
    /// postgresql port
    pub port: i16,
    /// postgresql user name
    pub user: String,
    /// postgresql password
    pub password: String,
    /// authentication
    pub auth_method: PgAuthMethod,
    /// persist database
    pub persistent: bool,
    /// duration to wait before terminating process execution
    /// pg_ctl start/stop and initdb timeout
    pub timeout: Duration,
    /// migrations folder
    /// sql script files to execute on migrate
    pub migration_dir: Option<PathBuf>,
}

///
/// Postgresql authentication method
///
/// Choose between plain password, md5 or scram_sha_256 authentication.
/// Scram_sha_256 authentication is only available on postgresql versions >= 11
///
pub enum PgAuthMethod {
    /// plain-text
    Plain,
    /// md5
    MD5,
    /// scram_sha_256
    ScramSha256,
}

///
/// Postgresql server status
///
#[derive(PartialEq)]
pub enum PgServerStatus {
    /// Postgres uninitialized
    Uninitialized,
    /// Initialization process running
    Initializing,
    /// Initialization process finished
    Initialized,
    /// Postgres server process starting
    Starting,
    /// Postgres server process started
    Started,
    /// Postgres server process stopping
    Stopping,
    /// Postgres server process stopped
    Stopped,
    /// Postgres failure
    Failure,
}

///
/// Postgesql process type
///
/// Used internally for distinguishing processes being executed
///
enum PgProcessType {
    /// initdb process
    InitDb,
    /// pg_ctl start process
    StartDb,
    /// pg_ctl stop process
    StopDb,
}

impl ToString for PgProcessType {
    fn to_string(&self) -> String {
        match self {
            PgProcessType::InitDb => { "initdb".to_string() }
            PgProcessType::StartDb => { "start".to_string() }
            PgProcessType::StopDb => { "stop".to_string() }
        }
    }
}

///
/// Embedded postgresql database
///
/// If the PgEmbed instance is dropped / goes out of scope and postgresql is still
/// running, the postgresql process will be killed and depending on the [PgSettings::persistent] setting,
/// file and directories will be cleaned up.
///
pub struct PgEmbed {
    /// Postgresql settings
    pub pg_settings: PgSettings,
    /// Download settings
    pub fetch_settings: pg_fetch::PgFetchSettings,
    /// Database uri `postgres://{username}:{password}@localhost:{port}`
    pub db_uri: String,
    /// Postgres server status
    pub server_status: PgServerStatus,
    /// Postgres files access
    pub pg_access: PgAccess,
}

impl Drop for PgEmbed {
    fn drop(&mut self) {
        if self.server_status != PgServerStatus::Stopped {
            let _ = &self.stop_db();
        }
        if !&self.pg_settings.persistent {
            let _ = &self.pg_access.clean();
        }
    }
}

impl PgEmbed {
    ///
    /// Create a new PgEmbed instance
    ///
    pub async fn new(pg_settings: PgSettings, fetch_settings: pg_fetch::PgFetchSettings) -> Result<Self, PgEmbedError> {
        let password: &str = &pg_settings.password;
        let db_uri = format!(
            "postgres://{}:{}@localhost:{}",
            &pg_settings.user,
            &password,
            &pg_settings.port
        );
        let pg_access = PgAccess::new(&fetch_settings, &pg_settings.database_dir).await?;
        Ok(
            PgEmbed {
                pg_settings,
                fetch_settings,
                db_uri,
                server_status: PgServerStatus::Uninitialized,
                pg_access,
            }
        )
    }

    ///
    /// Setup postgresql for execution
    ///
    /// Download, unpack, create password file and database
    ///
    pub async fn setup(&mut self) -> Result<(), PgEmbedError> {
        &self.aquire_postgres().await?;
        self.pg_access.create_password_file(self.pg_settings.password.as_bytes()).await?;
        &self.init_db().await?;
        Ok(())
    }

    ///
    /// Download and unpack postgres binaries
    ///
    pub async fn aquire_postgres(&self) -> Result<(), PgEmbedError> {
        let pg_bin_data = pg_fetch::fetch_postgres(&self.fetch_settings).await?;
        self.pg_access.write_pg_zip(&pg_bin_data).await?;
        pg_fetch::unpack_postgres(&self.pg_access.zip_file_path, &self.pg_access.cache_dir).await
    }

    ///
    /// Initialize postgresql database
    ///
    /// Returns `Ok(())` on success, otherwise returns an error.
    ///
    pub async fn init_db(&mut self) -> Result<(), PgEmbedError> {
        self.server_status = PgServerStatus::Initializing;
        let mut init_db_command = self.pg_access.init_db_command(&self.pg_settings.database_dir, &self.pg_settings.user, &self.pg_settings.auth_method);
        let mut process = init_db_command.get_mut()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| PgEmbedError::PgInitFailure(e))?;

        self.handle_process_io(&mut process).await;

        self.timeout_pg_process(&mut process, PgProcessType::InitDb).await
    }

    ///
    /// Start postgresql database
    ///
    /// Returns `Ok(())` on success, otherwise returns an error.
    ///
    pub async fn start_db(&mut self) -> Result<(), PgEmbedError> {
        self.server_status = PgServerStatus::Starting;
        let mut start_db_command = self.pg_access.start_db_command(&self.pg_settings.database_dir, self.pg_settings.port);

        // TODO: somehow the standard output of this command can not be piped, if piped it does not terminate. Find a solution!
        let mut process = start_db_command.get_mut()
            // .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| PgEmbedError::PgStartFailure(e))?;

        self.handle_process_io(&mut process).await;

        self.timeout_pg_process(&mut process, PgProcessType::StartDb).await
    }

    ///
    /// Stop postgresql database
    ///
    /// Returns `Ok(())` on success, otherwise returns an error.
    ///
    pub async fn stop_db(&mut self) -> Result<(), PgEmbedError> {
        self.server_status = PgServerStatus::Stopping;
        let mut stop_db_command = self.pg_access.stop_db_command(&self.pg_settings.database_dir);
        let mut process = stop_db_command.get_mut()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| PgEmbedError::PgStopFailure(e))?;

        self.handle_process_io(&mut process).await;

        self.timeout_pg_process(&mut process, PgProcessType::StopDb).await
    }

    ///
    /// Execute postgresql process with timeout
    ///
    async fn timeout_pg_process(&mut self, process: &mut Child, process_type: PgProcessType) -> Result<(), PgEmbedError> {
        let timed_exit_status: Result<io::Result<ExitStatus>, Elapsed> = timeout(self.pg_settings.timeout, process.wait()).await;
        match timed_exit_status {
            Ok(exit_result) => {
                match exit_result {
                    Ok(exit_status) => {
                        if exit_status.success() {
                            match process_type {
                                PgProcessType::InitDb => {
                                    self.server_status = PgServerStatus::Initialized;
                                }
                                PgProcessType::StartDb => {
                                    self.server_status = PgServerStatus::Started;
                                }
                                PgProcessType::StopDb => {
                                    self.server_status = PgServerStatus::Stopped;
                                }
                            }
                            Ok(())
                        } else {
                            self.server_status = PgServerStatus::Failure;
                            Err(PgEmbedError::PgStartFailure(Error::new(ErrorKind::Other, format!("Postgresql {} command failed with {}", process_type.to_string(), exit_status))))
                        }
                    }
                    Err(err) => {
                        self.server_status = PgServerStatus::Failure;
                        Err(PgEmbedError::PgStartFailure(Error::new(ErrorKind::Other, format!("Postgresql {} command failed with {}", process_type.to_string(), err.to_string()))))
                    }
                }
            }
            Err(_) => {
                self.server_status = PgServerStatus::Failure;
                Err(PgEmbedError::PgStopFailure(Error::new(ErrorKind::TimedOut, format!("Postgresql {} command timed out", process_type.to_string()))))
            }
        }
    }

    ///
    /// Handle process logging
    ///
    pub async fn handle_process_io(&self, process: &mut Child) -> Result<(), PgEmbedError> {
        let stdout = process.stdout.take().expect("child process did not have a handle to stdout");
        let stderr = process.stderr.take().expect("child process did not have a handle to stderr");

        let mut reader_out = BufReader::new(stdout).lines();
        let mut reader_err = BufReader::new(stderr).lines();

        while let Some(line) = reader_out.next_line().map_err(|e| PgEmbedError::PgBufferReadError(e)).await? {
            println!("#### out :::  {}", line);
        }

        while let Some(line) = reader_err.next_line().map_err(|e| PgEmbedError::PgBufferReadError(e)).await? {
            println!("#### err :::  {}", line);
        }
        Ok(())
    }

    ///
    /// Create a database
    ///
    #[cfg(any(feature = "rt_tokio_migrate", feature = "rt_async_std_migrate", feature = "rt_actix_migrate"))]
    pub async fn create_database(&self, db_name: &str) -> Result<(), PgEmbedErrorExt> {
        Postgres::create_database(&self.full_db_uri(db_name)).await?;
        Ok(())
    }

    ///
    /// Drop a database
    ///
    #[cfg(any(feature = "rt_tokio_migrate", feature = "rt_async_std_migrate", feature = "rt_actix_migrate"))]
    pub async fn drop_database(&self, db_name: &str) -> Result<(), PgEmbedErrorExt> {
        Postgres::drop_database(&self.full_db_uri(db_name)).await?;
        Ok(())
    }

    ///
    /// Check database existance
    ///
    #[cfg(any(feature = "rt_tokio_migrate", feature = "rt_async_std_migrate", feature = "rt_actix_migrate"))]
    pub async fn database_exists(&self, db_name: &str) -> Result<bool, PgEmbedErrorExt> {
        let result = Postgres::database_exists(&self.full_db_uri(db_name)).await?;
        Ok(result)
    }

    ///
    /// The full database uri
    ///
    /// (*postgres://{username}:{password}@localhost:{port}/{db_name}*)
    ///
    pub fn full_db_uri(&self, db_name: &str) -> String {
        format!("{}/{}", &self.db_uri, db_name)
    }

    ///
    /// Run migrations
    ///
    #[cfg(any(feature = "rt_tokio_migrate", feature = "rt_async_std_migrate", feature = "rt_actix_migrate"))]
    pub async fn migrate(&self, db_name: &str) -> Result<(), PgEmbedErrorExt> {
        if let Some(migration_dir) = &self.pg_settings.migration_dir {
            let m = Migrator::new(migration_dir.as_path()).await?;
            let pool = PgPoolOptions::new().connect(&self.full_db_uri(db_name)).await?;
            m.run(&pool).await?;
        }
        Ok(())
    }
}