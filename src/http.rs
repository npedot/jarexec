//! HTTP client and progress bar shared by dependency and JRE downloads.

use std::time::Duration;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("jarexec/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        // in the blocking client this applies to each body read: it is an idle timeout
        .timeout(Duration::from_secs(60))
        .build()?)
}

/// Bar over total bytes; hidden when there is nothing to transfer or stderr is not a terminal.
pub fn progress_bar(total: u64, label: &str) -> ProgressBar {
    if total == 0 {
        return ProgressBar::hidden();
    }
    ProgressBar::new(total).with_style(
        ProgressStyle::with_template(&format!(
            "{{spinner}} {label} {{bytes}}/{{total_bytes}} [{{bar:30}}] {{bytes_per_sec}}"
        ))
        .expect("valid template")
        .progress_chars("=> "),
    )
}
