use anyhow::{Context, Result};
use arrow_array::{Array, ArrayRef, Float64Array, RecordBatch};
use arrow_schema::{DataType, Field};
use std::collections::HashMap;
use std::sync::Arc;
use std::fs::File;
use std::io::Write;
use tqdm::tqdm;

use crate::analyzer::Analysis;
use crate::cpu_time_tracker::CpuTimeTracker;

/// Binned statistics for off-CPU time and CPI analysis
#[derive(Debug, Clone)]
pub struct OffCpuCpiStatistics {
    bin_width: f64,
    cpi_bin_width: f64,
    max_cpi: f64,
    max_measurement: f64,
    // HashMap<(metric_bin, cpi_bin), instruction_count>
    bins: HashMap<(u32, u32), u64>,
}

impl OffCpuCpiStatistics {
    /// Create new statistics with specified bin widths
    pub fn new(bin_width: f64, cpi_bin_width: f64, max_cpi: f64, max_measurement: f64) -> Self {
        Self {
            bin_width,
            cpi_bin_width,
            max_cpi,
            max_measurement,
            bins: HashMap::new(),
        }
    }
    
    /// Add a measurement to the statistics
    pub fn add_measurement(&mut self, metric_value: f64, cpi: f64, instructions: u64) {
        // Skip invalid measurements
        if metric_value < 0.0 || cpi <= 0.0 || instructions == 0 {
            return;
        }
        
        // Fold measurement values larger than max_measurement into the max measurement bin
        let clamped_metric = if metric_value > self.max_measurement { self.max_measurement } else { metric_value };
        let metric_bin = (clamped_metric / self.bin_width).floor() as u32;
        
        // Fold CPI values larger than max_cpi into the max CPI bin
        let clamped_cpi = if cpi > self.max_cpi { self.max_cpi } else { cpi };
        let cpi_bin = (clamped_cpi / self.cpi_bin_width).floor() as u32;
        
        // Add to bin
        *self.bins.entry((metric_bin, cpi_bin)).or_insert(0) += instructions;
    }
    
    /// Export statistics to CSV
    pub fn export_to_csv(&self, writer: &mut dyn Write, process_name: &str) -> Result<()> {
        for ((metric_bin, cpi_bin), instructions) in &self.bins {
            let metric_min = *metric_bin as f64 * self.bin_width;
            let metric_max = metric_min + self.bin_width;
            let cpi_min = *cpi_bin as f64 * self.cpi_bin_width;
            let cpi_max = cpi_min + self.cpi_bin_width;
            
            writeln!(
                writer,
                "{},{:.2},{:.2},{:.2},{:.2},{}",
                process_name,
                metric_min,
                metric_max,
                cpi_min,
                cpi_max,
                instructions
            )?;
        }
        Ok(())
    }
}

/// Per-PID per-CPU scheduling state for off-CPU time calculation
#[derive(Debug)]
struct PerPidPerCpuState {
    last_scheduled_out_time: HashMap<usize, u64>, // cpu_id -> timestamp when last scheduled out
    last_global_scheduled_out_time: u64, // timestamp when last scheduled out globally
    last_scheduled_out_global_cpu_time: HashMap<usize, u64>, // cpu_id -> global CPU time at last schedule out
    last_global_scheduled_out_global_cpu_time: u64, // global CPU time when last scheduled out globally
}

impl PerPidPerCpuState {
    fn new() -> Self {
        Self {
            last_scheduled_out_time: HashMap::new(),
            last_global_scheduled_out_time: 0,
            last_scheduled_out_global_cpu_time: HashMap::new(),
            last_global_scheduled_out_global_cpu_time: 0,
        }
    }
}

/// Main off-CPU analysis processor
pub struct OffCpuAnalysis {
    num_cpus: usize,
    
    // CPU time tracking (shared with concurrency analysis)
    cpu_time_tracker: CpuTimeTracker,
    
    // Off-CPU time tracking state (module-specific)
    per_pid_state: HashMap<u32, PerPidPerCpuState>,
    
    // Buffered measurements for next event (per CPU)
    buffered_measurements: Vec<(u64, u64, u64, u64)>, // per-CPU: (per_cpu_off_time, global_off_time, per_cpu_other_cpu_time, global_other_cpu_time)
    
