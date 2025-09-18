Title: Test Plan — resctrl-collector after handler refactor

Context
- Source issue: issue-cache-collection.md
- Components under test:
  - resctrl-collector library: crates/resctrl-collector/src/lib.rs
  - resctrl NRI plugin: crates/nri-resctrl-plugin/src/lib.rs
  - resctrl filesystem reader: crates/resctrl/src/lib.rs
  - Collector binary integration: crates/collector/src/main.rs

Objectives
- Validate correctness of handler-level logic (handle_* functions) independently, with fast, hermetic unit tests.
- Validate end-to-end behavior for per-pod LLC occupancy sampling and persistence to Parquet, including readiness gating and resilience.
- Verify correctness of labeling (namespace, pod name, UID) and group mapping across lifecycle events.
- Exercise failure paths (resctrl unavailable, invalid values, backpressure) and ensure graceful degradation with observable signals.

Test Scope & Levels
- L0a Unit — resctrl crate (already present): parses domain files and aggregates totals.
- L0b Unit — collector handlers (new): test handle_* functions in crates/resctrl-collector/src/lib.rs in isolation with fakes/mocks.
- L1 Plugin Integration (already present): nri-resctrl-plugin responds to NRI events and manages groups/tasks.
- L2 Collector Integration: minimal smoke of run() to cover wiring and readiness.
- L3 Binary E2E: full collector binary with resctrl enabled writes Parquet; readiness and rotation behave as expected.

Key Behaviors To Verify
- Schema correctness for occupancy batches: crates/resctrl-collector/src/lib.rs (create_schema).
- Readiness gating: ready() flips only after at least one resctrl and one metadata event.
- Sampling and drop-on-backpressure behavior: handle_sample_timer aggregates, emits, and rate-logs drops.
- Multi-domain aggregation correctness: verified in crates/resctrl (llc_occupancy_total_bytes); collector propagates totals into batches.
- Error handling: invalid or missing llc_occupancy values skip rows, loop continues.
- Health logging counts for failed groups and unreconciled containers: handle_health_timer logs expected summary.
- Env-driven configuration honored (RESCTRL_*): ResctrlCollectorConfig::from_env.

Prerequisites
- Linux kernel with resctrl support for L3 monitoring (for E2E only).
- NRI runtime socket available (containerd 2.x default) or enabled per docs/nri-setup.md (for E2E only).
- For unit/in-process tests, only Tokio and an in-memory fake or temp filesystem; no kernel features required.

L0b: Handler Unit Tests (Hermetic)

Approach
- Exercise handler functions directly without spawning the run() loop.
- Inject a fake occupancy reader into sampling logic via a small trait to avoid touching the real filesystem.

Testability Hooks (small additions)
- Define a tiny trait in crates/resctrl-collector for sampling:
  trait LlcReader { fn llc_occupancy_total_bytes(&self, group_path: &str) -> anyhow::Result<u64>; }
  - Implement for resctrl::Resctrl so production code remains unchanged.
  - In tests, implement a FakeLlcReader to return canned values or errors per group.
- Update handle_sample_timer to accept a &dyn LlcReader (or make ResctrlCollectorState generic over LlcReader). Production path passes the real resctrl handle.
- Keep ResctrlCollectorState and handlers pub(crate) so crate-local tests can instantiate and drive them.

Fixtures
- Minimal: mpsc::channel<RecordBatch> with small capacity for backpressure tests.
- FakeLlcReader map: group_path → Result<u64> to control sampling outcomes.

Test Cases (Hermetic)
1) Ready Gating via handlers
   - Instantiate ResctrlCollector and ResctrlCollectorState with a dummy sender.
   - Assert ready() is false; call handle_resctrl_event(AddOrUpdate with Exists) and handle_metadata_event(Add) once each.
   - Expect ready() becomes true.

2) Schema and Labeling
   - Use FakeLlcReader to return a fixed total for a known group path.
   - After sending AddOrUpdate + Add metadata for a UID, call handle_sample_timer with the fake reader.
   - Receive one RecordBatch; validate schema fields and that row contains namespace/name/uid/group and the expected llc_occupancy_bytes value.

3) Missing Metadata Path
   - Send only AddOrUpdate (no metadata); sample.
   - Expect ns/name columns are NULL while uid/group and llc are populated; after sending metadata and sampling again, labels appear.

4) Removal Lifecycle
   - After initial sampling, send Removed for the pod UID.
   - Subsequent samples emit no rows for that UID.

5) Error Handling: Read Failure
   - Configure FakeLlcReader to return an error for the group.
   - Sampling should skip the row and not send a batch; assert via receiver emptiness and presence of a debug log.

6) Backpressure/Drops
   - Create batch channel with capacity 1; perform two samples without draining the receiver to force try_send failure.
   - Capture logs; expect a warning of the form "Dropped N occupancy batches..." and that only one batch was received.

7) Health Logging
   - Drive state via handle_resctrl_event calls to produce pods with group_path None and with reconciled_containers < total_containers.
   - Call handle_health_timer; assert a single info log line summarizing failed and unreconciled counts.

8) Env Configuration
   - Set RESCTRL_SAMPLING_INTERVAL, RESCTRL_RETRY_INTERVAL, RESCTRL_HEALTH_INTERVAL, RESCTRL_CHANNEL_CAPACITY, RESCTRL_MOUNT with non-default values.
   - Build config via ResctrlCollectorConfig::from_env() and assert fields match.

Deliverables (Hermetic)
- Unit tests colocated in crates/resctrl-collector/src/lib.rs under #[cfg(test)] to access private handler/state.
- Tiny LlcReader trait and impl (production and test fakes) in crates/resctrl-collector.

L2: Collector Integration (run loop, smoke)

Approach
- Exercise resctrl_collector::run with a short sampling/health interval and a real mpsc sender.
- Do not depend on NRI connectivity; the loop runs even when plugins fail to connect.

Assertions
- Start run() with a shutdown token; ensure it starts, ticks at least once (e.g., by observing a harmless sample with no pods), and stops cleanly on cancellation.
- Ready provider in crates/collector uses ResctrlCollector::ready(); verify default readiness gating behavior remains false until both event types are observed (unit-tested above).

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
   - Health endpoint /ready transitions from false to true only after both metadata and resctrl events are observed.
   - For resctrl disabled or unavailable nodes, /ready remains false when resctrl.enabled=true.

2) Parquet Output
   - Files with prefix resctrl-occupancy-... appear in the configured local storage path.
   - Using parquet-tools or Rust parquet reader, validate schema fields and that rows exist for active pods with non-zero llc_occupancy_bytes.
   - Validate partitioning/rotation via SIGUSR1 to the resctrl writer process (see rotation wiring in crates/collector/src/main.rs).

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
- Gate E2E behind a label or nightly schedule due to kernel and privilege requirements; keep L0 hermetic handler tests in the default PR workflow.

Risks & Mitigations
- Hardware/kernel feature dependency: skip E2E when resctrl is unavailable; rely on hermetic L2.
- NRI absent on some runners: use KIND ≥ v0.27.0 to ensure containerd 2.x with NRI.
- Flaky timing in async tests: prefer deterministic waiting on channels over sleeps; bound with generous timeouts.

Success Criteria
- L0b handler unit tests consistently pass on CI without kernel dependencies.
- L3 E2E demonstrates non-zero llc_occupancy_bytes per active pod with correct labels and schema; readiness and rotation behave as designed.
