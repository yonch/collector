Title: Integration Test Plan — resctrl-collector (LLC occupancy via Linux resctrl)

Context
- Source issue: issue-cache-collection.md
- Components under test:
  - resctrl-collector library: crates/resctrl-collector/src/lib.rs
  - resctrl NRI plugin: crates/nri-resctrl-plugin/src/lib.rs
  - resctrl filesystem reader: crates/resctrl/src/lib.rs
  - Collector binary integration: crates/collector/src/main.rs

Objectives
- Validate end-to-end behavior for per-pod LLC occupancy sampling at 1 Hz and persistence to Parquet, including readiness gating and resilience.
- Verify correctness of labeling (namespace, pod name, UID) and group mapping across lifecycle events.
- Exercise failure paths (resctrl unavailable, invalid values, backpressure) and ensure graceful degradation with observable signals.

Test Scope & Levels
- L0 Unit (already present): resctrl crate parses domain files and aggregates totals.
- L1 Plugin Integration (already present): nri-resctrl-plugin responds to NRI events and manages groups/tasks.
- L2 Collector Integration (this plan): resctrl-collector consumes plugin + metadata streams, samples resctrl, emits Arrow RecordBatches with correct schema and values.
- L3 Binary E2E (this plan): full collector binary with resctrl enabled writes Parquet files on a resctrl-capable node; readiness and rotation behave as expected.

Key Behaviors To Verify
- Schema correctness for occupancy batches: crates/resctrl-collector/src/lib.rs:23
- Readiness gating: resctrl enabled implies readiness waits for first events from both resctrl and metadata: crates/resctrl-collector/src/lib.rs:49, crates/collector/src/main.rs:342
- Sampling interval and drop-on-backpressure behavior: crates/resctrl-collector/src/lib.rs:192, crates/resctrl-collector/src/lib.rs:285
- Multi-domain L3 aggregation correctness: crates/resctrl/src/lib.rs:119
- Error handling: invalid or missing llc_occupancy values do not crash sampling loop; events continue: crates/resctrl-collector/src/lib.rs:238
- Health logging counts for failed groups and unreconciled containers: crates/resctrl-collector/src/lib.rs:206
- Env-driven configuration honored (RESCTRL_*): crates/resctrl-collector/src/lib.rs:80

Prerequisites
- Linux kernel with resctrl support for L3 monitoring.
- NRI runtime socket available (containerd 2.x default) or enabled per docs/nri-setup.md.
- For hermetic/in-process tests, ability to run Tokio tests and write to a temp filesystem (no kernel features required when using a fake resctrl tree).

L2: In-Process Integration Tests (Hermetic)

Approach
- Add minimal testability hooks gated behind a test-only feature to avoid production impact.
- Keep all logic identical to production by executing the same sampling loop and Arrow batch construction.

Testability Hooks (small additions)
- Add a cfg(test) constructor to resctrl-collector that accepts:
  - Injected receivers for resctrl events and metadata messages (tokio mpsc::Receiver).
  - A Resctrl instance configured with a test mountpoint (PathBuf) for reading occupancy.
  - Timing parameters (shortened sampling/health intervals).
- Rationale: run() currently creates channels and plugins internally, making controlled event injection impossible during tests. This DI surface is strictly cfg(test) and mirrors production loop semantics.

Fixtures
- Build a temporary directory tree to simulate a resctrl monitor group:
  - <tmp>/mon_groups/pod_testuid/mon_data/mon_L3_00/llc_occupancy
  - <tmp>/mon_groups/pod_testuid/mon_data/mon_L3_01/llc_occupancy
- Write known numeric values to llc_occupancy files.

Test Cases (Hermetic)
1) Ready Gating (Cold Start)
   - Start loop with injected channels and no messages → ready() is false.
   - Send one PodResctrlEvent::AddOrUpdate for a pod with group path pointing to temp group; send one MetadataMessage::Add for the same pod UID.
   - Expect ready() becomes true.

2) Schema and Labeling
   - After case (1), wait a few ticks, collect one emitted RecordBatch from the batch receiver.
   - Validate Arrow schema fields exist and types match: timestamp(i64), pod_namespace(utf8, nullable), pod_name(utf8, nullable), pod_uid(utf8), resctrl_group(utf8), llc_occupancy_bytes(i64).
   - Confirm row contains labels from metadata (namespace/name) and the UID/group path sent in events.

3) Multi-Domain Aggregation
   - With two mon_L3_* files present (e.g., 123 and 456), expect llc_occupancy_bytes == 579.
   - Remove one domain dir between ticks; ensure totals reflect remaining domain only.

4) Missing Metadata Path
   - Send only PodResctrlEvent::AddOrUpdate (no metadata yet).
   - Expect ns/name columns are NULL while uid/group and llc value are populated; labels populate on subsequent batches after metadata arrives.