    // Statistics tracking
    per_process_per_cpu_off_time_stats: HashMap<String, OffCpuCpiStatistics>,
    per_process_global_off_time_stats: HashMap<String, OffCpuCpiStatistics>,
    per_process_per_cpu_other_cpu_time_stats: HashMap<String, OffCpuCpiStatistics>,
    per_process_global_other_cpu_time_stats: HashMap<String, OffCpuCpiStatistics>,
    
    // Output paths for CSV files
    per_cpu_off_time_csv_path: Option<String>,
    global_off_time_csv_path: Option<String>,
    per_cpu_other_cpu_time_csv_path: Option<String>,
    global_other_cpu_time_csv_path: Option<String>,
}

impl OffCpuAnalysis {
    /// Create a new off-CPU analysis processor
    pub fn new(num_cpus: usize) -> Result<Self> {
        Ok(Self {
            num_cpus,
            cpu_time_tracker: CpuTimeTracker::new(num_cpus),
            per_pid_state: HashMap::new(),
            buffered_measurements: vec![(0, 0, 0, 0); num_cpus],
            per_process_per_cpu_off_time_stats: HashMap::new(),
            per_process_global_off_time_stats: HashMap::new(),
            per_process_per_cpu_other_cpu_time_stats: HashMap::new(),
            per_process_global_other_cpu_time_stats: HashMap::new(),
            per_cpu_off_time_csv_path: None,
            global_off_time_csv_path: None,
            per_cpu_other_cpu_time_csv_path: None,
            global_other_cpu_time_csv_path: None,
        })
    }
    
    /// Set the output CSV file paths
    pub fn set_csv_paths(
        &mut self,
        per_cpu_off_time_path: String,
        global_off_time_path: String,
        per_cpu_other_cpu_time_path: String,
        global_other_cpu_time_path: String,
    ) {
        self.per_cpu_off_time_csv_path = Some(per_cpu_off_time_path);
        self.global_off_time_csv_path = Some(global_off_time_path);
        self.per_cpu_other_cpu_time_csv_path = Some(per_cpu_other_cpu_time_path);
        self.global_other_cpu_time_csv_path = Some(global_other_cpu_time_path);
    }
    
    /// Get or create per-PID state
    fn get_or_create_pid_state(&mut self, pid: u32) -> &mut PerPidPerCpuState {
        self.per_pid_state.entry(pid).or_insert_with(PerPidPerCpuState::new)
    }
    
    /// Calculate off-CPU times for a context switch in event
    fn calculate_off_cpu_times_for_context_switch_in(
        &self,
        timestamp: u64,
        incoming_pid: u32,
        cpu_id: usize,
    ) -> (u64, u64, u64, u64) {
        // Calculate per-CPU off-CPU time
        let (per_cpu_off_time, per_cpu_other_cpu_time) = if let Some(pid_state) = self.per_pid_state.get(&incoming_pid) {
            if let Some(&last_out_time) = pid_state.last_scheduled_out_time.get(&cpu_id) {
                let off_time = timestamp.saturating_sub(last_out_time);
                let last_global_cpu_time = pid_state.last_scheduled_out_global_cpu_time.get(&cpu_id).copied().expect("global cpu time is written together with last_scheduled_out_time");
                let current_global_cpu_time = self.cpu_time_tracker.get_total_cpu_time();
                let other_cpu_time = current_global_cpu_time.saturating_sub(last_global_cpu_time);
                (off_time, other_cpu_time)
            } else {
                (0, 0) // First time on this CPU
            }
        } else {
            (0, 0) // First time for this PID
        };
        
        // Calculate global off-CPU time
        let (global_off_time, global_other_cpu_time) = if let Some(pid_state) = self.per_pid_state.get(&incoming_pid) {
            if pid_state.last_global_scheduled_out_time > 0 {
                // Check that this is the only thread of this PID (thread count will be 1 after context switch in)
                let thread_count = self.cpu_time_tracker.get_pid_counter(incoming_pid)
                    .map(|counter| counter.current_thread_count);
                
                if Some(1) == thread_count {
                    // This is the only thread, so we can calculate global off-CPU time
                    let off_time = timestamp.saturating_sub(pid_state.last_global_scheduled_out_time);
                    let other_cpu_time = self.cpu_time_tracker.get_total_cpu_time()
                        .saturating_sub(pid_state.last_global_scheduled_out_global_cpu_time);
                    (off_time, other_cpu_time)
                } else {
                    // Process has other threads running, so global off-CPU time is 0
                    (0, 0)
                }
            } else {
                (0, 0) // First time globally
            }
        } else {
            (0, 0) // First time for this PID
        };
        
        (per_cpu_off_time, global_off_time, per_cpu_other_cpu_time, global_other_cpu_time)
    }
    
