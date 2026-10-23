//! Test-only log capture that is safe under the parallel test runner.
//!
//! `tracing::subscriber::with_default` installs a *scoped* dispatcher, and
//! tracing caches each callsite's interest across every live dispatcher. When
//! another test thread hits the same callsite while no capturing dispatcher is
//! alive, the callsite can be cached as "never" and the capture misses the
//! event (seen as an intermittent failure of
//! `stream_relay_emits_feedback_runtime_terminal_log`).
//!
//! Instead, one process-wide subscriber is installed once, and its filter
//! never lets a callsite be cached as enabled or disabled. A per-layer filter
//! only enables events on a thread that is currently capturing, and the
//! writer appends to that thread's buffer, so tests stay isolated from each
//! other without being serialised.

use std::cell::RefCell;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::subscriber::Interest;
use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, Filter, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

struct ActiveCapture {
    max_level: Level,
    buffer: Arc<Mutex<Vec<u8>>>,
}

thread_local! {
    static ACTIVE: RefCell<Option<ActiveCapture>> = const { RefCell::new(None) };
}

struct ThreadLocalWriter;

impl Write for ThreadLocalWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        ACTIVE.with(|active| {
            if let Some(capture) = active.borrow().as_ref() {
                capture.buffer.lock().expect("log buffer lock").extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct MakeThreadLocalWriter;

impl<'a> MakeWriter<'a> for MakeThreadLocalWriter {
    type Writer = ThreadLocalWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ThreadLocalWriter
    }
}

/// Enables an event only on a thread that is currently capturing. The
/// callsite interest is always `sometimes`: the default `Filter` behaviour
/// evaluates `enabled` once at registration, on whichever thread first hits
/// the callsite, and would cache "never" for every test after it.
struct CapturingThreadFilter;

impl<S> Filter<S> for CapturingThreadFilter {
    fn enabled(&self, metadata: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        ACTIVE.with(|active| {
            active
                .borrow()
                .as_ref()
                .is_some_and(|capture| *metadata.level() <= capture.max_level)
        })
    }

    fn callsite_enabled(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }
}

fn install_global_subscriber() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(MakeThreadLocalWriter)
            .with_ansi(false)
            .with_filter(CapturingThreadFilter);
        tracing::subscriber::set_global_default(Registry::default().with(layer))
            .expect("test log capture must own the global subscriber");
    });
}

/// Run `f` on the current thread and return the formatted log lines it
/// emitted at `max_level` or more severe. Events from other threads (other
/// tests, or tasks spawned onto a multi-thread runtime) are not captured.
pub(crate) fn capture_logs(max_level: Level, f: impl FnOnce()) -> String {
    install_global_subscriber();
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.borrow_mut().take());
        }
    }

    ACTIVE.with(|active| {
        *active.borrow_mut() = Some(ActiveCapture {
            max_level,
            buffer: Arc::clone(&buffer),
        });
    });
    let reset = Reset;
    f();
    drop(reset);

    String::from_utf8(buffer.lock().expect("log buffer lock").clone()).expect("utf8 logs")
}
