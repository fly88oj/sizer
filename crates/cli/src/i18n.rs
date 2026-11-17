//! Lightweight i18n facade for the Sizer CLI.
//!
//! Uses the `gettext` crate to load translation catalogs (MO files) at runtime.
//! When no catalog is available (the common case during development or when the
//! install did not ship translations), `tr()` simply returns the English source
//! string unchanged — so all user-facing text stays English out of the box.
//!
//! To enable translations, place compiled MO files under:
//!   <install-prefix>/share/locale/<lang>/LC_MESSAGES/sizer.mo
//!
//! The domain name is `"sizer"`; see the GNU gettext documentation for how to
//! extract strings from source and compile PO -> MO.

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use gettext::Catalog;

/// Domain name used when loading MO catalogs.
pub const DOMAIN: &str = "sizer";

/// Environment variable that, when set, overrides the default catalog search path.
pub const LOCALE_DIR_ENV: &str = "SIZER_LOCALE_DIR";

/// Global catalog reference — `None` means "use source strings as-is".
static CATALOG: OnceLock<RwLock<Option<Catalog>>> = OnceLock::new();

/// Returns a reference to the global catalog lock, initializing it on first use.
fn catalog_lock() -> &'static RwLock<Option<Catalog>> {
    CATALOG.get_or_init(|| RwLock::new(None))
}

/// Attempts to locate the compiled MO catalog on disk.
///
/// Search order:
///   1. `$SIZER_LOCALE_DIR/<locale>/LC_MESSAGES/sizer.mo` (override)
///   2. `<prefix>/share/locale/<locale>/LC_MESSAGES/sizer.mo`
///      where `<prefix>` comes from standard FHS locations or the executable path.
fn find_catalog() -> Option<PathBuf> {
    let locale = std::env::var("LANG")
        .or_else(|_| std::env::var("LC_ALL"))
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .ok()
        .and_then(|v| v.split('.').next().map(str::to_string))?;

    let candidates: Vec<PathBuf> = if let Ok(dir) = std::env::var(LOCALE_DIR_ENV) {
        vec![PathBuf::from(dir)]
    } else {
        let mut paths = Vec::new();
        // Standard FHS share prefixes.
        for prefix in ["/usr/local/share/locale", "/usr/share/locale"] {
            paths.push(PathBuf::from(prefix));
        }
        // Prefix hinted by the executable's install location.
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent().and_then(|p| p.parent()) {
                paths.push(parent.join("share").join("locale"));
            }
        }
        paths
    };

    for base in candidates {
        let candidate = base
            .join(&locale)
            .join("LC_MESSAGES")
            .join(format!("{DOMAIN}.mo"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Initializes the translation catalog once, probing common install locations.
///
/// Call this early in `main()` — subsequent calls are no-ops. If no catalog is
/// found, the lock stays `None` and [`tr`] simply returns the source string.
pub fn init() {
    if catalog_lock()
        .read()
        .expect("catalog lock poisoned")
        .is_some()
    {
        return;
    }

    if let Some(path) = find_catalog() {
        if let Ok(file) = File::open(&path) {
            let reader = BufReader::new(file);
            if let Ok(catalog) = Catalog::parse(reader) {
                *catalog_lock().write().expect("catalog lock poisoned") = Some(catalog);
            } else {
                eprintln!(
                    "sizer: warning: failed to parse translation catalog at {}",
                    path.display()
                );
            }
        }
    }
}

/// Translates `msgid` using the loaded catalog, falling back to `msgid` when no
/// catalog is available or the translation is missing.
///
/// This is the single entry point every user-facing string should pass through.
/// For formatted messages, build the fully-formed string first, then call `tr`
/// on it — GNU gettext typically expects complete sentences as msgids.
#[allow(dead_code)]
pub fn tr(msgid: &str) -> String {
    let guard = match catalog_lock().read() {
        Ok(g) => g,
        Err(_) => return msgid.to_string(),
    };
    match guard.as_ref() {
        Some(catalog) => catalog.gettext(msgid).to_string(),
        None => msgid.to_string(),
    }
}

/// Returns true if a catalog was loaded at all — useful for e2e probes.
#[allow(dead_code)]
pub fn is_active() -> bool {
    catalog_lock().read().map(|g| g.is_some()).unwrap_or(false)
}

/// Drops the loaded catalog (if any). Intended for tests that want to force a
/// re-parse, or for tools that swap catalogs at runtime.
#[allow(dead_code)]
pub fn reset_for_tests() {
    if let Ok(mut guard) = catalog_lock().write() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tr_returns_source_when_no_catalog_loaded() {
        // Tests run in a separate process from main(); the global CATALOG is
        // therefore `None` here, so tr() must act as identity.
        assert_eq!(tr("hello world"), "hello world");
        assert_eq!(tr("Save"), "Save");
    }
}
