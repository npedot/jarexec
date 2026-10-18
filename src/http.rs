//! Client HTTP e barra di avanzamento condivisi da download di dipendenze e JRE.

use std::time::Duration;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("jarexec/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        // nel client bloccante vale per ogni lettura del body: è un timeout di inattività
        .timeout(Duration::from_secs(60))
        .build()?)
}

/// Barra sui byte totali; nascosta se non c'è niente da trasferire o se stderr non è un terminale.
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
