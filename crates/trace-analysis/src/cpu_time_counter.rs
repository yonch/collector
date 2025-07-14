/// CPU time counter for tracking aggregate CPU time and thread counts
#[derive(Debug, Clone)]
pub struct CpuTimeCounter {
    aggregate_cpu_time: u64,
    last_update_timestamp: u64,
    pub current_thread_count: u32,
}

impl CpuTimeCounter {
    /// Create a new CPU time counter
    pub fn new() -> Self {
        Self {
            aggregate_cpu_time: 0,
            last_update_timestamp: 0,
            current_thread_count: 0,
        }
    }

    /// Increment the current thread count
    pub fn increase(&mut self) {
        self.current_thread_count += 1;
    }

    /// Decrement the current thread count
    pub fn decrease(&mut self) {
        if self.current_thread_count == 0 {
            panic!("Current thread count is 0");
        }
        self.current_thread_count -= 1;
    }

    /// Update aggregate CPU time with elapsed time since last update
    pub fn update(&mut self, timestamp: u64) {
        if self.last_update_timestamp > 0 {
            if timestamp < self.last_update_timestamp {
                panic!(
                    "Timestamp regression detected: {} < {}",
                    timestamp, self.last_update_timestamp
                );
            }
            let elapsed_time = timestamp - self.last_update_timestamp;
            self.aggregate_cpu_time += elapsed_time * (self.current_thread_count as u64);
        }
        self.last_update_timestamp = timestamp;
    }

    /// Get current aggregate CPU time in nanoseconds
    pub fn get_ns(&self) -> u64 {
        self.aggregate_cpu_time
    }
}