//! Logging configuration module

use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::format::Writer;

pub struct SimpleTime;

impl FormatTime for SimpleTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let now = chrono::Local::now();
        write!(w, "{}", now.format("%H:%M:%S"))
    }
}

pub fn init() {
    use tracing_subscriber::EnvFilter;
    
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    
    tracing_subscriber::fmt()
        .with_timer(SimpleTime)
        .with_target(false)
        .with_env_filter(filter)
        .init();
}
