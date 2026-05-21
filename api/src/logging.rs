use std::fmt;
#[cfg(feature = "verbose-logging")]
use std::sync::OnceLock;
#[cfg(feature = "verbose-logging")]
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Request,
    IoUring,
}

#[cfg(feature = "verbose-logging")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Text,
    Json,
}

#[cfg(feature = "verbose-logging")]
pub type Timer = Instant;
#[cfg(not(feature = "verbose-logging"))]
pub type Timer = ();

#[inline(always)]
pub fn timer_start() -> Timer {
    #[cfg(feature = "verbose-logging")]
    {
        Instant::now()
    }
    #[cfg(not(feature = "verbose-logging"))]
    {
        ()
    }
}

#[cfg(feature = "verbose-logging")]
#[inline(always)]
fn level_str(level: Level) -> &'static str {
    match level {
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARN",
    }
}

#[cfg(feature = "verbose-logging")]
#[inline(always)]
fn category_str(category: Category) -> &'static str {
    match category {
        Category::Request => "request",
        Category::IoUring => "iouring",
    }
}

#[cfg(feature = "verbose-logging")]
#[inline(always)]
fn transport_from_env(value: Option<&str>) -> Transport {
    match value {
        Some(v) if v.eq_ignore_ascii_case("json") => Transport::Json,
        _ => Transport::Text,
    }
}

#[cfg(feature = "verbose-logging")]
#[inline(always)]
fn transport() -> Transport {
    static TRANSPORT: OnceLock<Transport> = OnceLock::new();
    *TRANSPORT.get_or_init(|| transport_from_env(std::env::var("LOG_TRANSPORT").ok().as_deref()))
}

#[cfg(feature = "verbose-logging")]
#[inline(always)]
fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(feature = "verbose-logging")]
fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let code = c as u32;
                out.push_str("\\u00");
                out.push(HEX[((code >> 4) & 0x0f) as usize] as char);
                out.push(HEX[(code & 0x0f) as usize] as char);
            }
            _ => out.push(ch),
        }
    }
    out.push('"');
}

#[cfg(feature = "verbose-logging")]
fn format_json_log(level: Level, category: Category, msg: &str) -> String {
    let mut line = String::with_capacity(192 + msg.len());
    line.push('{');
    line.push_str("\"ts\":");
    line.push_str(&now_unix_ms().to_string());
    line.push_str(",\"event\":\"log\"");
    line.push_str(",\"level\":\"");
    line.push_str(level_str(level));
    line.push_str("\",\"category\":\"");
    line.push_str(category_str(category));
    line.push_str("\",\"message\":");
    push_json_string(&mut line, msg);
    line.push('}');
    line
}

#[cfg(feature = "verbose-logging")]
fn format_json_timing(
    level: Level,
    category: Category,
    op: &str,
    elapsed_us: u128,
    elapsed_ms: u128,
    context: &str,
) -> String {
    let mut line = String::with_capacity(256 + op.len() + context.len());
    line.push('{');
    line.push_str("\"ts\":");
    line.push_str(&now_unix_ms().to_string());
    line.push_str(",\"event\":\"timing\"");
    line.push_str(",\"level\":\"");
    line.push_str(level_str(level));
    line.push_str("\",\"category\":\"");
    line.push_str(category_str(category));
    line.push_str("\",\"op\":");
    push_json_string(&mut line, op);
    line.push_str(",\"elapsed_us\":");
    line.push_str(&elapsed_us.to_string());
    line.push_str(",\"elapsed_ms\":");
    line.push_str(&elapsed_ms.to_string());
    line.push_str(",\"context\":");
    push_json_string(&mut line, context);
    line.push('}');
    line
}

#[cfg(feature = "verbose-logging")]
fn format_log_line(transport: Transport, level: Level, category: Category, msg: &str) -> String {
    match transport {
        Transport::Text => format!(
            "[{}] [{}] {}",
            level_str(level),
            category_str(category),
            msg
        ),
        Transport::Json => format_json_log(level, category, msg),
    }
}

