Title: Per-pod cache occupancy sampling via Linux resctrl (1 Hz) and Parquet output

Summary
- Add a new sampling path that records per-pod Last Level Cache (LLC) occupancy every second using the Linux resctrl (RDT) subsystem and writes results as Parquet, labeled with pod metadata.
- Build a new crate `resctrl-collector` that integrates with our existing NRI plugins (resctrl monitor and metadata) and emits Arrow RecordBatches over a channel for the collector to persist via a dedicated Parquet writer.
- Extend the existing `resctrl` crate with a method to read LLC occupancy for a given monitor group.

Motivation
- We want continuous, low-overhead visibility into per-workload cache occupancy to support performance analysis and capacity planning.
- We already have many building blocks: an NRI plugin that creates/maintains resctrl monitor groups per pod and signals their lifecycle, and an NRI metadata plugin that provides pod metadata for labeling.
- This issue proposes the wiring and minimal missing functionality to make a working end-to-end pipeline.

Goals
- 1 Hz sampling of per-pod LLC occupancy via resctrl monitor groups (MVP; per-container may be considered later).
- Produce Parquet files with labels sufficient to join to workloads: namespace, pod name, pod UID, plus resctrl group info.
- Integrate cleanly with the collector’s Parquet writing and storage configuration using a dedicated writer instance for occupancy data.
- Provide a crate-level API that streams Arrow RecordBatches so the collector can consume without tight coupling.

Non-Goals
- CPU/memory throttling or control; this is metrics-only.
- Advanced resctrl features beyond LLC occupancy (e.g., MBM/TMEMBW) in the first iteration.
- Cross-platform support; this feature targets Linux with resctrl support only.

Background
- Linux resctrl (RDT) exposes monitoring for LLC occupancy per monitoring group under the mounted resctrl filesystem (commonly `/sys/fs/resctrl`).
- Monitoring is per-domain (e.g., L3 cache domain / socket). A monitor group typically reports per-domain values (e.g., under `mon_data/mon_L3_XX/llc_occupancy`).
- Our existing NRI resctrl plugin creates a monitor group per pod (or container) and can notify interested components when groups are created/removed. The metadata plugin provides pod metadata signals we can use for labeling.

Proposed Design
1) New crate: `resctrl-collector`
   - Responsibilities:
     - Initialize and subscribe to:
       - NRI resctrl monitor plugin: receive notifications of new/removed monitor groups, with identifiers sufficient to locate the resctrl monitor group path for sampling and associate it to a pod.
       - NRI metadata plugin: receive updates supplying pod name, namespace, UID, etc.
       - Communication is in-process via Rust mpsc channels as implemented by the NRI crates; no external IPC is required.
     - Maintain in-memory state:
       - Map from pod identifiers -> resctrl monitor group path(s) and domain info.
       - Map from pod identifiers -> metadata labels (namespace, pod name, pod UID).
       - Handle lifecycle: add/update on notifications; remove on delete events from the plugin (no local eviction heuristics).
     - Sampling loop:
       - Every second (configurable), iterate over known monitor groups and read LLC occupancy via the `resctrl` crate; aggregate across present domains.
       - Use monotonic time for scheduling and system clock for timestamps.
       - Create Arrow RecordBatches for all samples, labeled with whatever metadata is available at sampling time.
       - Emit batches over a bounded async channel supplied by the collector pipeline.
     - Periodic maintenance/health:
       - Call `retry_all_once()` on the plugin every 10 seconds (interval is configurable via a Config struct).
       - Every 60 seconds (configurable), check and report via `info!()` counts of:
         - Pods whose groups are in Failed state
         - Pods with un-reconciled containers (total != reconciled)
     - Backpressure / resilience:
       - Use a bounded channel of fixed capacity (64) for RecordBatches; on full, use `try_send` and drop the newest batch rather than blocking.
       - Track and report drop counts using metrics and rate-limited logs (warn once per second if any drops occurred), mirroring the existing timeslot→recordbatch translator.
       - Log and continue on per-group read failures. Treat errors primarily as races with group removal; do not locally evict groups—rely on NRI for lifecycle.

2) Extend crate: `resctrl`
   - Add a read method to fetch LLC occupancy for a monitor group. Example API idea:
     - `fn llc_occupancy_bytes(&self, group: &MonitorGroup) -> Result<Vec<DomainReading>>`
       - Where `DomainReading { domain_id: String, bytes: u64 }` corresponds to each `mon_L3_XX` (or equivalent domain) present for the group.
     - Optionally also provide a convenience aggregation: `fn llc_occupancy_total_bytes(&self, group: &MonitorGroup) -> Result<u64>` that sums across domains.
   - Implementation notes:
     - Discover domains present for the group (e.g., enumerate `mon_data/mon_L3_*`).
     - Read `llc_occupancy` for each domain.
     - When the group disappears between discovery and read, return an error that the caller treats as a potential race with group removal. The caller (resctrl-collector) should aggregate and warn (rate-limited) with a count of groups that failed mid-measurement rather than logging each error individually.
     - Do not maintain open file descriptors for resctrl files in this iteration; read as needed per sample for simplicity.

