//!
//! Cache postgresql files, access to executables, clean up files
//!

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::TryFutureExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};

use crate::pg_enums::{OperationSystem, PgAcquisitionStatus};
use crate::pg_errors::{PgEmbedError, PgEmbedErrorType};
use crate::pg_fetch::PgFetchSettings;
use crate::pg_types::{PgCommandSync, PgResult};

lazy_static! {
    ///
    /// Stores the paths to the cache directories while acquiring the related postgres binaries
    ///
    /// Used to prevent simultaneous downloads and unpacking of the same binaries
    /// while executing multiple PgEmbed instances concurrently.
    ///
    static ref ACQUIRED_PG_BINS: Arc<Mutex<HashMap<PathBuf, PgAcquisitionStatus>>> =
    Arc::new(Mutex::new(HashMap::with_capacity(5)));
}

const PG_EMBED_CACHE_DIR_NAME: &'static str = "pg-embed";
const PG_VERSION_FILE_NAME: &'static str = "PG_VERSION";

///
/// Access to pg_ctl, initdb, database directory and cache directory
///
pub struct PgAccess {
    /// Cache directory path
    pub cache_dir: PathBuf,
    /// Database directory path
    pub database_dir: PathBuf,
    /// Postgresql pg_ctl executable path
    pub pg_ctl_exe: PathBuf,
    /// Postgresql initdb executable path
    pub init_db_exe: PathBuf,
    /// Password file path
    pub pw_file_path: PathBuf,
    /// Postgresql binaries zip file path
    pub zip_file_path: PathBuf,
    /// Postgresql database version file
    /// used for internal checks
    pg_version_file: PathBuf,
}

impl PgAccess {
    ///
    /// Create a new instance
    ///
    /// Directory structure for cached postgresql binaries will be created
    ///
    pub async fn new(
        fetch_settings: &PgFetchSettings,
        database_dir: &PathBuf,
        cache_dir: Option<&PathBuf>,
    ) -> Result<Self, PgEmbedError> {
        let cache_dir = match cache_dir {
            Some(d) => d.clone(),
            None => Self::create_cache_dir_structure(&fetch_settings).await?,
        };

        Self::create_db_dir_structure(database_dir).await?;
        // pg_ctl executable
        let mut pg_ctl = cache_dir.clone();
        pg_ctl.push("bin/pg_ctl");
        // initdb executable
        let mut init_db = cache_dir.clone();
        init_db.push("bin/initdb");
        // postgres zip file
        let mut zip_file_path = cache_dir.clone();
        let platform = fetch_settings.platform();
        let file_name = format!("{}-{}.zip", platform, &fetch_settings.version.0);
        zip_file_path.push(file_name);
        // password file
        let mut pw_file = database_dir.clone();
        pw_file.set_extension("pwfile");
        // postgres version file
        let mut pg_version_file = database_dir.clone();
        pg_version_file.push(PG_VERSION_FILE_NAME);

        Ok(PgAccess {
            cache_dir,
            database_dir: database_dir.clone(),
            pg_ctl_exe: pg_ctl,
            init_db_exe: init_db,
            pw_file_path: pw_file,
            zip_file_path,
            pg_version_file,
        })
    }

    ///
    /// Create directory structure for cached postgresql executables
    ///
    /// Returns PathBuf(cache_directory) on success, an error otherwise
    ///
    async fn create_cache_dir_structure(fetch_settings: &PgFetchSettings) -> PgResult<PathBuf> {
        let cache_dir = dirs::cache_dir().ok_or_else(|| PgEmbedError {
            error_type: PgEmbedErrorType::InvalidPgUrl,
            source: None,
            message: None,
        })?;
        let os_string = match fetch_settings.operating_system {
            OperationSystem::Darwin | OperationSystem::Windows | OperationSystem::Linux => {
                fetch_settings.operating_system.to_string()
            }
            OperationSystem::AlpineLinux => {
                format!("arch_{}", fetch_settings.operating_system.to_string())
            }
        };
        let pg_path = format!(
            "{}/{}/{}/{}",
            PG_EMBED_CACHE_DIR_NAME,
            os_string,
            fetch_settings.architecture.to_string(),
            fetch_settings.version.0
        );
        let mut cache_pg_embed = cache_dir.clone();
        cache_pg_embed.push(pg_path);
        tokio::fs::create_dir_all(&cache_pg_embed)
            .map_err(|e| PgEmbedError {
                error_type: PgEmbedErrorType::DirCreationError,
                source: Some(Box::new(e)),
                message: None,
            })
            .await?;
        Ok(cache_pg_embed)
    }

