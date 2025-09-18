Title: Per-container cache occupancy sampling via Linux resctrl (1 Hz) and Parquet output

Summary
- Add a new sampling path that records per-container Last Level Cache (LLC) occupancy every second using the Linux resctrl (RDT) subsystem and writes results as Parquet, labeled with pod/container metadata.
- Build a new crate `resctrl-collector` that integrates with our existing NRI plugins (resctrl monitor and metadata) and emits Arrow RecordBatches over a channel for the collector to persist via the existing Parquet writer.
- Extend the existing `resctrl` crate with a method to read LLC occupancy for a given monitor group.

Motivation
- We want continuous, low-overhead visibility into per-container cache occupancy to support performance analysis and capacity planning.
- We already have many building blocks: an NRI plugin that creates/maintains resctrl monitor groups per pod and signals their lifecycle, and an NRI metadata plugin that provides pod metadata for labeling.
- This issue proposes the wiring and minimal missing functionality to make a working end-to-end pipeline.

Goals
- 1 Hz sampling of per-container (or per-pod, see open questions) LLC occupancy via resctrl monitor groups.
- Produce Parquet files with labels sufficient to join to workloads: namespace, pod name, pod UID, container name/ID, plus resctrl group info.
- Integrate cleanly with existing collector Parquet writer and storage configuration.
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
       - NRI resctrl monitor plugin: receive notifications of new/removed monitor groups, with identifiers sufficient to locate the resctrl monitor group path for sampling and associate it to a pod/container.
       - NRI metadata plugin: receive updates supplying pod name, namespace, UID, container names/IDs, etc.
     - Maintain in-memory state:
       - Map from pod/container identifiers -> resctrl monitor group path(s) and domain info.
       - Map from pod/container identifiers -> metadata labels (namespace, pod name, pod UID, container name/ID, node, etc.).
       - Handle lifecycle: add/update on notifications; remove/evict on delete.
     - Sampling loop:
       - Every second (configurable), iterate over known monitor groups and read LLC occupancy via the `resctrl` crate for each present domain.
       - Create Arrow RecordBatches for all samples, labeled with metadata available at sampling time.
       - Send RecordBatches over a bounded async channel to the collector’s Parquet writer path.
     - Backpressure / resilience:
       - Bounded channel with drop/newest or coalescing policy (configurable) to avoid unbounded memory growth.
       - Log and continue on per-group read failures; evict groups that consistently fail or are removed upstream.

2) Extend crate: `resctrl`
   - Add a read method to fetch LLC occupancy for a monitor group. Example API idea:
     - `fn llc_occupancy_bytes(&self, group: &MonitorGroup) -> Result<Vec<DomainReading>>`
       - Where `DomainReading { domain_id: String, bytes: u64 }` corresponds to each `mon_L3_XX` (or equivalent domain) present for the group.
     - Optionally also provide a convenience aggregation: `fn llc_occupancy_total_bytes(&self, group: &MonitorGroup) -> Result<u64>` that sums across domains.
   - Implementation notes:
     - Discover domains present for the group (e.g., enumerate `mon_data/mon_L3_*`).
     - Read `llc_occupancy` for each domain.
     - Return robust errors when the group disappears between discovery and read; callers should treat as transient unless persistent.

3) Collector integration
   - Add a new config section `resctrl_collector`:
     - `enabled` (bool, default false)
     - `sampling_interval` (duration, default 1s)
     - `resctrl_mount` (path, default `/sys/fs/resctrl`)
     - `aggregate_domains` (bool, default true): if true, emit a single aggregated value per sample; if false, emit per-domain samples.
     - `eviction_grace` (duration): how long to retain unseen groups before evicting.
     - `channel_capacity` (int): bounded channel size for RecordBatches.
   - Instantiate `resctrl-collector` when enabled and subscribe the Parquet writer to its output channel.
   - Write Parquet using the existing writer configured storage/rotation.

Output Schema (initial)
- timestamp_ns: i64
- node: utf8 (if available)
- pod_namespace: utf8
- pod_name: utf8
- pod_uid: utf8
- container_name: utf8
- container_id: utf8
- resctrl_group: utf8 (monitor group path or logical name)
- domain_id: utf8 (socket/cache domain; empty if aggregated)
- llc_occupancy_bytes: u64
- sampling_interval_ms: u32

Minimal Viable Path
- Implement per-pod monitor groups only; label at pod level; optionally include container list for future fan-out.
- Aggregate across domains by default to provide one value per group per tick.