3) Collector integration
   - Add a new config section `resctrl_collector` (feature gated, disabled by default):
     - `enabled` (bool, default false)
     - `sampling_interval` (duration, default 1s)
     - `resctrl_mount` (path, default `/sys/fs/resctrl`)
     - Channel capacity is fixed at 64; not configurable in this iteration.
     - Domains are always aggregated in this iteration (no per-domain output option).
   - Instantiate `resctrl-collector` when enabled. The collector creates the RecordBatch output `mpsc::Sender` and passes it into `resctrl-collector`.
   - `resctrl-collector` exposes a `run()` that accepts a cancellation token and, on shutdown, closes the output channel to signal downstream completion. Internally it uses a TaskTracker to manage spawned tasks, calls `close()` after spawning, and `wait()`s during shutdown.
   - Write Parquet using a dedicated ParquetWriter instance for occupancy data, with its own schema and filename prefix, separate from other writers.
   - Helm: add a chart values block to enable/configure this feature (see charts/collector/values.yaml), wiring values into the pod environment. Feature remains disabled by default.

Output Schema (initial)
- timestamp_ns: i64
- pod_namespace: utf8
- pod_name: utf8
- pod_uid: utf8
- resctrl_group: utf8 (monitor group path or logical name)
- llc_occupancy_bytes: u64

Notes:
- Sampling interval should be captured as Parquet file metadata if supported by the existing writer, not as a column.

Minimal Viable Path
- Implement per-pod monitor groups only; label at pod level (no container list in this iteration).
- Aggregate across domains by default to provide one value per group per tick.

Error Handling & Lifecycle
- If a resctrl group disappears mid-sample, treat as a race with group removal: aggregate failures and emit a periodic warn! with a count (rate-limited). Do not schedule local removal; rely on NRI for lifecycle.
- If metadata is missing at sample time, emit metrics with whatever labels are available. Since ordering across metadata channels is not guaranteed, if only cgroup information is available, include pod UID only; enrich later as updates arrive.
- Protect against resctrl mount absence or unsupported hardware by gating feature startup. When enabled but unavailable, set the collector’s readiness to Not Ready so operators can detect issues (in addition to clear diagnostics/logs).

Security & Permissions
- The collector process must be able to read the resctrl filesystem (often requires privileged access or specific capabilities in containerized deployments).
- The nri-resctrl-plugin is responsible for adding container tasks to monitor group `tasks` files; we assume it functions correctly (verified by integration tests).

Design Decisions and Open Questions
1) Granularity: Use per-pod monitor groups for MVP. Per-container would require separate monitor groups and is out of scope for now.
2) Domain handling: Always aggregate across domains in this iteration; no per-domain output.
3) Communication model: In-process Rust mpsc channels between the collector and NRI crates; no external IPC required.
4) Label completeness: Ordering across channels is best-effort. If only cgroup info is available, record pod UID only; enrich later when metadata arrives.
5) Time source: Use monotonic clock for scheduling and system clock for timestamps.

Acceptance Criteria
- `resctrl` crate exposes an API to read LLC occupancy for a given monitor group, with unit tests for path parsing and domain enumeration.
- `resctrl-collector` crate:
  - Initializes/consumes NRI resctrl and metadata signals in-process.
  - Maintains a mapping from pod identifiers to monitor groups and metadata.
  - Samples all known groups at 1 Hz by default and emits Arrow RecordBatches over the provided channel.
  - Uses bounded channel with drop-on-full, metrics, and periodic logging of drops.
  - Handles group removal races gracefully, with aggregated warnings.
  - Provides `run()` compatible with the collector’s cancellation-token approach, closes output channel on shutdown, and uses a TaskTracker to manage tasks.
- Collector integrates the RecordBatch channel with a dedicated Parquet writer for occupancy data, producing Parquet files in the configured storage.
- Feature is gated by config and disabled by default; readiness reflects resctrl availability when enabled.

Implementation Plan (high-level)
- [ ] Implement `resctrl` crate read API for LLC occupancy across domains; unit test path parsing and domain enumeration.
- [ ] Build `resctrl-collector` crate with in-process channel subscriptions to NRI resctrl and metadata plugins.
- [ ] Implement 1 Hz sampling loop (Tokio interval), reading via `resctrl` crate, aggregating across domains, and building RecordBatches.
- [ ] Bounded channel and backpressure strategy; metrics/logging on drops (drop-on-full with periodic warnings).
- [ ] Add periodic plugin `retry_all_once()` and health reporting (failed groups, unreconciled containers).
- [ ] Basic integration tests behind a feature flag with a mocked `resctrl` trait.
- Collector integration
  - [ ] Config surface `resctrl_collector` with defaults; env wiring in Helm chart.
  - [ ] Instantiate `resctrl-collector` when enabled and connect it to a new Parquet writer dedicated to occupancy data.
  - [ ] Define Parquet schema and ensure partitioning/rotation align with existing conventions.

Risks & Mitigations
- Hardware/kernel dependency: resctrl not present or disabled. Mitigate with feature gating and clear diagnostics; expose readiness Not Ready when enabled but unavailable.
- Permissions: insufficient privileges to read resctrl. Document required capabilities and provide startup checks.
- Overhead: sampling loop should be light. Avoid maintaining open file descriptors; read per sample for simplicity.

Notes on What’s Structurally Required (must-have to work)
- NRI resctrl plugin must create monitor groups and keep pod tasks assigned to those groups; otherwise data is meaningless.
- The plugin exposes identifiers sufficient to map monitor group paths to pods via an in-process Rust mpsc API that `resctrl-collector` consumes (no external IPC).
- The `resctrl` crate must support reading LLC occupancy values from a given monitor group, across domains.
- The collector must have permission to read resctrl filesystem.

Definition of Done
- When enabled, the collector writes Parquet files containing 1 Hz LLC occupancy samples for each active monitor group with pod-level labels and passes basic integration checks on a resctrl-capable node.