    async fn create_db_dir_structure(db_dir: &PathBuf) -> PgResult<()> {
        tokio::fs::create_dir_all(db_dir)
            .map_err(|e| PgEmbedError {
                error_type: PgEmbedErrorType::DirCreationError,
                source: Some(Box::new(e)),
                message: None,
            })
            .await
    }

    ///
    /// Check if postgresql executables are already cached
    ///
    pub async fn pg_executables_cached(&self) -> PgResult<bool> {
        Self::path_exists(self.init_db_exe.as_path()).await
    }

    ///
    /// Check if database files exist
    ///
    pub async fn db_files_exist(&self) -> PgResult<bool> {
        Self::path_exists(self.pg_version_file.as_path()).await
    }

    ///
    /// Check if database version file exists
    ///
    pub async fn pg_version_file_exists(db_dir: &PathBuf) -> PgResult<bool> {
        let mut pg_version_file = db_dir.clone();
        pg_version_file.push(PG_VERSION_FILE_NAME);
        let file_exists = if let Ok(_) = tokio::fs::File::open(pg_version_file.as_path()).await {
            true
        } else {
            false
        };
        Ok(file_exists)
    }

    ///
    /// Check if file path exists
    ///
    async fn path_exists(file: &Path) -> PgResult<bool> {
        if let Ok(_) = tokio::fs::File::open(file).await {
            Ok(true)
        } else {
            Ok(false)
        }
    }

    ///
    /// Mark postgresql binaries acquisition in progress
    ///
    /// Used while acquiring postgresql binaries, so that no two instances
    /// of PgEmbed try to acquire the same resources
    ///
    pub async fn mark_acquisition_in_progress(&self) -> PgResult<()> {
        let mut lock = ACQUIRED_PG_BINS.lock().await;
        lock.insert(self.cache_dir.clone(), PgAcquisitionStatus::InProgress);
        Ok(())
    }

    ///
    /// Mark postgresql binaries acquisition finished
    ///
    /// Used when acquiring postgresql has finished, so that other instances
    /// of PgEmbed don't try to reacquire resources
    ///
    pub async fn mark_acquisition_finished(&self) -> PgResult<()> {
        let mut lock = ACQUIRED_PG_BINS.lock().await;
        lock.insert(self.cache_dir.clone(), PgAcquisitionStatus::Finished);
        Ok(())
    }

    ///
    /// Check postgresql acquisition status
    ///
    pub async fn acquisition_status(&self) -> PgAcquisitionStatus {
        let lock = ACQUIRED_PG_BINS.lock().await;
        let acquisition_status = lock.get(&self.cache_dir);
        match acquisition_status {
            None => PgAcquisitionStatus::Undefined,
            Some(status) => *status,
        }
    }

    ///
    /// Determine if postgresql binaries acquisition is needed
    ///
    pub async fn acquisition_needed(&self) -> PgResult<bool> {
        if !self.pg_executables_cached().await? {
            match self.acquisition_status().await {
                PgAcquisitionStatus::InProgress => {
                    let mut interval = interval(Duration::from_millis(100));
                    while self.acquisition_status().await == PgAcquisitionStatus::InProgress {
                        interval.tick().await;
                    }
                    Ok(false)
                }
                PgAcquisitionStatus::Finished => Ok(false),
                PgAcquisitionStatus::Undefined => Ok(true),
            }
        } else {
            Ok(false)
        }
    }

