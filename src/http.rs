//! HTTP client and progress reporting shared by dependency and JRE downloads.

use std::cell::RefCell;
use std::sync::Arc;
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

/// Receives the progress of transfers instead of the terminal bar, e.g. to draw it in a GUI.
/// `inc` may be called from several threads at once.
pub trait ProgressSink: Send + Sync {
    /// A transfer of `total` bytes starts (dependencies of one application, a JRE, …).
    fn start(&self, label: &str, total: u64);
    fn inc(&self, bytes: u64);
    fn finish(&self);
}

thread_local! {
    static SINK: RefCell<Option<Arc<dyn ProgressSink>>> = const { RefCell::new(None) };
}

/// Runs `f` with every progress started on this thread reported to `sink`.
pub fn report_progress_to<R>(sink: Arc<dyn ProgressSink>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Arc<dyn ProgressSink>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SINK.with(|s| *s.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(SINK.with(|s| s.borrow_mut().replace(sink)));
    f()
}

/// Progress of one transfer: a terminal bar, or the sink set by [`report_progress_to`].
pub enum Progress {
    Bar(ProgressBar),
    Sink(Arc<dyn ProgressSink>),
}

impl Progress {
    pub fn inc(&self, bytes: u64) {
        match self {
            Progress::Bar(bar) => bar.inc(bytes),
            Progress::Sink(sink) => sink.inc(bytes),
        }
    }

    pub fn finish_and_clear(&self) {
        match self {
            Progress::Bar(bar) => bar.finish_and_clear(),
            Progress::Sink(sink) => sink.finish(),
        }
    }
}

/// Progress over total bytes. Without a sink it is a terminal bar, hidden when there is
/// nothing to transfer or stderr is not a terminal.
pub fn progress_bar(total: u64, label: &str) -> Progress {
    if let Some(sink) = SINK.with(|s| s.borrow().clone()) {
        sink.start(label, total);
        return Progress::Sink(sink);
    }
    if total == 0 {
        return Progress::Bar(ProgressBar::hidden());
    }
    Progress::Bar(
        ProgressBar::new(total).with_style(
            ProgressStyle::with_template(&format!(
                "{{spinner}} {label} {{bytes}}/{{total_bytes}} [{{bar:30}}] {{bytes_per_sec}}"
            ))
            .expect("valid template")
            .progress_chars("=> "),
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl ProgressSink for Recorder {
        fn start(&self, label: &str, total: u64) {
            self.0
                .lock()
                .unwrap()
                .push(format!("start {label} {total}"));
        }
        fn inc(&self, bytes: u64) {
            self.0.lock().unwrap().push(format!("inc {bytes}"));
        }
        fn finish(&self) {
            self.0.lock().unwrap().push("finish".into());
        }
    }

    #[test]
    fn sink_receives_progress_only_inside_its_scope() {
        let recorder = Arc::new(Recorder::default());
        report_progress_to(recorder.clone(), || {
            let p = progress_bar(10, "JRE");
            p.inc(4);
            p.inc(6);
            p.finish_and_clear();
        });
        assert!(matches!(progress_bar(10, "JRE"), Progress::Bar(_)));
        assert_eq!(
            *recorder.0.lock().unwrap(),
            ["start JRE 10", "inc 4", "inc 6", "finish"]
        );
    }
}
