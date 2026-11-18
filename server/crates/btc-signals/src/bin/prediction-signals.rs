//! `prediction-signals` — the prediction lane CLI. The runner itself lives in
//! the library (`btc_signals::prediction_run`) so the exchange can drive it
//! in-process.

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    btc_signals::prediction_run::cli_main().await
}
