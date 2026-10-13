use ssh_monitor::app::{self, Headless, Session};
use ssh_monitor::cli::{self, Parsed};
use ssh_monitor::clock::{RealClock, local_utc_offset_secs};
use ssh_monitor::csv::CsvLog;
use ssh_monitor::runner::{CommandRunner, RecordingRunner, TokioRunner};
use ssh_monitor::{config, snapshot};
use std::process::ExitCode;
use std::sync::Arc;

const EXIT_ERROR: u8 = 3;

fn main() -> ExitCode {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(Parsed::Run(args)) => args,
        Ok(Parsed::Help) => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("ssh-monitor {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("ssh-monitor: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    // Must happen before the runtime spawns threads (see `clock::local_utc_offset_secs`).
    let utc_offset_secs = local_utc_offset_secs();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ssh-monitor: cannot start runtime: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    match runtime.block_on(run(args, utc_offset_secs)) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("ssh-monitor: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

async fn run(args: cli::Args, utc_offset_secs: i32) -> anyhow::Result<u8> {
    let mut runner: Arc<dyn CommandRunner> = Arc::new(TokioRunner);
    if let Some(dir) = &args.record {
        runner = Arc::new(RecordingRunner::new(runner, dir)?);
    }
    let host = config::resolve(runner.as_ref(), &args.host).await?;
    let csv = match &args.log {
        Some(path) => Some(CsvLog::open(path)?),
        None => None,
    };
    let session = Session {
        host,
        runner,
        clock: Arc::new(RealClock::new()),
        interval: args.tick(),
        csv,
        utc_offset_secs,
    };
    if args.once {
        let clock = Arc::clone(&session.clock);
        let monitor = app::run(session, &mut Headless, None, Some(app::ONCE_SAMPLE)).await?;
        let doc = snapshot::json(&monitor, clock.now());
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(snapshot::exit_code(monitor.known_state()));
    }
    ssh_monitor::ui::tui::run(session).await
}
