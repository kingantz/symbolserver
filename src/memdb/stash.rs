//! Provide access to locally cached memdb SDKs
//!
//! The `MemDbStash` pulls in remote SDKs from an S3 bucket and provides
//! access to it.  This is used by the symbol server to manage the local
//! cache and also to refer to memdb files that are mmap'ed in.
use std::fs;
use std::io;
use std::iter::FromIterator;
use std::path::{Path, PathBuf};
use std::collections::{HashMap, HashSet};
use std::collections::hash_map::Values as HashMapValuesIter;
use std::sync::{Arc, RwLock};

use serde_json;
use xz2::write::XzDecoder;
use chrono::Utc;
use console::style;
use indicatif::{ProgressBar, ProgressStyle};

use super::read::MemDb;
use super::super::config::Config;
use super::super::sdk::SdkInfo;
use super::super::s3::S3Server as S3;
use super::super::utils::{copy_with_progress, HumanDuration, IgnorePatterns, Rev};
use super::super::{Result, ResultExt, ErrorKind};

/// Helper for synching
pub struct SyncOptions {
    pub user_facing: bool,
}

/// The main memdb stash type
pub struct MemDbStash {
    path: PathBuf,
    s3: S3,
    local_state: RwLock<Option<Arc<SdkSyncState>>>,
    memdbs: RwLock<HashMap<SdkInfo, Arc<MemDb<'static>>>>,
    ignore_patterns: IgnorePatterns,
}

/// Information about a remotely available SDK
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RemoteSdk {
    filename: String,
    info: SdkInfo,
    size: u64,
    etag: String,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
struct SdkSyncState {
    sdks: HashMap<String, RemoteSdk>,
    revision: Option<u64>,
}

/// Information about the health of the stash sync
#[derive(Debug)]
pub struct SyncStatus {
    remote_total: u32,
    missing: u32,
    different: u32,
    revision: u64,
    offline: bool,
}

impl RemoteSdk {
    /// Creates a remote SDK object from some information
    pub fn new(filename: String, info: SdkInfo, etag: String, size: u64) -> RemoteSdk {
        RemoteSdk {
            filename: filename,
            info: info,
            etag: etag,
            size: size,
        }
    }

    /// The remotely visible filename for the SDK
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// The size of the SDK in bytes
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns the SDK info
    pub fn info(&self) -> &SdkInfo {
        &self.info
    }
}

/// Iterator over the SDKs
pub type RemoteSdkIter<'a> = HashMapValuesIter<'a, String, RemoteSdk>;

impl SdkSyncState {
    pub fn get_sdk(&self, info: &SdkInfo) -> Option<&RemoteSdk> {
        self.sdks.get(&info.memdb_filename())
    }

    pub fn update_sdk(&mut self, sdk: &RemoteSdk) {
        self.sdks.insert(sdk.info().memdb_filename().to_string(), sdk.clone());
    }

    pub fn remove_sdk(&mut self, info: &SdkInfo) {
        self.sdks.remove(&info.memdb_filename());
    }

    pub fn sdks<'a>(&'a self) -> RemoteSdkIter<'a> {
        self.sdks.values()
    }

    pub fn sdk_count(&self) -> usize {
        self.sdks.len()
    }
}

impl SyncStatus {

    /// Indicates that the server is running offline (no S3 access)
    pub fn is_offline(&self) -> bool {
        self.offline
    }

    /// Returns the lag (number of SDKs behind upstream)
    pub fn lag(&self) -> u32 {
        self.missing + self.different
    }

    /// Returns true if the local sync is still considered healthy
    pub fn is_healthy(&self) -> bool {
        if self.offline {
            true
        } else {
            let total = self.remote_total as f32;
            let lag = self.lag() as f32;
            lag / total < 0.10
        }
    }

    /// Returns the revision of the stash
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

impl Default for SyncOptions {
    fn default() -> SyncOptions {
        SyncOptions {
            user_facing: false,
        }
    }
}

impl MemDbStash {
    /// Opens a stash for a given config.
    pub fn new(config: &Config) -> Result<MemDbStash> {
        Ok(MemDbStash {
            path: config.get_symbol_dir()?.to_path_buf(),
            s3: S3::from_config(config)?,
            local_state: RwLock::new(None),
            memdbs: RwLock::new(HashMap::new()),
            ignore_patterns: config.get_ignore_patterns()?.clone(),
        })
    }

    fn get_local_sync_state_filename(&self) -> PathBuf {
        self.path.join("sync.state")
    }

    fn save_state(&self, new_state: &SdkSyncState, filename: &Path) -> Result<()> {
        let mut tmp_filename = filename.to_path_buf();
        tmp_filename.set_extension("tempstate");
        {
            let mut f = fs::File::create(&tmp_filename)?;
            serde_json::to_writer(&mut f, new_state)
                .chain_err(|| "Could not update sync state")?;
        }
        fs::rename(&tmp_filename, &filename)?;
        Ok(())
    }