    /// Update PID state after context switch
    fn update_pid_state_after_context_switch(
        &mut self,
        timestamp: u64,
        outgoing_pid: u32,
        _incoming_pid: u32,
        cpu_id: usize,
    ) {
        let current_global_cpu_time = self.cpu_time_tracker.get_total_cpu_time();
        
        // Check if outgoing PID is now completely off all CPUs
        let outgoing_thread_count = self.cpu_time_tracker.get_or_create_pid_counter(outgoing_pid).current_thread_count;
        
        // Update outgoing PID state
        let outgoing_state = self.get_or_create_pid_state(outgoing_pid);
        outgoing_state.last_scheduled_out_time.insert(cpu_id, timestamp);
        outgoing_state.last_scheduled_out_global_cpu_time.insert(cpu_id, current_global_cpu_time);
        
        if outgoing_thread_count == 0 {
            outgoing_state.last_global_scheduled_out_time = timestamp;
            outgoing_state.last_global_scheduled_out_global_cpu_time = current_global_cpu_time;
        }
    }
    
    /// Process a single event
    fn process_event(
        &mut self,
        timestamp: u64,
        pid: u32,
        cpu_id: usize,
        is_context_switch: bool,
        next_tgid: Option<u32>,
    ) -> Result<(u64, u64, u64, u64)> {
        if cpu_id >= self.num_cpus {
            return Err(anyhow::anyhow!("Invalid CPU ID: {}", cpu_id));
        }
        
        // Get the buffered measurements for this CPU (from previous event)
        let current_measurements = self.buffered_measurements[cpu_id];
        
        // Update CPU time tracker
        if is_context_switch {
            let next_pid = next_tgid.expect("next_tgid should always be present on context switches");
            self.cpu_time_tracker.process_context_switch(
                timestamp,
                cpu_id,
                pid,
                next_pid,
            )?;
            
            // Update PID state after context switch
            self.update_pid_state_after_context_switch(timestamp, pid, next_pid, cpu_id);
            
            // Calculate off-CPU times for the incoming process (next event's measurements)
            let next_measurements = self.calculate_off_cpu_times_for_context_switch_in(
                timestamp, next_pid, cpu_id
            );
            self.buffered_measurements[cpu_id] = next_measurements;
        } else {
            self.cpu_time_tracker.process_timer_event(timestamp, pid, cpu_id)?;
            
            // For timer events, next measurements are all zeros
            self.buffered_measurements[cpu_id] = (0, 0, 0, 0);
        }
        
        Ok(current_measurements)
    }
}

