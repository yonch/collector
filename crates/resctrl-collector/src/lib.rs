use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use arrow_array::builder::{Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use log::{debug, info, warn};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use nri::metadata::{ContainerMetadata, MetadataMessage, MetadataPlugin};
use nri::NRI;
use nri_resctrl_plugin::{PodResctrlEvent, ResctrlGroupState, ResctrlPlugin, ResctrlPluginConfig};

/// Default channel capacity for communication with the plugins
const DEFAULT_CHANNEL_CAPACITY: usize = 256;

/// Create the Arrow schema for resctrl LLC occupancy samples
pub fn create_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("timestamp", DataType::Int64, false),
        Field::new("pod_namespace", DataType::Utf8, true),
        Field::new("pod_name", DataType::Utf8, true),
        Field::new("pod_uid", DataType::Utf8, true),
        Field::new("resctrl_group", DataType::Utf8, true),
        Field::new("llc_occupancy_bytes", DataType::Int64, false),
    ]))
}

/// Resctrl collector instance state
#[derive(Default)]
pub struct ResctrlCollector {
    resctrl_synced: AtomicBool,
    metadata_synced: AtomicBool,
}

impl ResctrlCollector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns true when both resctrl and metadata have produced at least
    /// one event since startup, indicating initial synchronize completed.
    pub fn ready(&self) -> bool {
        self.resctrl_synced.load(Ordering::Relaxed) && self.metadata_synced.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct PodState {
    group_path: Option<String>,
    total_containers: usize,
    reconciled_containers: usize,
}

/// Internal mutable state and handlers for the collector.
///
/// This isolates logic to make it easier to unit test individual handlers
/// without having to run the full event loop and real plugins.
struct ResctrlCollectorState {
    this: Arc<ResctrlCollector>,
    pods: HashMap<String, PodState>,               // keyed by pod_uid
    pod_labels: HashMap<String, (String, String)>, // pod_uid -> (ns, name)
    rc: resctrl::Resctrl,
    schema: SchemaRef,
    batch_sender: mpsc::Sender<RecordBatch>,
    dropped_batches: u64,
}

impl ResctrlCollectorState {
    fn new(
        this: Arc<ResctrlCollector>,
        batch_sender: mpsc::Sender<RecordBatch>,
        cfg: &ResctrlCollectorConfig,
    ) -> Self {
        let rc = resctrl::Resctrl::new(resctrl::Config {
            root: cfg.mountpoint.clone(),
            ..Default::default()
        });
        Self {
            this,
            pods: HashMap::new(),
            pod_labels: HashMap::new(),
            rc,
            schema: create_schema(),
            batch_sender,
            dropped_batches: 0,
        }
    }

    /// Handle a periodic sampling tick: read group occupancies and emit a batch.
    fn handle_sample_timer(&mut self) {
        let now_ns: i64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0) as i64;

        // Snapshot groups to sample to avoid holding borrows
        let groups_to_sample: Vec<(String, String)> = self
            .pods
            .iter()
            .filter_map(|(uid, ps)| ps.group_path.as_ref().map(|gp| (uid.clone(), gp.clone())))
            .collect();

        let rows_cap = groups_to_sample.len();
        if rows_cap > 0 {
            let mut ts_b = Int64Builder::with_capacity(rows_cap);
            let mut ns_b = StringBuilder::with_capacity(rows_cap, rows_cap * 16);
            let mut name_b = StringBuilder::with_capacity(rows_cap, rows_cap * 16);
            let mut uid_b = StringBuilder::with_capacity(rows_cap, rows_cap * 16);
            let mut grp_b = StringBuilder::with_capacity(rows_cap, rows_cap * 24);
            let mut llc_b = Int64Builder::with_capacity(rows_cap);

            let mut rows_appended = 0usize;
            for (uid, gp) in groups_to_sample {
                match self.rc.llc_occupancy_total_bytes(&gp) {
                    Ok(total) => {
                        let labels = self.pod_labels.get(&uid).cloned();
                        ts_b.append_value(now_ns);
                        if let Some((ns, name)) = labels {
                            ns_b.append_value(ns.as_str());
                            name_b.append_value(name.as_str());
                        } else {
                            ns_b.append_null();
                            name_b.append_null();
                        }
                        uid_b.append_value(uid.as_str());
                        grp_b.append_value(gp.as_str());
                        llc_b.append_value(total as i64);
                        rows_appended += 1;
                    }
                    Err(e) => {
                        debug!("resctrl read failed for {}: {}", gp, e);
                    }
                }
            }

            if rows_appended > 0 {
                let arrays: Vec<ArrayRef> = vec![
                    Arc::new(ts_b.finish()),
                    Arc::new(ns_b.finish()),
                    Arc::new(name_b.finish()),
                    Arc::new(uid_b.finish()),
                    Arc::new(grp_b.finish()),
                    Arc::new(llc_b.finish()),
                ];
                let batch = match RecordBatch::try_new(self.schema.clone(), arrays) {
                    Ok(b) => b,
                    Err(e) => {
                        debug!("failed to build occupancy batch: {}", e);
                        return;
                    }
                };
                if let Err(_e) = self.batch_sender.try_send(batch) {
                    self.dropped_batches += 1;
                }
            }
        }

        if self.dropped_batches > 0 {
            warn!(
                "Dropped {} occupancy batches in the last second",
                self.dropped_batches
            );
            self.dropped_batches = 0;
        }
    }

    /// Handle periodic health logging.
    fn handle_health_timer(&self) {
        let mut failed = 0usize;
        let mut not_reconciled = 0usize;
        for (_uid, ps) in self.pods.iter() {
            if ps.group_path.is_none() {
                failed += 1;
            }
            if ps.reconciled_containers < ps.total_containers {
                not_reconciled += 1;
            }
        }
        info!(
            "resctrl health: pods_failed={}, pods_unreconciled={}",
            failed, not_reconciled
        );
    }

    /// Handle a resctrl plugin event.
    fn handle_resctrl_event(&mut self, ev: PodResctrlEvent) {
        if !self.this.resctrl_synced.load(Ordering::Relaxed) {
            self.this.resctrl_synced.store(true, Ordering::Relaxed);
        }
        match ev {
            PodResctrlEvent::AddOrUpdate(add) => {
                let entry = self.pods.entry(add.pod_uid.clone()).or_default();
                entry.total_containers = add.total_containers;
                entry.reconciled_containers = add.reconciled_containers;
                if let ResctrlGroupState::Exists(p) = add.group_state {
                    entry.group_path = Some(p);
                } else {
                    entry.group_path = None;
                }
            }
            PodResctrlEvent::Removed(r) => {
                self.pods.remove(&r.pod_uid);
                self.pod_labels.remove(&r.pod_uid);
            }
        }
    }

    /// Handle a metadata plugin event.
    fn handle_metadata_event(&mut self, msg: MetadataMessage) {
        if !self.this.metadata_synced.load(Ordering::Relaxed) {
            self.this.metadata_synced.store(true, Ordering::Relaxed);
        }
        match msg {
            MetadataMessage::Add(_cid, boxed) => {
                let ContainerMetadata {
                    pod_uid,
                    pod_namespace,
                    pod_name,
                    ..
                } = *boxed;
                if !pod_uid.is_empty() {
                    self.pod_labels.insert(pod_uid, (pod_namespace, pod_name));
                }
            }
            MetadataMessage::Remove(_cid) => {
                // nothing to do at pod map
            }
        }
    }

    /// Handle a retry tick by invoking the plugin's retry mechanism.
    fn handle_retry_timer(&self, resctrl_plugin: &ResctrlPlugin) {
        if let Err(e) = resctrl_plugin.retry_all_once() {
            debug!("retry_all_once error: {:?}", e);
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResctrlCollectorConfig {
    /// Sampling interval
    pub sample_interval: Duration,
    /// Retry-all interval for the plugin
    pub retry_interval: Duration,
    /// Health logging interval
    pub health_interval: Duration,
    /// Output channel capacity (RecordBatches)
    pub channel_capacity: usize,
    /// resctrl mountpoint (root path)
    pub mountpoint: PathBuf,
}

impl Default for ResctrlCollectorConfig {
    fn default() -> Self {
        Self {
            sample_interval: Duration::from_secs(1),
            retry_interval: Duration::from_secs(10),
            health_interval: Duration::from_secs(60),
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            mountpoint: PathBuf::from("/sys/fs/resctrl"),
        }
    }
}

impl ResctrlCollectorConfig {
    /// Create a config from environment variables with sensible defaults.
    /// Supported variables:
    /// - `RESCTRL_SAMPLING_INTERVAL` (humantime, e.g., "1s", "500ms")
    /// - `RESCTRL_RETRY_INTERVAL` (humantime)
    /// - `RESCTRL_HEALTH_INTERVAL` (humantime)
    /// - `RESCTRL_CHANNEL_CAPACITY` (usize > 0)
    /// - `RESCTRL_MOUNT` (path)
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(s) = env::var("RESCTRL_SAMPLING_INTERVAL") {
            if let Ok(d) = humantime::parse_duration(&s) {
                cfg.sample_interval = d;
            }
        }
        if let Ok(s) = env::var("RESCTRL_RETRY_INTERVAL") {
            if let Ok(d) = humantime::parse_duration(&s) {
                cfg.retry_interval = d;
            }
        }
        if let Ok(s) = env::var("RESCTRL_HEALTH_INTERVAL") {
            if let Ok(d) = humantime::parse_duration(&s) {
                cfg.health_interval = d;
            }
        }
        if let Ok(s) = env::var("RESCTRL_CHANNEL_CAPACITY") {
            if let Ok(n) = s.parse::<usize>() {
                if n > 0 {
                    cfg.channel_capacity = n;
                }
            }
        }
        if let Ok(m) = env::var("RESCTRL_MOUNT") {
            if !m.is_empty() {
                cfg.mountpoint = PathBuf::from(m);
            }
        }
        cfg
    }
}

/// Run the resctrl collector loop.
///
/// Best-effort: if NRI runtime is not available, the task runs idle.
pub async fn run(
    this: Arc<ResctrlCollector>,
    batch_sender: mpsc::Sender<RecordBatch>,
    shutdown: CancellationToken,
    cfg: ResctrlCollectorConfig,
) -> Result<()> {
    // Outgoing channel is provided by caller; we use try_send and drop on full
    let (resctrl_tx, mut resctrl_rx) = mpsc::channel::<PodResctrlEvent>(cfg.channel_capacity);
    let (meta_tx, mut meta_rx) = mpsc::channel::<MetadataMessage>(cfg.channel_capacity);

    // Create plugins
    let resctrl_plugin = Arc::new(ResctrlPlugin::new(
        ResctrlPluginConfig::default(),
        resctrl_tx,
    ));
    let meta_plugin = Arc::new(MetadataPlugin::new(meta_tx));

    // Helper to connect a plugin to NRI (best-effort)
    async fn connect_plugin<P: nri::api_ttrpc::Plugin + Send + Sync + 'static>(
        plugin: Arc<P>,
        name: &str,
        idx: &str,
    ) -> Result<Option<(NRI, tokio::task::JoinHandle<Result<()>>)>> {
        let socket_path = std::env::var("NRI_SOCKET_PATH")
            .unwrap_or_else(|_| "/var/run/nri/nri.sock".to_string());
        match tokio::net::UnixStream::connect(&socket_path).await {
            Ok(stream) => {
                info!("Connecting {} to NRI at {}", name, socket_path);
                let (nri, join) = NRI::new(stream, plugin, name, idx).await?;
                if let Err(e) = nri.register().await {
                    warn!("{} registration failed (continuing without): {}", name, e);
                    Ok(None)
                } else {
                    Ok(Some((nri, join)))
                }
            }
            Err(e) => {
                warn!(
                    "NRI socket unavailable for {} ({}): {}",
                    name, socket_path, e
                );
                Ok(None)
            }
        }
    }

    // Attempt connections
    let nri_resctrl = connect_plugin(resctrl_plugin.clone(), "resctrl-plugin", "10").await?;
    let nri_meta = connect_plugin(meta_plugin.clone(), "metadata-plugin", "10").await?;

    // Internal state
    let mut state = ResctrlCollectorState::new(this.clone(), batch_sender, &cfg);

    // Intervals
    let mut sample_tick = tokio::time::interval(cfg.sample_interval);
    let mut retry_tick = tokio::time::interval(cfg.retry_interval);
    let mut health_tick = tokio::time::interval(cfg.health_interval);

    // Lifecycle join handles (detached through completion handler in caller typically)
    let (mut nri_resctrl_handle, mut nri_meta_handle) = (None, None);
    if let Some((_nri, jh)) = nri_resctrl {
        nri_resctrl_handle = Some(jh);
    }
    if let Some((_nri, jh)) = nri_meta {
        nri_meta_handle = Some(jh);
    }

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                break;
            }
            // Periodic sample
            _ = sample_tick.tick() => {
                state.handle_sample_timer();
            }
            // Retry plugin work if connected
            _ = retry_tick.tick(), if nri_resctrl_handle.is_some() => {
                state.handle_retry_timer(&resctrl_plugin);
            }
            // Health logging
            _ = health_tick.tick() => {
                state.handle_health_timer();
            }
            // Resctrl events
            maybe_ev = resctrl_rx.recv() => {
                if let Some(ev) = maybe_ev {
                    state.handle_resctrl_event(ev);
                }
            }
            // Metadata events
            maybe_meta = meta_rx.recv() => {
                if let Some(msg) = maybe_meta {
                    state.handle_metadata_event(msg);
                }
            }
        }
    }

    // Best-effort: close NRI connections
    if let Some(jh) = nri_resctrl_handle {
        jh.abort();
    }
    if let Some(jh) = nri_meta_handle {
        jh.abort();
    }
    Ok(())
}
