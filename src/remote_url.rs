//! Typed classification of a git remote URL against a workspace's
//! declared `clone_method`.
//!
//! Motivated by a real incident: `sync::inject_github_token` rewrites
//! an HTTPS clone URL to `https://x-access-token:<token>@github.com/...`
//! so container clones can authenticate without a prompt. Git persists
//! the clone URL **verbatim** into `.git/config`, so whatever
//! `GITHUB_TOKEN` was live at clone time is fossilized in the repo
//! forever. A 2026-07-29 fleet sweep found 25 such repos carrying three
//! distinct tokens — one still live — spanning two orgs.
//!
//! Nothing detected that state, and nothing healed it. This module is
//! the detection half; `jobs::remediate_remote` is the healing half.
//!
//! The single most important invariant here: **a verdict can never
//! carry the secret it found.** These verdicts are serialized into
//! `drift-events.jsonl` on disk. A verdict that quoted the offending
//! URL would copy the token out of `.git/config` and into the audit
//! log — reproducing the leak while reporting it. [`Credential`]
//! therefore has no public constructor from a raw secret and no field
//! holding one; it records only the *userinfo shape*
//! (`x-access-token:***`), which is what an operator needs to
//! recognize the class of credential without ever seeing its value.

use serde::{Deserialize, Serialize};

use crate::config::CloneMethod;

/// A parsed `github.com` repository coordinate.
///
/// The only way to obtain one is [`GitHubSlug::parse`] against a real
/// remote URL, and it is the only input from which
/// [`GitHubSlug::canonical_url`] will build a replacement URL. That
/// pairing is deliberate: a remediation URL cannot be constructed
/// except from a coordinate we successfully parsed out of the URL we
/// are replacing, so remediation cannot retarget a repo at a different
/// path than the one it already pointed to. The dangerous shape —
/// guessing `org/repo` from a directory name and pointing a remote at
/// it — is unrepresentable rather than merely discouraged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GitHubSlug {
    org: String,
    repo: String,
}

impl GitHubSlug {
    /// Parse the `org/repo` coordinate out of any recognized
    /// `github.com` remote URL form. Returns `None` for non-GitHub
    /// remotes and for GitHub URLs whose path isn't exactly two
    /// segments.
    fn parse(url: &str) -> Option<Self> {
        let path = github_path(url)?;
        let path = path.strip_suffix(".git").unwrap_or(path);
        let (org, repo) = path.split_once('/')?;
        if org.is_empty() || repo.is_empty() || repo.contains('/') {
            return None;
        }
        Some(Self {
            org: org.to_string(),
            repo: repo.to_string(),
        })
    }

    /// The canonical remote URL for this coordinate under `method`.
    /// Mirrors `config::Workspace::clone_url` so a remediated remote
    /// is byte-identical to what a fresh clone would produce.
    pub(crate) fn canonical_url(&self, method: &CloneMethod) -> String {
        let Self { org, repo } = self;
        match method {
            CloneMethod::Ssh => format!("git@github.com:{org}/{repo}.git"),
            CloneMethod::Https => format!("https://github.com/{org}/{repo}.git"),
        }
    }

    pub(crate) fn slug(&self) -> String {
        format!("{}/{}", self.org, self.repo)
    }
}

/// The **shape** of a credential embedded in a remote URL's userinfo,
/// with the secret discarded at construction.
///
/// `x-access-token:ghp_abc123@` becomes `x-access-token:***`. There is
/// no accessor returning the secret because no field holds it — by the
/// time a `Credential` exists, the secret has already been dropped.
/// This is what makes it safe to serialize a verdict into the on-disk
/// drift log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Credential {
    /// Redacted userinfo, e.g. `x-access-token:***` or `***`.
    shape: String,
}

impl Credential {
    /// Redact a raw `userinfo` span (the part before `@`).
    ///
    /// `user:secret` keeps the username (which is routinely a
    /// non-secret sentinel like `x-access-token` or `oauth2`, and is
    /// the useful diagnostic) and masks the password. A bare `secret`
    /// with no colon is masked whole — we cannot tell a username from
    /// a token there, so we assume the dangerous reading.
    fn redact(userinfo: &str) -> Self {
        let shape = match userinfo.split_once(':') {
            Some((user, _secret)) if !user.is_empty() => format!("{user}:***"),
            _ => "***".to_string(),
        };
        Self { shape }
    }

