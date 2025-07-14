use anyhow::Result;
use std::collections::HashMap;

use crate::cpu_time_counter::CpuTimeCounter;

/// Per-CPU state for tracking current PID and last update timestamp
#[derive(Debug)]
pub struct PerCpuState {
    pub current_pid: Option<u32>,
    pub last_timestamp: u64,
}

impl PerCpuState {
    pub fn new() -> Self {
        Self {
            current_pid: None,
            last_timestamp: 0,
        }
    }
}

/// CPU time tracker that maintains state for both concurrency and off-CPU analysis
pub struct CpuTimeTracker {
    num_cpus: usize,
    
    // Per-PID CPU time counters
    per_pid_counters: HashMap<u32, CpuTimeCounter>,
    
    // Global CPU time counter for non-kernel threads
    total_counter: CpuTimeCounter,
    
    // Per-CPU state tracking
    per_cpu_state: Vec<PerCpuState>,
}

impl CpuTimeTracker {
    /// Create a new CPU time tracker
    pub fn new(num_cpus: usize) -> Self {
        Self {
            num_cpus,
            per_pid_counters: HashMap::new(),
            total_counter: CpuTimeCounter::new(),
            per_cpu_state: (0..num_cpus).map(|_| PerCpuState::new()).collect(),
        }
    }
    
    /// Check if a process ID represents a kernel thread
    pub fn is_kernel(pid: u32) -> bool {
        pid == 0
    }
    
    /// Get or create a counter for the given PID
    pub fn get_or_create_pid_counter(&mut self, pid: u32) -> &mut CpuTimeCounter {
        self.per_pid_counters.entry(pid).or_insert_with(CpuTimeCounter::new)
    }
    
    /// Get per-CPU state
    pub fn get_per_cpu_state(&mut self, cpu_id: usize) -> &mut PerCpuState {
        &mut self.per_cpu_state[cpu_id]
    }
    
    /// Get current aggregate CPU time for a PID
    pub fn get_pid_cpu_time(&self, pid: u32) -> u64 {
        self.per_pid_counters.get(&pid).map_or(0, |counter| counter.get_ns())
    }
    
    /// Get PID counter without creating it
    pub fn get_pid_counter(&self, pid: u32) -> Option<&CpuTimeCounter> {
        self.per_pid_counters.get(&pid)
    }
    
    /// Get current total CPU time
    pub fn get_total_cpu_time(&self) -> u64 {
        self.total_counter.get_ns()
    }
    
    /// Process a context switch event
    pub fn process_context_switch(
        &mut self,
        timestamp: u64,
        cpu_id: usize,
        outgoing_pid: u32,
        incoming_pid: u32,
    ) -> Result<()> {
        if cpu_id >= self.num_cpus {
            return Err(anyhow::anyhow!("Invalid CPU ID: {}", cpu_id));
        }
        
        // Sanity check: if current_pid is set, it should match the outgoing PID
        if let Some(current_pid) = self.per_cpu_state[cpu_id].current_pid {
            if current_pid != outgoing_pid {
                panic!(
                    "Current PID mismatch on CPU {}: expected {}, got {}",
                    cpu_id, current_pid, outgoing_pid
                );
            }
        }
        
        // Update total counter
        self.total_counter.update(timestamp);
        
        // Get or create counters for both PIDs
        let outgoing_counter = self.get_or_create_pid_counter(outgoing_pid);
        outgoing_counter.update(timestamp);
        
        let incoming_counter = self.get_or_create_pid_counter(incoming_pid);
        incoming_counter.update(timestamp);
        
        // Only decrease counters if current_pid is set (we've seen a context switch before)
        if self.per_cpu_state[cpu_id].current_pid.is_some() {
            // Decrease counter for outgoing process
            let outgoing_counter = self.get_or_create_pid_counter(outgoing_pid);
            outgoing_counter.decrease();
            if !Self::is_kernel(outgoing_pid) {
                self.total_counter.decrease();
            }
        }
        
        // Increase counter for incoming process
        let incoming_counter = self.get_or_create_pid_counter(incoming_pid);
        incoming_counter.increase();
        if !Self::is_kernel(incoming_pid) {
            self.total_counter.increase();
        }
        
        // Update per-CPU state
        self.per_cpu_state[cpu_id].current_pid = Some(incoming_pid);
        self.per_cpu_state[cpu_id].last_timestamp = timestamp;
        
        Ok(())
    }
    
    /// Process a timer event (non-context switch)
    pub fn process_timer_event(&mut self, timestamp: u64, pid: u32, cpu_id: usize) -> Result<()> {
        if cpu_id >= self.num_cpus {
            return Err(anyhow::anyhow!("Invalid CPU ID: {}", cpu_id));
        }
        
        // Update counters
        self.total_counter.update(timestamp);
        let pid_counter = self.get_or_create_pid_counter(pid);
        pid_counter.update(timestamp);
        
        // Update per-CPU state timestamp
        self.per_cpu_state[cpu_id].last_timestamp = timestamp;
        
        Ok(())
    }
}