#[cfg(feature = "verbose-logging")]
fn format_timing_line(
    transport: Transport,
    level: Level,
    category: Category,
    op: &str,
    elapsed_us: u128,
    elapsed_ms: u128,
    context: &str,
) -> String {
    match transport {
        Transport::Text => format!(
            "[{}] [{}] op={} elapsed_us={} elapsed_ms={} {}",
            level_str(level),
            category_str(category),
            op,
            elapsed_us,
            elapsed_ms,
            context
        ),
        Transport::Json => format_json_timing(level, category, op, elapsed_us, elapsed_ms, context),
    }
}

#[macro_export]
macro_rules! api_log {
    ($level:expr, $category:expr, $msg:expr) => {
        #[cfg(feature = "verbose-logging")]
        {
            $crate::logging::_log($level, $category, $msg);
        }
    };
    ($level:expr, $category:expr, $($arg:tt)*) => {
        #[cfg(feature = "verbose-logging")]
        {
            $crate::logging::_log($level, $category, &format!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! api_log_timing {
    ($level:expr, $category:expr, $op:expr, $started_at:expr, $($arg:tt)*) => {
        #[cfg(feature = "verbose-logging")]
        {
            $crate::logging::_log_timing($level, $category, $op, $started_at, format_args!($($arg)*));
        }
    };
}

#[doc(hidden)]
pub fn _log(level: Level, category: Category, msg: &str) {
    #[cfg(feature = "verbose-logging")]
    {
        eprintln!("{}", format_log_line(transport(), level, category, msg));
    }
    #[cfg(not(feature = "verbose-logging"))]
    {
        let _ = (level, category, msg);
    }
}

#[doc(hidden)]
pub fn _log_timing(
    level: Level,
    category: Category,
    op: &str,
    started_at: Timer,
    context: fmt::Arguments<'_>,
) {
    #[cfg(feature = "verbose-logging")]
    {
        let elapsed = started_at.elapsed();
        eprintln!(
            "{}",
            format_timing_line(
                transport(),
                level,
                category,
                op,
                elapsed.as_micros(),
                elapsed.as_millis(),
                &context.to_string(),
            )
        );
    }
    #[cfg(not(feature = "verbose-logging"))]
    {
        let _ = (level, category, op, started_at, context);
    }
}

#[cfg(all(test, feature = "verbose-logging"))]
mod tests {
    use super::{
        Category, Level, Transport, format_json_log, format_json_timing, format_log_line,
        format_timing_line, transport_from_env,
    };

    #[test]
    fn json_log_contains_core_fields() {
        let line = format_json_log(Level::Info, Category::Request, "hello world");
        assert!(line.starts_with('{'));
        assert!(line.contains("\"event\":\"log\""));
        assert!(line.contains("\"level\":\"INFO\""));
        assert!(line.contains("\"category\":\"request\""));
        assert!(line.contains("\"message\":\"hello world\""));
    }

    #[test]
    fn json_timing_contains_timing_fields() {
        let line = format_json_timing(
            Level::Debug,
            Category::IoUring,
            "send_fd_total",
            123,
            0,
            "fd=7 retries=1 result=ok",
        );
        assert!(line.contains("\"event\":\"timing\""));
        assert!(line.contains("\"op\":\"send_fd_total\""));
        assert!(line.contains("\"elapsed_us\":123"));
        assert!(line.contains("\"elapsed_ms\":0"));
        assert!(line.contains("\"context\":\"fd=7 retries=1 result=ok\""));
    }

    #[test]
    fn text_formatters_match_human_readable_output() {
        let line = format_log_line(Transport::Text, Level::Info, Category::Request, "hello");
        assert_eq!(line, "[INFO] [request] hello");

        let timing = format_timing_line(
            Transport::Text,
            Level::Warn,
            Category::IoUring,
            "send_fd_total",
            12,
            0,
            "fd=7",
        );
        assert_eq!(
            timing,
            "[WARN] [iouring] op=send_fd_total elapsed_us=12 elapsed_ms=0 fd=7"
        );
    }

    #[test]
    fn transport_selection_accepts_json_only() {
        assert_eq!(transport_from_env(None), Transport::Text);
        assert_eq!(transport_from_env(Some("text")), Transport::Text);
        assert_eq!(transport_from_env(Some("json")), Transport::Json);
        assert_eq!(transport_from_env(Some("both")), Transport::Text);
    }
}