    pub(crate) fn shape(&self) -> &str {
        &self.shape
    }
}

/// Typed verdict on one remote URL.
///
/// `PartialEq` without `Eq` because `CloneMethod` (carried by
/// `ProtocolMismatch`) derives only `PartialEq` upstream.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RemoteUrlVerdict {
    /// URL is a GitHub remote already matching the declared method,
    /// with no embedded credential. Nothing to do.
    Conforming,

    /// URL embeds a credential in its userinfo. Always drift regardless
    /// of declared method — a secret in `.git/config` is a leak whether
    /// or not the protocol is the configured one. Healing rewrites to
    /// the canonical URL, which drops the credential.
    EmbeddedCredential {
        slug: GitHubSlug,
        credential: Credential,
    },

    /// A clean GitHub URL whose protocol disagrees with the workspace's
    /// declared `clone_method`. Lower severity than a leak — a
    /// convergence gap, not a secret on disk.
    ProtocolMismatch {
        slug: GitHubSlug,
        actual: CloneMethod,
    },

    /// Not a `github.com` remote, or a GitHub URL we cannot parse into
    /// a coordinate. Out of scope: tend has no authority over a fork
    /// remote pointing at gitlab, a local path remote, or a URL shape
    /// we do not understand well enough to rewrite safely.
    ///
    /// Unparseable-but-GitHub deliberately lands here rather than in a
    /// "malformed" variant. The only action a malformed verdict could
    /// justify is rewriting a URL we just admitted we cannot read.
    OutOfScope,
}

impl RemoteUrlVerdict {
    /// Whether this verdict describes drift needing action.
    pub(crate) fn is_drift(&self) -> bool {
        matches!(
            self,
            Self::EmbeddedCredential { .. } | Self::ProtocolMismatch { .. }
        )
    }
}

/// Classify `url` against the workspace's declared `clone_method`.
///
/// Pure — no I/O, no environment. The caller supplies the observed URL
/// and the declared intent; every remote-URL policy decision tend makes
/// is decided here and nowhere else.
///
/// Precedence is severity-ordered: an embedded credential outranks a
/// protocol mismatch, because an HTTPS-with-token URL under a declared
/// `ssh` workspace is *both*, and the leak is what matters. Healing is
/// identical either way (rewrite to canonical), so the ordering costs
/// no remediation coverage — it only picks which of the two an operator
/// is told about.
pub(crate) fn classify(url: &str, declared: &CloneMethod) -> RemoteUrlVerdict {
    let Some(slug) = GitHubSlug::parse(url) else {
        return RemoteUrlVerdict::OutOfScope;
    };

    if let Some(userinfo) = embedded_userinfo(url) {
        return RemoteUrlVerdict::EmbeddedCredential {
            slug,
            credential: Credential::redact(userinfo),
        };
    }

    let actual = observed_method(url);
    match (&actual, declared) {
        (CloneMethod::Ssh, CloneMethod::Ssh) | (CloneMethod::Https, CloneMethod::Https) => {
            RemoteUrlVerdict::Conforming
        }
        _ => RemoteUrlVerdict::ProtocolMismatch { slug, actual },
    }
}

/// Extract the `userinfo` span of an HTTPS URL — the text between
/// `https://` and the `@` preceding the host — when one is present and
/// is not the benign bare `git@` of an SSH URL.
fn embedded_userinfo(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (userinfo, _) = rest.split_once('@')?;
    // A userinfo containing `/` means the `@` was in the path, not the
    // authority (e.g. a branch name with an `@`), so there is no
    // credential here.
    if userinfo.contains('/') {
        return None;
    }
    Some(userinfo)
}

/// Which protocol the URL actually uses. Only called after
/// [`GitHubSlug::parse`] succeeded, so the URL is one of the
/// recognized GitHub forms.
fn observed_method(url: &str) -> CloneMethod {
    if url.starts_with("https://") || url.starts_with("http://") {
        CloneMethod::Https
    } else {
        CloneMethod::Ssh
    }
}