    ///
    /// Write pg binaries zip to postgresql cache directory
    ///
    pub async fn write_pg_zip(&self, bytes: &[u8]) -> PgResult<()> {
        let mut file: tokio::fs::File = tokio::fs::File::create(&self.zip_file_path.as_path())
            .map_err(|e| PgEmbedError {
                error_type: PgEmbedErrorType::WriteFileError,
                source: Some(Box::new(e)),
                message: None,
            })
            .await?;
        file.write_all(&bytes)
            .map_err(|e| PgEmbedError {
                error_type: PgEmbedErrorType::WriteFileError,
                source: Some(Box::new(e)),
                message: None,
            })
            .await?;
        Ok(())
    }

    ///
    /// Clean up created files and directories.
    ///
    /// Remove created directories containing the database and the password file.
    ///
    pub fn clean(&self) -> PgResult<()> {
        // not using tokio::fs async methods because clean() is called on drop
        std::fs::remove_dir_all(self.database_dir.as_path()).map_err(|e| PgEmbedError {
            error_type: PgEmbedErrorType::PgCleanUpFailure,
            source: Some(Box::new(e)),
            message: None,
        })?;
        std::fs::remove_file(self.pw_file_path.as_path()).map_err(|e| PgEmbedError {
            error_type: PgEmbedErrorType::PgCleanUpFailure,
            source: Some(Box::new(e)),
            message: None,
        })?;
        Ok(())
    }

    ///
    /// Purge postgresql executables
    ///
    /// Remove all cached postgresql executables
    ///
    pub async fn purge() -> PgResult<()> {
        let mut cache_dir = dirs::cache_dir().ok_or_else(|| PgEmbedError {
            error_type: PgEmbedErrorType::ReadFileError,
            source: None,
            message: Some(String::from("cache dir error")),
        })?;
        cache_dir.push(PG_EMBED_CACHE_DIR_NAME);
        let _ = tokio::fs::remove_dir_all(cache_dir.as_path())
            .map_err(|e| PgEmbedError {
                error_type: PgEmbedErrorType::PgPurgeFailure,
                source: Some(Box::new(e)),
                message: None,
            })
            .await;
        Ok(())
    }

    ///
    /// Clean up database directory and password file
    ///
    pub async fn clean_up(database_dir: PathBuf, pw_file: PathBuf) -> PgResult<()> {
        tokio::fs::remove_dir_all(database_dir.as_path())
            .await
            .map_err(|e| PgEmbedError {
                error_type: PgEmbedErrorType::PgCleanUpFailure,
                source: Some(Box::new(e)),
                message: None,
            })?;

        tokio::fs::remove_file(pw_file.as_path())
            .await
            .map_err(|e| PgEmbedError {
                error_type: PgEmbedErrorType::PgCleanUpFailure,
                source: Some(Box::new(e)),
                message: None,
            })
    }

    ///
    /// Create a database password file
    ///
    /// Returns `Ok(())` on success, otherwise returns an error.
    ///
    pub async fn create_password_file(&self, password: &[u8]) -> PgResult<()> {
        let mut file: tokio::fs::File = tokio::fs::File::create(self.pw_file_path.as_path())
            .map_err(|e| PgEmbedError {
                error_type: PgEmbedErrorType::WriteFileError,
                source: Some(Box::new(e)),
                message: None,
            })
            .await?;
        let _ = file
            .write(password)
            .map_err(|e| PgEmbedError {
                error_type: PgEmbedErrorType::WriteFileError,
                source: Some(Box::new(e)),
                message: None,
            })
            .await?;
        Ok(())
    }

    ///
    /// Create synchronous pg_ctl stop command
    ///
    pub fn stop_db_command_sync(&self, database_dir: &PathBuf) -> PgCommandSync {
        let pg_ctl_executable = self.pg_ctl_exe.to_str().unwrap();
        let mut command = Box::new(Cell::new(std::process::Command::new(pg_ctl_executable)));
        command
            .get_mut()
            .args(&["stop", "-w", "-D", database_dir.to_str().unwrap()]);
        command
    }
}