impl Analysis for OffCpuAnalysis {
    fn process_record_batch(&mut self, batch: &RecordBatch) -> Result<Vec<ArrayRef>> {
        let num_rows = batch.num_rows();

        // Extract required columns
        let timestamp_array = batch
            .column_by_name("timestamp")
            .context("Missing timestamp column")?
            .as_any()
            .downcast_ref::<arrow_array::Int64Array>()
            .context("Invalid timestamp column type")?;
        let pid_array = batch
            .column_by_name("pid")
            .context("Missing pid column")?
            .as_any()
            .downcast_ref::<arrow_array::Int32Array>()
            .context("Invalid pid column type")?;
        let cpu_id_array = batch
            .column_by_name("cpu_id")
            .context("Missing cpu_id column")?
            .as_any()
            .downcast_ref::<arrow_array::Int32Array>()
            .context("Invalid cpu_id column type")?;
        let is_context_switch_array = batch
            .column_by_name("is_context_switch")
            .context("Missing is_context_switch column")?
            .as_any()
            .downcast_ref::<arrow_array::BooleanArray>()
            .context("Invalid is_context_switch column type")?;
        let next_tgid_array = batch
            .column_by_name("next_tgid")
            .context("Missing next_tgid column")?
            .as_any()
            .downcast_ref::<arrow_array::Int32Array>()
            .context("Invalid next_tgid column type")?;
        
        // Extract performance metrics columns
        let instructions_array = batch
            .column_by_name("instructions")
            .context("Missing instructions column")?
            .as_any()
            .downcast_ref::<arrow_array::Int64Array>()
            .context("Invalid instructions column type")?;
        let cycles_array = batch
            .column_by_name("cycles")
            .context("Missing cycles column")?
            .as_any()
            .downcast_ref::<arrow_array::Int64Array>()
            .context("Invalid cycles column type")?;
        let process_name_array = batch
            .column_by_name("process_name")
            .context("Missing process_name column")?
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .context("Invalid process_name column type")?;

        // Prepare output arrays for off-CPU metrics
        let mut per_cpu_off_time_ns = Vec::with_capacity(num_rows);
        let mut global_off_time_ns = Vec::with_capacity(num_rows);
        let mut per_cpu_other_cpu_time_ns = Vec::with_capacity(num_rows);
        let mut global_other_cpu_time_ns = Vec::with_capacity(num_rows);

        // Process each row
        for i in 0..num_rows {
            let timestamp = timestamp_array.value(i) as u64;
            let pid = pid_array.value(i) as u32;
            let cpu_id = cpu_id_array.value(i) as usize;
            let is_context_switch = is_context_switch_array.value(i);
            let next_tgid = if next_tgid_array.is_null(i) {
                None
            } else {
                Some(next_tgid_array.value(i) as u32)
            };
            let instructions = instructions_array.value(i) as u64;
            let cycles = cycles_array.value(i) as u64;
            let process_name = process_name_array.value(i);

            let (per_cpu_off_time, global_off_time, per_cpu_other_cpu_time, global_other_cpu_time) =
                self.process_event(timestamp, pid, cpu_id, is_context_switch, next_tgid)?;

            per_cpu_off_time_ns.push(per_cpu_off_time as f64);
            global_off_time_ns.push(global_off_time as f64);
            per_cpu_other_cpu_time_ns.push(per_cpu_other_cpu_time as f64);
            global_other_cpu_time_ns.push(global_other_cpu_time as f64);
            
            // Calculate CPI and update statistics if we have valid data
            if instructions > 0 && cycles > 0 {
                let cpi = cycles as f64 / instructions as f64;
                let process_name_key = process_name.to_string();
                
                // Get or create statistics for this process name
                // Using 100 microsecond bins (1e5 ns = 100μs bin width), capped at 4ms (4e6 ns)
                let per_cpu_off_time_stats = self.per_process_per_cpu_off_time_stats.entry(process_name_key.clone()).or_insert_with(|| {
                    OffCpuCpiStatistics::new(1e5, 0.2, 4.0, 4e6) // 100μs bins, 0.2 CPI bins, max CPI 4.0, max measurement 4ms
                });
                let global_off_time_stats = self.per_process_global_off_time_stats.entry(process_name_key.clone()).or_insert_with(|| {
                    OffCpuCpiStatistics::new(1e5, 0.2, 4.0, 4e6)
                });
                let per_cpu_other_cpu_time_stats = self.per_process_per_cpu_other_cpu_time_stats.entry(process_name_key.clone()).or_insert_with(|| {
                    OffCpuCpiStatistics::new(1e5, 0.2, 4.0, 4e6) // 100μs bins for CPU time, capped at 4ms
                });
                let global_other_cpu_time_stats = self.per_process_global_other_cpu_time_stats.entry(process_name_key).or_insert_with(|| {
                    OffCpuCpiStatistics::new(1e5, 0.2, 4.0, 4e6)
                });
                
                // Add measurements to statistics only when there's a scheduling gap (non-zero off-CPU times)
                if per_cpu_off_time > 0 {
                    per_cpu_off_time_stats.add_measurement(per_cpu_off_time as f64, cpi, instructions);
                }
                if global_off_time > 0 {
                    global_off_time_stats.add_measurement(global_off_time as f64, cpi, instructions);
                }
                if per_cpu_other_cpu_time > 0 {
                    per_cpu_other_cpu_time_stats.add_measurement(per_cpu_other_cpu_time as f64, cpi, instructions);
                }
                if global_other_cpu_time > 0 {
                    global_other_cpu_time_stats.add_measurement(global_other_cpu_time as f64, cpi, instructions);
                }
            }
        }

        // Return new columns as ArrayRef
        Ok(vec![
            Arc::new(Float64Array::from(per_cpu_off_time_ns)),
            Arc::new(Float64Array::from(global_off_time_ns)),
            Arc::new(Float64Array::from(per_cpu_other_cpu_time_ns)),
            Arc::new(Float64Array::from(global_other_cpu_time_ns)),
        ])
    }