/// Reduce any recognized `github.com` URL to its bare `org/repo[.git]`
/// path. Returns `None` for non-GitHub hosts.
///
/// Handles the four forms seen in the fleet:
///   `git@github.com:org/repo.git`
///   `ssh://git@github.com/org/repo.git`
///   `https://github.com/org/repo.git`
///   `https://<userinfo>@github.com/org/repo.git`
fn github_path(url: &str) -> Option<&str> {
    // scp-like syntax: [user@]github.com:org/repo
    if let Some((authority, path)) = url.split_once(':') {
        if !authority.contains('/') && authority.ends_with("github.com") {
            return Some(path);
        }
    }

    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
        .or_else(|| url.strip_prefix("git://"))?;

    // Drop userinfo if present.
    let rest = match rest.split_once('@') {
        Some((userinfo, after)) if !userinfo.contains('/') => after,
        _ => rest,
    };

    let (host, path) = rest.split_once('/')?;
    // Tolerate an explicit port on the host.
    let host = host.split(':').next().unwrap_or(host);
    if host != "github.com" {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SSH: CloneMethod = CloneMethod::Ssh;
    const HTTPS: CloneMethod = CloneMethod::Https;

    fn slug_of(v: &RemoteUrlVerdict) -> String {
        match v {
            RemoteUrlVerdict::EmbeddedCredential { slug, .. }
            | RemoteUrlVerdict::ProtocolMismatch { slug, .. } => slug.slug(),
            other => panic!("expected a drift verdict, got {other:?}"),
        }
    }

    #[test]
    fn canonical_ssh_url_conforms_under_ssh() {
        assert_eq!(
            classify("git@github.com:pleme-io/tend.git", &SSH),
            RemoteUrlVerdict::Conforming
        );
    }

    #[test]
    fn canonical_https_url_conforms_under_https() {
        assert_eq!(
            classify("https://github.com/pleme-io/tend.git", &HTTPS),
            RemoteUrlVerdict::Conforming
        );
    }

    /// The exact shape `sync::inject_github_token` produces, which is
    /// what the 2026-07-29 sweep found fossilized in 25 repos.
    #[test]
    fn injected_token_url_is_a_credential_leak() {
        let url = "https://x-access-token:ghp_AAAAAAAAAAAAAAAAAAAA@github.com/akeylesslabs/k8s.git";
        let verdict = classify(url, &SSH);
        assert_eq!(slug_of(&verdict), "akeylesslabs/k8s");
        match verdict {
            RemoteUrlVerdict::EmbeddedCredential { credential, .. } => {
                assert_eq!(credential.shape(), "x-access-token:***");
            }
            other => panic!("expected EmbeddedCredential, got {other:?}"),
        }
    }

    /// The load-bearing safety property: a verdict must never carry
    /// the secret, because verdicts are written to drift-events.jsonl.
    #[test]
    fn verdict_never_carries_the_secret() {
        let secret = "ghp_SUPERSECRETTOKENVALUE0000";
        let url = format!("https://x-access-token:{secret}@github.com/org/repo.git");
        let verdict = classify(&url, &SSH);

        let rendered = format!("{verdict:?}");
        assert!(
            !rendered.contains(secret),
            "verdict Debug leaked the token: {rendered}"
        );

        match verdict {
            RemoteUrlVerdict::EmbeddedCredential { credential, .. } => {
                let json = serde_json::to_string(&credential).unwrap();
                assert!(!json.contains(secret), "serialized credential leaked: {json}");
            }
            other => panic!("expected EmbeddedCredential, got {other:?}"),
        }
    }

    #[test]
    fn bare_token_userinfo_is_masked_whole() {
        let url = "https://ghp_AAAAAAAAAAAAAAAAAAAA@github.com/org/repo.git";
        match classify(url, &SSH) {
            RemoteUrlVerdict::EmbeddedCredential { credential, .. } => {
                assert_eq!(credential.shape(), "***");
            }
            other => panic!("expected EmbeddedCredential, got {other:?}"),
        }
    }

    /// A credential is drift even when the protocol is the declared
    /// one — the leak is independent of protocol conformance.
    #[test]
    fn credential_is_drift_even_under_declared_https() {
        let url = "https://x-access-token:ghp_AAAA@github.com/org/repo.git";
        assert!(matches!(
            classify(url, &HTTPS),
            RemoteUrlVerdict::EmbeddedCredential { .. }
        ));
    }

    #[test]
    fn https_under_declared_ssh_is_protocol_mismatch() {
        let verdict = classify("https://github.com/pleme-io/tend.git", &SSH);
        assert_eq!(slug_of(&verdict), "pleme-io/tend");
        assert!(matches!(
            verdict,
            RemoteUrlVerdict::ProtocolMismatch {
                actual: CloneMethod::Https,
                ..
            }
        ));
    }

    #[test]
    fn ssh_under_declared_https_is_protocol_mismatch() {
        assert!(matches!(
            classify("git@github.com:pleme-io/tend.git", &HTTPS),
            RemoteUrlVerdict::ProtocolMismatch {
                actual: CloneMethod::Ssh,
                ..
            }
        ));
    }

    #[test]
    fn ssh_protocol_url_form_is_recognized() {
        assert_eq!(
            classify("ssh://git@github.com/pleme-io/tend.git", &SSH),
            RemoteUrlVerdict::Conforming
        );
    }

    #[test]
    fn non_github_remotes_are_out_of_scope() {
        for url in [
            "git@gitlab.com:org/repo.git",
            "https://gitlab.com/org/repo.git",
            "https://git.sr.ht/~user/repo",
            "/srv/mirrors/repo.git",
            "../sibling-repo",
        ] {
            assert_eq!(
                classify(url, &SSH),
                RemoteUrlVerdict::OutOfScope,
                "expected {url} out of scope"
            );
        }
    }

    /// A host merely *ending* in github.com must not be treated as
    /// GitHub — `evilgithub.com` would otherwise be rewritten to a real
    /// GitHub URL, silently retargeting the remote.
    #[test]
    fn lookalike_host_is_out_of_scope() {
        assert_eq!(
            classify("https://evilgithub.com/org/repo.git", &SSH),
            RemoteUrlVerdict::OutOfScope
        );
    }

    #[test]
    fn github_url_without_two_path_segments_is_out_of_scope() {
        for url in [
            "https://github.com/orgonly",
            "https://github.com/",
            "https://github.com/a/b/c",
        ] {
            assert_eq!(
                classify(url, &SSH),
                RemoteUrlVerdict::OutOfScope,
                "expected {url} out of scope"
            );
        }
    }

    #[test]
    fn missing_dot_git_suffix_still_parses() {
        let verdict = classify("https://github.com/pleme-io/tend", &SSH);
        assert_eq!(slug_of(&verdict), "pleme-io/tend");
    }

    /// Healing must produce exactly what a fresh clone would, so a
    /// remediated repo is indistinguishable from a newly synced one.
    #[test]
    fn canonical_url_matches_clone_url_shape() {
        let slug = GitHubSlug::parse("https://x-access-token:ghp_A@github.com/org/repo.git").unwrap();
        assert_eq!(
            slug.canonical_url(&CloneMethod::Ssh),
            "git@github.com:org/repo.git"
        );
        assert_eq!(
            slug.canonical_url(&CloneMethod::Https),
            "https://github.com/org/repo.git"
        );
    }

    /// Healing is idempotent: the canonical URL classifies as
    /// Conforming, so a healed repo never re-reports next cycle.
    #[test]
    fn healing_converges_in_one_pass() {
        for declared in [SSH, HTTPS] {
            let dirty = "https://x-access-token:ghp_AAAA@github.com/org/repo.git";
            let verdict = classify(dirty, &declared);
            let slug = match verdict {
                RemoteUrlVerdict::EmbeddedCredential { slug, .. } => slug,
                other => panic!("expected EmbeddedCredential, got {other:?}"),
            };
            let healed = slug.canonical_url(&declared);
            assert_eq!(
                classify(&healed, &declared),
                RemoteUrlVerdict::Conforming,
                "healed URL {healed} did not converge under {declared}"
            );
        }
    }

    #[test]
    fn is_drift_only_for_actionable_verdicts() {
        assert!(!RemoteUrlVerdict::Conforming.is_drift());
        assert!(!RemoteUrlVerdict::OutOfScope.is_drift());
        assert!(classify("https://ghp_A@github.com/o/r.git", &SSH).is_drift());
        assert!(classify("https://github.com/o/r.git", &SSH).is_drift());
    }
}
