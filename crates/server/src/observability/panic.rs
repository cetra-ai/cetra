//! The panic hook of `cetra serve` (track TL, design § Panics).
//!
//! It chains Rust's default hook, so stderr keeps the message as before, then emits one ERROR
//! event `panic` with the code location. The message is never recorded: its text can hold data
//! (`unwrap()` prints the error's `Debug`; a bad string slice prints the string). Task panics
//! that tokio catches go through the same hook.

use std::cell::Cell;
use std::panic::PanicHookInfo;
use std::sync::Once;

/// The target of the `panic` event, this module's path. Track TL M5's crash layer skips events
/// with it, so a panic is reported once.
pub(crate) const PANIC_EVENT_TARGET: &str = "cetra_server::observability::panic";

thread_local! {
    /// Set while this thread runs the hook: a panic inside the hook (a layer that panics)
    /// skips the event instead of recursing.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// Installs the hook. Only the first call in a process does anything, so the default hook is
/// chained once. `cetra serve` calls it before it loads the config.
pub fn install() {
    // The crash reports' uptime counts from here (track TL M5); `serve::bind` marks it too.
    crate::observability::facts::mark_process_start();
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            default(info);
            report(info);
        }));
    });
}

/// What the hook adds to the default hook: the crash report when crash reports are on (track TL M5), then the `panic` event.
fn report(info: &PanicHookInfo<'_>) {
    let Some(_guard) = HookGuard::enter() else {
        return;
    };
    // Opt-in crash report first (track TL M5): the event below goes through every layer, which
    // may block on a lock the panicking thread holds; the spool is what must survive.
    // Nothing happens until `subscriber::init` enabled it.
    crate::observability::crash::report_panic(info);
    let location = panic_location(info);
    tracing::error!(target: PANIC_EVENT_TARGET, location = %location, "panic");
}

/// `file:line:column` of the panic, or `unknown`.
pub(crate) fn panic_location(info: &PanicHookInfo<'_>) -> String {
    info.location().map_or_else(
        || "unknown".to_owned(),
        |location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        },
    )
}

/// Marks this thread as inside the hook until dropped.
struct HookGuard;

impl HookGuard {
    /// `None` when this thread is already inside the hook, or when its thread-local storage is
    /// gone (a panic during thread exit).
    fn enter() -> Option<Self> {
        let entered = IN_HOOK
            .try_with(|flag| !flag.replace(true))
            .unwrap_or(false);
        entered.then_some(Self)
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        let _ = IN_HOOK.try_with(|flag| flag.set(false));
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    /// A writer that appends to a shared buffer, the shape of the CLI test's `Buffer`.
    #[derive(Clone)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("log buffer poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_panic_target_is_this_module() {
        assert_eq!(
            PANIC_EVENT_TARGET,
            module_path!().trim_end_matches("::tests")
        );
    }

    #[test]
    fn hook_guard_refuses_a_second_entry() {
        let first = HookGuard::enter();
        assert!(first.is_some());
        assert!(HookGuard::enter().is_none());
        drop(first);
        assert!(HookGuard::enter().is_some());
    }

    #[test]
    fn a_panic_emits_an_error_event_with_the_location_only() {
        crate::test_support::ensure_global_subscriber();
        install();
        install();

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = Buffer(Arc::clone(&buffer));
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_target(true);
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(layer));
        let outcome = {
            let _guard = tracing::dispatcher::set_default(&dispatch);
            std::panic::catch_unwind(|| {
                panic!("sk-cetra-planted0000000000000000000000000 user@example.com prompt text")
            })
        };
        assert!(outcome.is_err());
        drop(dispatch);

        let bytes = buffer.lock().expect("test buffer").clone();
        let logs = String::from_utf8_lossy(&bytes);
        assert!(logs.contains("ERROR"), "{logs}");
        assert!(
            logs.contains("cetra_server::observability::panic"),
            "{logs}"
        );
        assert!(logs.contains("panic"), "{logs}");
        assert!(logs.contains("location="), "{logs}");
        assert!(logs.contains("observability/panic.rs"), "{logs}");
        assert!(!logs.contains("sk-cetra-planted"), "{logs}");
        assert!(!logs.contains("user@example.com"), "{logs}");
        assert!(!logs.contains("prompt text"), "{logs}");
        assert_eq!(logs.matches("location=").count(), 1, "{logs}");
    }
}
