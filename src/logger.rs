use eyre::{Result, WrapErr};

pub fn init() -> Result<()> {
    env_logger::Builder::from_default_env()
        .format(|buf, record| {
            use std::io::Write;

            let level = record.level();
            let color = match level {
                log::Level::Error => "\x1b[31;1m",
                log::Level::Warn => "\x1b[33m",
                log::Level::Info => "\x1b[32m",
                log::Level::Debug => "\x1b[34m",
                log::Level::Trace => "\x1b[35m",
            };
            let reset = "\x1b[0m";

            writeln!(
                buf,
                "[{}] {}{:5}{} [{}] {}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S"),
                color,
                level,
                reset,
                record.target(),
                record.args()
            )
        })
        .try_init()
        .wrap_err("failed to initialize logger")?;

    Ok(())
}