Error Handling & Lifecycle
- If a resctrl group disappears mid-sample, mark that sample as failed for that group and schedule removal unless the NRI plugin re-adds it.
- If metadata is missing at sample time, emit metrics with whatever labels are available; populate missing labels as they arrive.
- Protect against resctrl mount absence or unsupported hardware by gating feature startup and emitting a clear diagnostic; do not crash the collector.

Security & Permissions
- The collector process must be able to read the resctrl filesystem (often requires privileged access or specific capabilities in containerized deployments).
- NRI resctrl plugin must ensure container tasks are added to the monitor group `tasks` file; otherwise occupancy will be zero or stale. This is a structural requirement.

Open Questions
1) Pod vs container granularity: Should the NRI resctrl plugin create one group per pod or per container? If per-pod, do we also need a fan-out to per-container values? For MVP, we can record per-pod; per-container would require separate monitor groups.
2) Domain handling: Do we emit a row per domain or aggregate by default? Proposal: aggregate by default with an option to emit per-domain.
3) Communication model: The NRI plugins currently “send a message on a channel”. To keep this in-process, we should use library crates with internal channels rather than external daemons. If they run out-of-process, we need a concrete IPC (e.g., gRPC) and a client in `resctrl-collector`.
4) Label completeness: Which fields are guaranteed from metadata at the time the resctrl group appears (e.g., pod UID, container ID, container name)? We should define a stable key set to avoid label churn.
5) Time source: Use monotonic clock for scheduling and system clock for timestamps. Confirm precision and any clock sync requirements.

Acceptance Criteria
- `resctrl` crate exposes an API to read LLC occupancy for a given monitor group, with unit tests for path parsing and domain enumeration.
- `resctrl-collector` crate:
  - Initializes/consumes NRI resctrl and metadata signals in-process.
  - Maintains a mapping from identifiers to monitor groups and metadata.
  - Samples all known groups at 1 Hz by default and emits Arrow RecordBatches over a channel.
  - Handles group removal and transient read failures gracefully.
- Collector integrates the RecordBatch channel into the existing Parquet writer, producing Parquet files in the configured storage.
- Feature is gated by config and disabled by default.

Implementation Plan (high-level)
- resctrl crate
  - [ ] Add domain discovery under a monitor group (e.g., `mon_data/mon_L3_*`).
  - [ ] Add `llc_occupancy_bytes` read API returning per-domain values and a convenience aggregator.
  - [ ] Unit tests using a fake filesystem layout to validate parsing and error cases.
- resctrl-collector crate
  - [ ] Define public API: builder/config, `run()` task, output `mpsc::Receiver<RecordBatch>`.
  - [ ] Wire NRI resctrl plugin events to internal state (add/update/remove monitor groups, include group path and association keys).
  - [ ] Wire NRI metadata plugin events to internal label state.
  - [ ] Implement 1 Hz sampling loop (Tokio interval), reading via `resctrl` crate and building RecordBatches.
  - [ ] Bounded channel and backpressure strategy; metrics/logging on drops.
  - [ ] Basic integration tests behind a feature flag with a mocked `resctrl` trait.
- Collector integration
  - [ ] Config surface `resctrl_collector` with defaults.
  - [ ] Instantiate `resctrl-collector` when enabled and connect its channel to the existing Parquet writer.
  - [ ] Define Parquet schema and ensure partitioning/rotation align with existing conventions.

Risks & Mitigations
- Hardware/kernel dependency: resctrl not present or disabled. Mitigate with feature gating and clear diagnostics; no-op when unavailable.
- Permissions: insufficient privileges to read resctrl. Document required capabilities and provide startup checks.
- Overhead: sampling loop should be light, but ensure minimal file reads and reuse opened descriptors where possible.
- Label churn: late/changed metadata could produce cardinality issues. Use stable keys (UIDs) and update labels carefully.

Notes on What’s Structurally Required (must-have to work)
- NRI resctrl plugin must create monitor groups and keep container/pod tasks assigned to those groups; otherwise data is meaningless.
- The plugin must expose identifiers sufficient to map monitor group paths to pods/containers (e.g., pod UID, container ID) via an in-process API or well-defined IPC that `resctrl-collector` consumes.
- The `resctrl` crate must support reading LLC occupancy values from a given monitor group, across domains.
- The collector must have permission to read resctrl filesystem.

Follow-ups (future work, not blocking MVP)
- Add Memory Bandwidth Monitoring (MBM) counters alongside LLC occupancy.
- Per-container monitor groups if we start with per-pod.
- Export Prometheus gauge for live inspection in addition to Parquet.
- Adaptive sampling interval and jitter to reduce contention.

Definition of Done
- When enabled, the collector writes Parquet files containing 1 Hz LLC occupancy samples for each active monitor group with pod/container labels and passes basic integration checks on a resctrl-capable node.

