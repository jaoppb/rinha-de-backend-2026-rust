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

pub fn log(level: Level, category: Category, msg: &str) {
    #[cfg(feature = "verbose-logging")]
    {
        let level_str = match level {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
        };
        let cat_str = match category {
            Category::Request => "request",
            Category::IoUring => "iouring",
        };
        eprintln!("[{}] [{}] {}", level_str, cat_str, msg);
    }
    #[cfg(not(feature = "verbose-logging"))]
    {
        let _ = (level, category, msg);
    }
}