    fn read_local_state(&self) -> Result<SdkSyncState> {
        let rv: SdkSyncState = match fs::File::open(&self.get_local_sync_state_filename()) {
            Ok(f) => serde_json::from_reader(io::BufReader::new(f))
                .chain_err(|| "Parsing error on loading sync state")?,
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound {
                    Default::default()
                } else {
                    return Err(err).chain_err(|| "Error loading sync state");
                }
            }
        };
        let mut opt = self.local_state.write().unwrap();
        *opt = Some(Arc::new(rv.clone()));
        Ok(rv)
    }

    fn get_local_state(&self) -> Result<Arc<SdkSyncState>> {
        if let Some(ref arc) = *self.local_state.read().unwrap() {
            return Ok(arc.clone());
        }
        self.read_local_state()?;
        Ok(self.local_state.read().unwrap().as_ref().unwrap().clone())
    }

    fn save_local_state(&self, new_state: &SdkSyncState) -> Result<()> {
        self.save_state(new_state, &self.get_local_sync_state_filename())?;
        let mut opt = self.local_state.write().unwrap();
        *opt = Some(Arc::new(new_state.clone()));
        Ok(())
    }

    fn fetch_remote_state(&self) -> Result<SdkSyncState> {
        let mut sdks = HashMap::new();
        for remote_sdk in self.s3.list_upstream_sdks()? {
            sdks.insert(remote_sdk.info().memdb_filename().into(), remote_sdk);
        }
        Ok(SdkSyncState { sdks: sdks, revision: None })
    }

    fn update_sdk(&self, sdk: &RemoteSdk, options: &SyncOptions) -> Result<()> {
        // XXX: the progress bar here can stall out because we currently
        // need to buffer the download into memory in the s3 code :(
        let progress = if options.user_facing {
            ProgressBar::new(sdk.size())
        } else {
            info!("updating {}", sdk.info());
            ProgressBar::hidden()
        };
        progress.set_style(ProgressStyle::default_bar()
            .template("{wide_bar} {bytes}/{total_bytes}"));
        let started = Utc::now();
        println!("{} {}", style("Updating").green(), sdk.info());
        let mut src = self.s3.download_sdk(sdk)?;
        let dst = fs::File::create(self.path.join(sdk.info().memdb_filename()))?;
        let mut dst = XzDecoder::new(dst);
        copy_with_progress(&progress, &mut src, &mut dst)?;
        progress.finish_and_clear();

        let duration = Utc::now() - started;
        if !options.user_facing {
            info!("updated {} in {}", sdk.info(), HumanDuration(duration));
        }
        Ok(())
    }

    fn remove_sdk(&self, sdk: &RemoteSdk, options: &SyncOptions) -> Result<()> {
        if options.user_facing {
            info!("{} {}", style("Deleting").red(), sdk.info());
        } else {
            info!("removing {}", sdk.info());
        }
        if let Err(err) = fs::remove_file(self.path.join(sdk.info().memdb_filename())) {
            if err.kind() != io::ErrorKind::NotFound {
                return Err(err.into());
            }
        }
        Ok(())
    }

    /// Returns the current revision
    pub fn get_revision(&self) -> Result<u64> {
        Ok(self.read_local_state()?.revision.unwrap_or(0))
    }

    /// Returns the number of local SDKs
    pub fn sdk_count(&self) -> Result<usize> {
        Ok(self.get_local_state()?.sdk_count())
    }

    /// Returns a list of all synched SDKs
    pub fn list_sdks(&self) -> Result<Vec<SdkInfo>> {
        let local_state = self.get_local_state()?;
        let mut rv : Vec<_> = local_state.sdks().map(|sdk| {
            sdk.info().clone()
        }).collect();
        rv.sort();
        Ok(rv)
    }

    /// Checks the local stash against the server
    pub fn get_sync_status(&self) -> Result<SyncStatus> {
        let local_state = self.read_local_state()?;

        let mut remote_total = 0;
        let mut missing = 0;
        let mut different = 0;
        let mut offline = false;

        match self.fetch_remote_state() {
            Ok(remote_state) => {
                for sdk in remote_state.sdks() {
                    if self.sdk_is_ignored(sdk.info()) {
                        continue;
                    }
                    if let Some(local_sdk) = local_state.get_sdk(sdk.info()) {
                        if local_sdk != sdk {
                            different += 1;
                        }
                    } else {
                        missing += 1;
                    }
                    remote_total += 1;
                }
            }
            Err(err) => {
                if let &ErrorKind::S3Unavailable(_) = err.kind() {
                    offline = true;
                } else {
                    return Err(err);
                }
            }
        }

        Ok(SyncStatus {
            remote_total: remote_total as u32,
            missing: missing as u32,
            different: different as u32,
            revision: local_state.revision.unwrap_or(0),
            offline: offline,
        })
    }