    fn new_columns_schema(&self) -> Vec<Arc<Field>> {
        vec![
            Arc::new(Field::new("per_cpu_off_time_ns", DataType::Float64, false)),
            Arc::new(Field::new("global_off_time_ns", DataType::Float64, false)),
            Arc::new(Field::new("per_cpu_other_cpu_time_ns", DataType::Float64, false)),
            Arc::new(Field::new("global_other_cpu_time_ns", DataType::Float64, false)),
        ]
    }
    
    fn finalize(&self) -> Result<()> {
        // Export CSV files if paths are set
        if let Some(path) = &self.per_cpu_off_time_csv_path {
            println!("Exporting per-CPU off-time statistics to: {}", path);
            self.export_per_cpu_off_time_csv(path)?;
        }
        
        if let Some(path) = &self.global_off_time_csv_path {
            println!("Exporting global off-time statistics to: {}", path);
            self.export_global_off_time_csv(path)?;
        }
        
        if let Some(path) = &self.per_cpu_other_cpu_time_csv_path {
            println!("Exporting per-CPU other CPU time statistics to: {}", path);
            self.export_per_cpu_other_cpu_time_csv(path)?;
        }
        
        if let Some(path) = &self.global_other_cpu_time_csv_path {
            println!("Exporting global other CPU time statistics to: {}", path);
            self.export_global_other_cpu_time_csv(path)?;
        }
        
        Ok(())
    }
}

impl OffCpuAnalysis {
    /// Export per-CPU off-time statistics to CSV
    pub fn export_per_cpu_off_time_csv(&self, file_path: &str) -> Result<()> {
        let mut file = File::create(file_path)?;
        writeln!(file, "process_name,off_time_min,off_time_max,cpi_min,cpi_max,instructions")?;
        
        for (process_name, stats) in tqdm(&self.per_process_per_cpu_off_time_stats).desc(Some("Exporting per-CPU off-time CSV")) {
            stats.export_to_csv(&mut file, process_name)?;
        }
        
        Ok(())
    }
    
    /// Export global off-time statistics to CSV
    pub fn export_global_off_time_csv(&self, file_path: &str) -> Result<()> {
        let mut file = File::create(file_path)?;
        writeln!(file, "process_name,off_time_min,off_time_max,cpi_min,cpi_max,instructions")?;
        
        for (process_name, stats) in tqdm(&self.per_process_global_off_time_stats).desc(Some("Exporting global off-time CSV")) {
            stats.export_to_csv(&mut file, process_name)?;
        }
        
        Ok(())
    }
    
    /// Export per-CPU other CPU time statistics to CSV
    pub fn export_per_cpu_other_cpu_time_csv(&self, file_path: &str) -> Result<()> {
        let mut file = File::create(file_path)?;
        writeln!(file, "process_name,cpu_time_min,cpu_time_max,cpi_min,cpi_max,instructions")?;
        
        for (process_name, stats) in tqdm(&self.per_process_per_cpu_other_cpu_time_stats).desc(Some("Exporting per-CPU other CPU time CSV")) {
            stats.export_to_csv(&mut file, process_name)?;
        }
        
        Ok(())
    }
    
    /// Export global other CPU time statistics to CSV
    pub fn export_global_other_cpu_time_csv(&self, file_path: &str) -> Result<()> {
        let mut file = File::create(file_path)?;
        writeln!(file, "process_name,cpu_time_min,cpu_time_max,cpi_min,cpi_max,instructions")?;
        
        for (process_name, stats) in tqdm(&self.per_process_global_other_cpu_time_stats).desc(Some("Exporting global other CPU time CSV")) {
            stats.export_to_csv(&mut file, process_name)?;
        }
        
        Ok(())
    }
}