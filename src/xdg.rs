//! XDG tier resolution — ONE resolver, shared by every consumer.
//!
//! okiba (置き場) resolves a tier and REFUSES a relative override rather than
//! resolving it against the cwd (the XDG spec's own rule). That guarantee is
//! only as strong as the resolver each caller actually reaches for, so there
//! is exactly one construction site and every consumer goes through it —
//! `src/cache.rs` and `src/config.rs` previously carried byte-identical
//! copies of the lookup below, which is the shape a shared primitive exists
//! to remove.
//!
//! Construction goes through okiba's `from_env` seam rather than `for_app`
//! deliberately: `for_app` reads `$HOME` and nothing else, while a launchd or
//! systemd unit routinely runs with HOME unset and must still resolve via
//! getpwuid. `dirs::home_dir()` is that fallback, and a bare `for_app` would
//! have lost it.

/// The `tend`-scoped okiba resolver, reading the process environment.
///
/// Consumers pick their own terminal fallback for [`okiba::Missing`] — a
/// cache and a config file do not want the same last resort — but every one
/// of them inherits the "a relative override is ignored, never joined" rule
/// from here.
pub(crate) fn resolver() -> okiba::Okiba {
    okiba::Okiba::from_env("tend", |k| match k {
        "HOME" => std::env::var("HOME")
            .ok()
            .or_else(|| dirs::home_dir().map(|p| p.to_string_lossy().into_owned())),
        other => std::env::var(other).ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The invariant the whole module exists for, pinned against okiba's own
    /// `from_env` seam so it runs with no process environment and cannot race
    /// another test mutating `std::env`.
    #[test]
    fn a_relative_tier_override_is_ignored_not_joined() {
        for bogus in ["", "rel/x", "./x", ".."] {
            let o = okiba::Okiba::from_env("tend", |k| match k {
                "XDG_CONFIG_HOME" | "XDG_CACHE_HOME" => Some(bogus.to_string()),
                "HOME" => Some("/home/u".to_string()),
                _ => None,
            });
            assert_eq!(
                o.base(okiba::Tier::Config).unwrap(),
                PathBuf::from("/home/u/.config"),
                "XDG_CONFIG_HOME={bogus:?} must fall through to $HOME, not be joined"
            );
            assert_eq!(
                o.base(okiba::Tier::Cache).unwrap(),
                PathBuf::from("/home/u/.cache"),
                "XDG_CACHE_HOME={bogus:?} must fall through to $HOME, not be joined"
            );
        }
    }

    /// A relative `$HOME` leaves nothing absolute to build on, and okiba says
    /// so with a typed error instead of inventing a cwd-relative base. Each
    /// consumer's own fallback is what turns this into a usable path.
    #[test]
    fn a_relative_home_yields_missing_rather_than_a_relative_base() {
        let o = okiba::Okiba::from_env("tend", |k| match k {
            "HOME" => Some("rel/home".to_string()),
            _ => None,
        });
        assert!(o.base(okiba::Tier::Config).is_err());
        assert!(o.base(okiba::Tier::Cache).is_err());
    }

    /// The live resolver never hands back a relative path on this machine.
    #[test]
    fn the_process_resolver_is_absolute_or_missing() {
        for tier in [okiba::Tier::Config, okiba::Tier::Cache] {
            if let Ok(p) = resolver().base(tier) {
                assert!(p.is_absolute(), "{tier:?} resolved to a relative {p:?}");
            }
        }
    }
}