    /// Checks if the SDK is ignored by config
    pub fn sdk_is_ignored(&self, info: &SdkInfo) -> bool {
        self.ignore_patterns.is_match(&info.sdk_id())
    }

    /// Synchronize the local stash with the server
    pub fn sync(&self, options: SyncOptions) -> Result<()> {
        let mut local_state = self.read_local_state()?;
        let remote_state = self.fetch_remote_state()?;
        let started = Utc::now();
        let mut changed = false;
        let mut to_delete : HashSet<_> = HashSet::from_iter(
            local_state.sdks().map(|x| x.info().clone()));
        let mut sdks : Vec<_> = remote_state.sdks()
            .map(|x| x.info().clone()).collect();
        sdks.sort_by(|a, b| b.cmp(a));

        for sdk_info in sdks.iter() {
            if !self.sdk_is_ignored(sdk_info) {
                let mut changed_something = false;
                let sdk = remote_state.get_sdk(sdk_info).unwrap();
                if let Some(local_sdk) = local_state.get_sdk(sdk_info) {
                    if local_sdk != sdk {
                        self.update_sdk(&sdk, &options)?;
                        self.memdbs.write().unwrap().remove(sdk_info);
                        changed_something = true;
                    } else if options.user_facing {
                        println!("{} {}", style("Unchanged").cyan(), sdk_info);
                    } else {
                        debug!("unchanged sdk {}", sdk_info);
                    }
                } else {
                    self.update_sdk(&sdk, &options)?;
                    changed_something = true;
                }
                if changed_something {
                    changed = true;
                    local_state.update_sdk(&sdk);
                    local_state.revision = Some(local_state.revision.unwrap_or(0) + 1);
                    self.save_local_state(&local_state)?;
                }
            } else {
                if options.user_facing {
                    println!("{} {} by config", style("Ignored").yellow(), sdk_info);
                } else {
                    debug!("ignored sdk {} by config", sdk_info);
                }
            }

            to_delete.remove(sdk_info);
        }

        for sdk_info in to_delete.iter() {
            local_state.remove_sdk(sdk_info);
            if let Some(sdk) = local_state.get_sdk(sdk_info) {
                self.remove_sdk(sdk, &options)?;
                self.memdbs.write().unwrap().remove(&sdk.info());
            }
        }

        let duration = Utc::now() - started;
        if options.user_facing {
            println!("Sync done in {}", HumanDuration(duration));
        } else if changed {
            info!("finished sync in {}", HumanDuration(duration));
        }

        // save us one last time
        local_state.revision = Some(local_state.revision.unwrap_or(0) + 1);
        self.save_local_state(&local_state)?;

        Ok(())
    }

    /// Looks up an memdb by an SDK info if it's available.
    ///
    /// This returns a memdb wrapped in an arc as internally the system
    /// might try to unload the memdb if no longer needed.  If the MemDb
    /// does not exist, a `UnknownSdk` error is returned.
    pub fn get_memdb(&self, info: &SdkInfo) -> Result<Arc<MemDb<'static>>> {
        // try to fetch it from the local mapping.  The sync method will
        // remove it from here automatically.
        if let Some(arc) = self.memdbs.read().unwrap().get(info) {
            return Ok(arc.clone());
        }

        let local_state = self.get_local_state()?;

        // make sure we check in the local state first if the SDK exists.
        // if we go directly to the memdbs array or look at the file system
        // we might start to consider things that are not available yet or
        // not available any longer.
        if local_state.get_sdk(&info).is_some() {
            let memdb = MemDb::from_path(self.path.join(&info.memdb_filename()))?;
            self.memdbs.write().unwrap().insert(info.clone(), Arc::new(memdb));
            if let Some(arc) = self.memdbs.read().unwrap().get(info) {
                return Ok(arc.clone());
            }
        }

        Err(ErrorKind::UnknownSdk.into())
    }

    /// Looks up an memdb by an SDK info as string if available.
    pub fn get_memdb_from_sdk_id(&self, sdk_id: &str) -> Result<Arc<MemDb<'static>>> {
        if let Some(sdk_info) = SdkInfo::from_filename(sdk_id) {
            self.get_memdb(&sdk_info)
        } else {
            Err(ErrorKind::UnknownSdk.into())
        }
    }

    /// Given an SDK info this returns an array of fuzzy matches for it.
    pub fn fuzzy_match_sdk_id(&self, sdk_id: &str) -> Result<Vec<SdkInfo>> {
        let local_state = self.get_local_state()?;
        let mut rv = vec![];

        if let Some(sdk_info) = SdkInfo::from_filename(sdk_id) {
            // find all sdks that have a fuzzy match
            for other in local_state.sdks() {
                let q = other.info().get_fuzzy_match(&sdk_info).unwrap_or(99999);
                rv.push((q, other.info().clone()));
            }

            rv.sort_by_key(|&(q, ref info)| (q, info > &sdk_info, Rev(info.clone())));
        }

        Ok(rv.into_iter().take(10).map(|(_, info)| info).collect())
    }
}
