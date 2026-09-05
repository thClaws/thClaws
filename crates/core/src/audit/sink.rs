//! Sink contract. Every sink is fail-open: `write` returns `Err` and the
//! caller counts a drop; nothing here may block a tool call.

pub trait AuditSink: Send + Sync {
    fn name(&self) -> &'static str;
    fn write(&self, line: &str) -> Result<(), String>;
    /// Push any buffered records now (process exit, session end).
    fn flush(&self) {}
    /// Records lost inside the sink after `write` accepted them (e.g. a
    /// failed batch POST). Added to the registry's write-failure count.
    fn dropped_extra(&self) -> u64 {
        0
    }
}