5) Removal Lifecycle
   - After initial sampling, send PodResctrlEvent::Removed for the pod UID; remove fake group tree.
   - Expect no further rows for that UID after a grace tick; loop continues for other pods if present.

6) Error Handling: Invalid Values
   - Write a non-numeric llc_occupancy value; expect the row is skipped with a debug log and loop continues.
   - Restore valid values; rows resume without crash.

7) Backpressure/Drops
   - Configure channel_capacity to a very small number (e.g., 1) and sample_interval to ~10–20ms.
   - Intentionally block the downstream batch receiver for >1s to force try_send failures.
   - Expect fewer batches received than ticks and a warning log once per second indicating dropped count.

8) Health Logging
   - Emit AddOrUpdate events with ResctrlGroupState::Failed and with total_containers > reconciled_containers.
   - After health_interval, assert a log line exists like "resctrl health: pods_failed=..., pods_unreconciled=..." with correct counts.

9) Env Configuration
   - Set RESCTRL_SAMPLING_INTERVAL, RESCTRL_RETRY_INTERVAL, RESCTRL_HEALTH_INTERVAL, RESCTRL_CHANNEL_CAPACITY, RESCTRL_MOUNT with non-default values.
   - Build config via ResctrlCollectorConfig::from_env() and assert fields match.

Deliverables (Hermetic)
- tests/integration_resctrl_collector.rs under crates/resctrl-collector with #[tokio::test] cases above.
- cfg(test) helper in crates/resctrl-collector/src/lib.rs to run a loop with injected receivers and a custom Resctrl instance.

L3: Binary E2E (Cluster or Single-Node)

Approach
- Validate end-to-end from NRI events → resctrl-collector batches → dedicated Parquet writer.
- Use the Helm chart to run the collector as a DaemonSet with resctrl enabled and storage.type=local for simple file inspection.

Environment Options
- KIND (v0.27.0+) with containerd 2.x and NRI enabled by default.
- K3s release with containerd 2.x (see docs/nri-setup.md for version matrix).
- Node must expose /sys/fs/resctrl and permit read access from the collector pod (chart mounts host path).

Setup
- helm install collector charts/collector with overrides:
  - resctrl.enabled=true
  - storage.type=local
  - storage.prefix=/tmp/resctrl-occupancy-
  - collector.verbose=true
  - Optionally set resctrl.samplingInterval="250ms" to accelerate test.
- Confirm NRI socket present in pods via init logs or enable automatic config per docs.

Workload
- Deploy a small CPU-active pod (e.g., busybox running a tight loop) to ensure non-zero occupancy.
- Ensure at least two pods run on the same node to exercise multiple groups.

Assertions
1) Readiness
   - Health endpoint /ready should transition from false to true only after both metadata and resctrl events are observed.
   - For resctrl disabled or unavailable nodes, /ready remains false when resctrl.enabled=true.

2) Parquet Output
   - Files with prefix resctrl-occupancy-... appear in the configured local storage path.
   - Using parquet-tools or Rust parquet reader, validate schema fields and that rows exist for active pods with non-zero llc_occupancy_bytes.
   - Validate partitioning/rotation via SIGUSR1 to the resctrl writer process (see crates/collector/src/main.rs:355 for rotation wiring).

3) Labeling
   - Rows contain pod_namespace, pod_name, and pod_uid matching the running workloads.

4) Multi-Domain Systems
   - On multi-socket nodes, verify occupancy values vary per pod and overtime; if possible, temporarily pin workloads to different sockets and observe differences.

5) Failure/Permission Scenarios
   - Run a pod without resctrl mounted (toggle chart mount off) with resctrl.enabled=true → expect Not Ready and no Parquet output; logs should explain missing permissions or mount.

Timing & Tuning
- Keep test window short by increasing sampling frequency (e.g., 250ms) and limiting runtime to ~10–20 seconds.

Artifacts & Inspection
- Archive produced Parquet files as CI artifacts for debugging.
- Capture collector logs to verify health summaries and drop warnings.

CI Integration
- Add a CI job that provisions a KIND cluster (v0.27.0+), installs the Helm chart with local storage, runs workload, gathers Parquet files, and performs schema/row assertions (e.g., via a small Rust or Python checker step).
- Gate E2E behind a label or nightly schedule due to kernel and privilege requirements; keep L2 hermetic tests in the default PR workflow.

Risks & Mitigations
- Hardware/kernel feature dependency: skip E2E when resctrl is unavailable; rely on hermetic L2.
- NRI absent on some runners: use KIND ≥ v0.27.0 to ensure containerd 2.x with NRI.
- Flaky timing in async tests: prefer deterministic waiting on channels over sleeps; bound with generous timeouts.

Success Criteria
- L2 tests consistently pass on CI without kernel dependencies.
- L3 E2E demonstrates non-zero llc_occupancy_bytes per active pod with correct labels and schema; readiness and rotation behave as designed.
