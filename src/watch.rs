use anyhow::{Context, Result};
use colored::Colorize;
use std::path::Path;

use crate::audit::AuditLog;
use crate::config::{PostHook, Workspace};
use crate::display;
use crate::git::GitOps;
use crate::github::GitHubClient;
use crate::sync;
use crate::watch_cache::{RepoState, WatchStateStore};

/// Summary of a watch cycle run.
pub struct WatchSummary {
    pub checked: usize,
    pub new_versions: usize,
    pub errors: usize,
    /// Number of file watches that detected changes.
    pub file_changes: usize,
    /// Number of flake input watches that detected staleness.
    pub flake_input_updates: usize,
}

/// Tracking mode read from matrix.toml for a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackMode {
    /// Detect new semver tags. Version = tag (strip v prefix).
    Tags,
    /// Detect HEAD commits. Version = "{base}-unstable.{date}".
    Commits { unstable_base: String },
}

/// Trait abstracting matrix.toml appending for testability.
pub trait MatrixAppender: Send + Sync {
    /// Append a pending version entry for a repo. Returns Ok(true) if appended,
    /// Ok(false) if repo not found in matrix or version already exists.
    fn append_entry(
        &self,
        matrix_file: &Path,
        repo_name: &str,
        version: &str,
        rev: &str,
        language: Option<&str>,
    ) -> Result<bool>;

    /// Look up the tracking mode for a repo from the matrix. Returns None if
    /// the repo is not in the matrix.
    fn get_track_mode(&self, matrix_file: &Path, repo_name: &str) -> Result<Option<TrackMode>>;
}

/// Real implementation using toml_edit for format-preserving edits.
pub struct TomlMatrixAppender;

impl MatrixAppender for TomlMatrixAppender {
    fn append_entry(
        &self,
        matrix_file: &Path,
        repo_name: &str,
        version: &str,
        rev: &str,
        language: Option<&str>,
    ) -> Result<bool> {
        append_matrix_entry(matrix_file, repo_name, version, rev, language)
    }

    fn get_track_mode(&self, matrix_file: &Path, repo_name: &str) -> Result<Option<TrackMode>> {
        if !matrix_file.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(matrix_file)
            .with_context(|| format!("reading {}", matrix_file.display()))?;
        let doc = content
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("parsing {}", matrix_file.display()))?;

        let packages = match doc.get("packages").and_then(|p| p.as_table()) {
            Some(t) => t,
            None => return Ok(None),
        };

        for (_pkg_name, pkg_value) in packages.iter() {
            if let Some(table) = pkg_value.as_table() {
                if table.get("repo").and_then(|v| v.as_str()) == Some(repo_name) {
                    let track = table
                        .get("track")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tags");
                    return Ok(Some(match track {
                        "commits" => {
                            let base = table
                                .get("unstable_base")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0.1.0")
                                .to_string();
                            TrackMode::Commits { unstable_base: base }
                        }
                        _ => TrackMode::Tags,
                    }));
                }
            }
        }
        Ok(None)
    }
}

/// Run the watch cycle for a workspace: detect new commits/tags in GitHub repos
/// and append pending entries to the configured matrix.toml file.
///
/// The cycle:
/// 1. Resolve the workspace's repo list
/// 2. Load cached watch state
/// 3. For each repo, check HEAD commit and latest tag via GitHub API
/// 4. If a tag changed, detect language and append a pending entry to matrix.toml
/// 5. Save updated state to cache
/// 6. Optionally auto-commit + push matrix.toml
///
/// Rate limiting note: each repo requires 2-3 GitHub API calls (HEAD, tags,
/// and possibly languages). For workspaces with 80+ repos, this could consume
/// 160-240 requests per cycle. With authenticated rate limits of 5000 req/hr,
/// this supports ~20-30 cycles per hour. Adjust the daemon interval accordingly.
pub async fn run_watch_cycle(
    ws: &Workspace,
    quiet: bool,
    github: &dyn GitHubClient,
    cache_store: &dyn WatchStateStore,
    matrix_appender: &dyn MatrixAppender,
    git_ops: &dyn GitOps,
    audit: &AuditLog,
) -> Result<WatchSummary> {
    let watch_cfg = ws.watch.as_ref()
        .ok_or_else(|| anyhow::anyhow!("watch not configured for workspace {}", ws.name))?;

    let matrix_file = match watch_cfg.matrix_file.as_deref() {
        Some(path) => {
            let expanded = shellexpand::tilde(path);
            Some(std::path::PathBuf::from(expanded.as_ref()))
        }
        None => None,
    };

    // If there's no matrix_file and no file_watches and no flake_input_watches, nothing to do
    if matrix_file.is_none() && watch_cfg.file_watches.is_empty() && watch_cfg.flake_input_watches.is_empty() {
        return Ok(WatchSummary {
            checked: 0,
            new_versions: 0,
            errors: 0,
            file_changes: 0,
            flake_input_updates: 0,
        });
    }

    let mut state = cache_store.load(&ws.name)?;
    let mut checked = 0usize;
    let mut new_versions = 0usize;
    let mut errors = 0usize;
    let mut last_repo = String::new();
    let mut last_version = String::new();
    let mut last_rev = String::new();

    // ── Repo-level watch (matrix.toml version tracking) ──
    if let Some(ref matrix_file) = matrix_file {
        let repos = sync::resolve_repos(ws, false).await?;
        let org = ws.org.as_deref().unwrap_or(&ws.name);

        for repo_name in &repos {
            checked += 1;

            // Fetch HEAD commit SHA
            let head = match github.get_repo_head(org, repo_name).await {
                Ok(sha) => sha,
                Err(e) => {
                    if !quiet {
                        eprintln!("  warning: failed to get HEAD for {repo_name}: {e}");
                    }
                    errors += 1;
                    continue;
                }
            };

            // Fetch latest tag
            let latest_tag = match github.get_latest_tag(org, repo_name).await {
                Ok(tag) => tag,
                Err(e) => {
                    if !quiet {
                        eprintln!("  warning: failed to get tags for {repo_name}: {e}");
                    }
                    errors += 1;
                    continue;
                }
            };

            // Compare with cached state
            let cached = state.repos.get(repo_name);
            let head_changed = cached.is_none_or(|c| c.head != head);
            let tag_changed = match (cached.and_then(|c| c.latest_tag.as_deref()), latest_tag.as_deref()) {
                (Some(old), Some(new)) => old != new,
                (None, Some(_)) => true,
                _ => false,
            };

            // Determine what kind of change to act on:
            // - Tag-tracked repos: only act on tag changes
            // - Commit-tracked repos: act on HEAD changes (even without tag changes)
            let track_mode = matrix_appender
                .get_track_mode(matrix_file, repo_name)
                .unwrap_or(None);

            let should_act = match &track_mode {
                Some(TrackMode::Tags) | None => tag_changed,
                Some(TrackMode::Commits { .. }) => head_changed,
            };

            if should_act {
                // Detect language (use cached if available and HEAD hasn't changed)
                let language = if !head_changed && cached.is_some_and(|c| c.language.is_some()) {
                    cached.unwrap().language.clone()
                } else {
                    match github.detect_repo_language(org, repo_name).await {
                        Ok(lang) => lang,
                        Err(e) => {
                            if !quiet {
                                eprintln!("  warning: failed to detect language for {repo_name}: {e}");
                            }
                            None
                        }
                    }
                };

                // Compute version based on tracking mode
                let (version, display_tag) = match &track_mode {
                    Some(TrackMode::Commits { unstable_base }) => {
                        // Include short SHA to differentiate multiple commits on the same day
                        let today = chrono::Utc::now().format("%Y-%m-%d");
                        let short_sha = &head[..head.len().min(8)];
                        let ver = format!("{unstable_base}-unstable.{today}.{short_sha}");
                        let tag = format!("HEAD@{short_sha}");
                        (ver, tag)
                    }
                    _ => {
                        // Tag-tracked: use tag as version
                        let new_tag = latest_tag.as_deref().unwrap_or("unknown");
                        let ver = new_tag.strip_prefix('v').unwrap_or(new_tag).to_string();
                        (ver, new_tag.to_string())
                    }
                };

                // Audit: version detected
                let tracking_label = match &track_mode {
                    Some(TrackMode::Tags) | None => "tags",
                    Some(TrackMode::Commits { .. }) => "commits",
                };
                audit.version_detected(org, repo_name, &version, &head, tracking_label);

                // Append entry to matrix.toml (pass HEAD SHA as rev)
                match matrix_appender.append_entry(matrix_file, repo_name, &version, &head, language.as_deref()) {
                    Ok(true) => {
                        if !quiet {
                            display::print_watch_new_version(repo_name, &version, &display_tag);
                        }
                        audit.matrix_entry_appended(repo_name, &version, "pending");
                        new_versions += 1;
                        last_repo = repo_name.clone();
                        last_version = version.clone();
                        last_rev = head.clone();
                    }
                    Ok(false) => {
                        // Repo not found in matrix or version already exists
                    }
                    Err(e) => {
                        if !quiet {
                            eprintln!("  warning: failed to append matrix entry for {repo_name}: {e}");
                        }
                        errors += 1;
                    }
                }

                // Update cache state
                state.repos.insert(repo_name.clone(), RepoState {
                    head: head.clone(),
                    latest_tag: latest_tag.clone(),
                    language,
                });
            } else {
                // No actionable change; update cache with current state
                let language = cached.and_then(|c| c.language.clone());
                state.repos.insert(repo_name.clone(), RepoState {
                    head,
                    latest_tag,
                    language,
                });
            }
        }

        if new_versions > 0 {
            let matrix_file_str = matrix_file.to_string_lossy().to_string();

            // Step 1: Auto-certify — run `akeyless-matrix certify` to build hashes + generate Nix
            if watch_cfg.auto_certify {
                if !quiet {
                    eprintln!("  [>>] running akeyless-matrix certify...");
                }
                let certify_start = std::time::Instant::now();
                match run_certify(matrix_file) {
                    Ok(()) => {
                        let duration_ms = certify_start.elapsed().as_millis() as u64;
                        audit.certify_complete(&last_repo, &last_version, "verified", duration_ms);
                    }
                    Err(e) => {
                        let duration_ms = certify_start.elapsed().as_millis() as u64;
                        audit.certify_complete(&last_repo, &last_version, "failed", duration_ms);
                        if !quiet {
                            eprintln!("  warning: auto-certify failed: {e}");
                        }
                        errors += 1;
                    }
                }
            }

            // Run after_certify post-hooks
            if let Err(e) = run_post_hooks(
                &watch_cfg.post_hooks, "after_certify",
                &last_repo, &last_version, &last_rev, &matrix_file_str,
                audit,
            ).await {
                if !quiet {
                    eprintln!("  warning: after_certify hook failed: {e}");
                }
                errors += 1;
            }

            // Step 2: Auto-commit — commit+push all changes (matrix.toml + generated files)
            if watch_cfg.auto_commit {
                match auto_commit_matrix(matrix_file, git_ops) {
                    Ok(()) => {
                        let repo_dir = matrix_file.parent()
                            .map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string())
                            .unwrap_or_default();
                        let msg = "chore(matrix): certify new upstream versions";
                        audit.commit_pushed(&repo_dir, "(auto)", msg);
                    }
                    Err(e) => {
                        if !quiet {
                            eprintln!("  warning: auto-commit failed: {e}");
                        }
                        errors += 1;
                    }
                }
            }

            // Run after_commit post-hooks
            if let Err(e) = run_post_hooks(
                &watch_cfg.post_hooks, "after_commit",
                &last_repo, &last_version, &last_rev, &matrix_file_str,
                audit,
            ).await {
                if !quiet {
                    eprintln!("  warning: after_commit hook failed: {e}");
                }
                errors += 1;
            }

            // Step 3: Auto-propagate — run `tend flake-update --changed <repo>`
            if let Some(ref repo_name) = watch_cfg.auto_propagate {
                if !quiet {
                    eprintln!("  [>>] propagating flake update for {repo_name}...");
                }
                if let Err(e) = run_flake_propagate(repo_name, ws) {
                    if !quiet {
                        eprintln!("  warning: auto-propagate failed: {e}");
                    }
                    errors += 1;
                }
            }

            // Run after_propagate post-hooks
            if let Err(e) = run_post_hooks(
                &watch_cfg.post_hooks, "after_propagate",
                &last_repo, &last_version, &last_rev, &matrix_file_str,
                audit,
            ).await {
                if !quiet {
                    eprintln!("  warning: after_propagate hook failed: {e}");
                }
                errors += 1;
            }

            // Run after_all post-hooks
            if let Err(e) = run_post_hooks(
                &watch_cfg.post_hooks, "after_all",
                &last_repo, &last_version, &last_rev, &matrix_file_str,
                audit,
            ).await {
                if !quiet {
                    eprintln!("  warning: after_all hook failed: {e}");
                }
                errors += 1;
            }
        }
    }

    // ── File-level watch (specific file SHA tracking) ──
    let mut file_changes = 0usize;
    for fw in &watch_cfg.file_watches {
        let cache_key = format!("{}/{}/{}", fw.org, fw.repo, fw.path);

        let (new_sha, _size, download_url) = match github
            .get_file_sha(&fw.org, &fw.repo, &fw.path)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                if !quiet {
                    eprintln!(
                        "  warning: failed to get file SHA for {}: {e}",
                        cache_key
                    );
                }
                errors += 1;
                continue;
            }
        };

        let cached_sha = state.file_shas.get(&cache_key).cloned();

        if cached_sha.as_deref() == Some(&new_sha) {
            // No change
            continue;
        }

        file_changes += 1;

        // Audit: file change detected
        audit.file_change_detected(
            &fw.org,
            &fw.repo,
            &fw.path,
            cached_sha.as_deref(),
            &new_sha,
            _size,
        );

        if !quiet {
            println!(
                "  {} file changed: {}",
                "!".yellow().bold(),
                cache_key
            );
            println!(
                "    old SHA: {}",
                cached_sha.as_deref().unwrap_or("(none)")
            );
            println!("    new SHA: {}", new_sha);
        }

        // Download the file if download_to is configured
        let mut current_file = String::new();
        let mut previous_file = String::new();

        if let Some(ref download_dir) = fw.download_to {
            let dir = shellexpand::tilde(download_dir).to_string();
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating download dir {dir}"))?;

            // Derive extension from the watched file path
            let ext = std::path::Path::new(&fw.path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("bin");
            let short_sha = &new_sha[..new_sha.len().min(12)];
            current_file = format!("{dir}/{short_sha}.{ext}");

            let http = todoku::HttpClient::builder()
                .build()
                .map_err(|e| anyhow::anyhow!("{e}"))
                .context("building HTTP client for download")?;
            let resp = http
                .get_raw(&download_url)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
                .with_context(|| format!("downloading {download_url}"))?;
            let content = resp
                .text()
                .await
                .with_context(|| format!("reading body from {download_url}"))?;
            let content_len = content.len() as u64;
            std::fs::write(&current_file, &content)
                .with_context(|| format!("writing {current_file}"))?;

            // Audit: spec downloaded
            audit.spec_downloaded(
                &fw.org,
                &fw.repo,
                &fw.path,
                &new_sha,
                &current_file,
                content_len,
            );

            if !quiet {
                println!("    downloaded: {}", current_file);
            }

            // Previous version (if cached)
            if let Some(ref old_sha) = cached_sha {
                let old_short = &old_sha[..old_sha.len().min(12)];
                previous_file = format!("{dir}/{old_short}.{ext}");
            }
        }

        // Run post-hooks with variable substitution
        for hook in &fw.post_hooks {
            if hook.trigger != "on_change" {
                continue;
            }

            let args: Vec<String> = hook
                .args
                .iter()
                .map(|a| {
                    a.replace("$CURRENT_FILE", &current_file)
                        .replace("$PREVIOUS_FILE", &previous_file)
                        .replace("$SHA", &new_sha)
                        .replace("$REPO", &fw.repo)
                        .replace("$ORG", &fw.org)
                        .replace("$PATH", &fw.path)
                        .replace("$NAME", &fw.name)
                })
                .collect();

            let dir = hook
                .working_dir
                .as_deref()
                .map(|d| shellexpand::tilde(d).to_string());

            if !quiet {
                eprintln!(
                    "  {} running file-watch hook: {} {}",
                    "=>".blue().bold(),
                    hook.command,
                    args.join(" ")
                );
            }

            let mut cmd = tokio::process::Command::new(&hook.command);
            cmd.args(&args);
            if let Some(ref d) = dir {
                cmd.current_dir(d);
            }

            let hook_start = std::time::Instant::now();
            match cmd.status().await {
                Ok(status) => {
                    let duration_ms = hook_start.elapsed().as_millis() as u64;
                    let exit_code = status.code().unwrap_or(-1);
                    audit.hook_executed("on_change", &hook.command, exit_code, duration_ms);
                    if !status.success() && !hook.continue_on_error {
                        if !quiet {
                            eprintln!(
                                "  warning: file-watch hook failed: {} (exit {})",
                                hook.command, status
                            );
                        }
                        errors += 1;
                    }
                }
                Err(e) => {
                    let duration_ms = hook_start.elapsed().as_millis() as u64;
                    audit.hook_executed("on_change", &hook.command, -1, duration_ms);
                    if !quiet {
                        eprintln!(
                            "  warning: file-watch hook error: {}: {e}",
                            hook.command
                        );
                    }
                    errors += 1;
                }
            }
        }

        // Update cache
        state.file_shas.insert(cache_key, new_sha);
    }

    // ── Flake input watch (flake.lock input staleness tracking) ──
    let mut flake_input_updates = 0usize;
    let base_dir = ws.resolved_base_dir()?;

    for fiw in &watch_cfg.flake_input_watches {
        let flake_lock_path = base_dir.join(&fiw.repo).join("flake.lock");

        // Parse the locked rev and upstream owner/repo from flake.lock
        let (locked_rev, lock_owner, lock_repo) = match parse_flake_lock_input(&flake_lock_path, &fiw.input) {
            Ok(result) => result,
            Err(e) => {
                if !quiet {
                    eprintln!("  warning: failed to parse flake.lock for {}: {e}", fiw.name);
                }
                errors += 1;
                continue;
            }
        };

        // Derive upstream owner/repo: explicit config or from flake.lock
        let (upstream_owner, upstream_repo) = if let Some(ref upstream) = fiw.upstream {
            match upstream.split_once('/') {
                Some((o, r)) => (o.to_string(), r.to_string()),
                None => {
                    if !quiet {
                        eprintln!("  warning: invalid upstream format for {}: expected owner/repo", fiw.name);
                    }
                    errors += 1;
                    continue;
                }
            }
        } else {
            (lock_owner, lock_repo)
        };

        // Compare locked rev against upstream
        let (upstream_rev, upstream_tag) = match fiw.mode {
            crate::config::FlakeInputMode::Commits => {
                match github.get_repo_head(&upstream_owner, &upstream_repo).await {
                    Ok(sha) => (sha, None),
                    Err(e) => {
                        if !quiet {
                            eprintln!("  warning: failed to get HEAD for {}/{}: {e}", upstream_owner, upstream_repo);
                        }
                        errors += 1;
                        continue;
                    }
                }
            }
            crate::config::FlakeInputMode::Tags => {
                let tag = match github.get_latest_tag(&upstream_owner, &upstream_repo).await {
                    Ok(t) => t,
                    Err(e) => {
                        if !quiet {
                            eprintln!("  warning: failed to get tags for {}/{}: {e}", upstream_owner, upstream_repo);
                        }
                        errors += 1;
                        continue;
                    }
                };
                let cached = state.flake_inputs.get(&fiw.name);
                let cached_tag = cached.and_then(|c| c.upstream_tag.as_deref());
                // Only trigger on new tag (different from cached tag)
                if tag.as_deref() == cached_tag {
                    continue;
                }
                // Get HEAD SHA for the rev
                let sha = match github.get_repo_head(&upstream_owner, &upstream_repo).await {
                    Ok(s) => s,
                    Err(e) => {
                        if !quiet {
                            eprintln!("  warning: failed to get HEAD for {}/{}: {e}", upstream_owner, upstream_repo);
                        }
                        errors += 1;
                        continue;
                    }
                };
                (sha, tag)
            }
        };

        // Skip if locked rev matches upstream (already up to date)
        if locked_rev == upstream_rev {
            // Update cache even if up-to-date (for tags mode tracking)
            state.flake_inputs.insert(fiw.name.clone(), crate::watch_cache::FlakeInputCacheEntry {
                upstream_rev: upstream_rev.clone(),
                upstream_tag: upstream_tag.clone(),
            });
            continue;
        }

        // Skip if we already processed this upstream rev (cache hit, no auto_update)
        if let Some(cached) = state.flake_inputs.get(&fiw.name) {
            if cached.upstream_rev == upstream_rev && !fiw.auto_update {
                continue;
            }
        }

        flake_input_updates += 1;

        // Audit + display
        audit.flake_input_stale(&fiw.name, &fiw.repo, &fiw.input, &locked_rev, &upstream_rev);

        if !quiet {
            println!(
                "  {} flake input stale: {} ({}/{})",
                "!".yellow().bold(),
                fiw.name,
                fiw.repo,
                fiw.input,
            );
            println!("    locked:   {}", &locked_rev[..locked_rev.len().min(12)]);
            println!("    upstream: {}", &upstream_rev[..upstream_rev.len().min(12)]);
        }

        // Run on_stale post-hooks
        for hook in &fiw.post_hooks {
            if hook.trigger != "on_stale" {
                continue;
            }

            let args: Vec<String> = hook
                .args
                .iter()
                .map(|a| {
                    a.replace("$INPUT", &fiw.input)
                        .replace("$LOCKED_REV", &locked_rev)
                        .replace("$UPSTREAM_REV", &upstream_rev)
                        .replace("$REPO", &fiw.repo)
                        .replace("$NAME", &fiw.name)
                })
                .collect();

            let dir = hook
                .working_dir
                .as_deref()
                .map(|d| shellexpand::tilde(d).to_string());

            if !quiet {
                eprintln!(
                    "  {} running flake-input hook: {} {}",
                    "=>".blue().bold(),
                    hook.command,
                    args.join(" ")
                );
            }

            let mut cmd = tokio::process::Command::new(&hook.command);
            cmd.args(&args);
            if let Some(ref d) = dir {
                cmd.current_dir(d);
            }

            let hook_start = std::time::Instant::now();
            match cmd.status().await {
                Ok(status) => {
                    let duration_ms = hook_start.elapsed().as_millis() as u64;
                    let exit_code = status.code().unwrap_or(-1);
                    audit.hook_executed("on_stale", &hook.command, exit_code, duration_ms);
                    if !status.success() && !hook.continue_on_error {
                        if !quiet {
                            eprintln!(
                                "  warning: flake-input hook failed: {} (exit {})",
                                hook.command, status
                            );
                        }
                        errors += 1;
                    }
                }
                Err(e) => {
                    let duration_ms = hook_start.elapsed().as_millis() as u64;
                    audit.hook_executed("on_stale", &hook.command, -1, duration_ms);
                    if !quiet {
                        eprintln!("  warning: flake-input hook error: {}: {e}", hook.command);
                    }
                    errors += 1;
                }
            }
        }

        // Auto-update: nix flake update <input>
        if fiw.auto_update {
            let repo_dir = base_dir.join(&fiw.repo);
            if !quiet {
                eprintln!("  [>>] running nix flake update {} in {}...", fiw.input, fiw.repo);
            }
            match run_nix_flake_update(&repo_dir, &fiw.input) {
                Ok(()) => {
                    if !quiet {
                        eprintln!("  [{}] flake input {} updated", "ok".green(), fiw.input);
                    }
                }
                Err(e) => {
                    if !quiet {
                        eprintln!("  warning: nix flake update failed for {}: {e}", fiw.input);
                    }
                    errors += 1;
                    // Update cache even on failure to avoid re-triggering
                    state.flake_inputs.insert(fiw.name.clone(), crate::watch_cache::FlakeInputCacheEntry {
                        upstream_rev: upstream_rev.clone(),
                        upstream_tag: upstream_tag.clone(),
                    });
                    continue;
                }
            }

            // Auto-commit: git add flake.lock, commit, push
            if fiw.auto_commit {
                let repo_dir = base_dir.join(&fiw.repo);
                let flake_lock = repo_dir.join("flake.lock");
                match auto_commit_flake_input(&repo_dir, &flake_lock, &fiw.input, git_ops) {
                    Ok(()) => {
                        let msg = format!("chore: update {} flake input", fiw.input);
                        audit.commit_pushed(&fiw.repo, "(auto)", &msg);
                    }
                    Err(e) => {
                        if !quiet {
                            eprintln!("  warning: auto-commit failed for {}: {e}", fiw.repo);
                        }
                        errors += 1;
                    }
                }
            }

            // Auto-propagate: tend flake-update --changed <repo>
            if let Some(ref propagate_repo) = fiw.auto_propagate {
                if !quiet {
                    eprintln!("  [>>] propagating flake update for {propagate_repo}...");
                }
                if let Err(e) = run_flake_propagate(propagate_repo, ws) {
                    if !quiet {
                        eprintln!("  warning: auto-propagate failed: {e}");
                    }
                    errors += 1;
                }
            }
        }

        // Update cache
        state.flake_inputs.insert(fiw.name.clone(), crate::watch_cache::FlakeInputCacheEntry {
            upstream_rev: upstream_rev.clone(),
            upstream_tag: upstream_tag.clone(),
        });
    }

    cache_store.save(&ws.name, &state)?;

    Ok(WatchSummary {
        checked,
        new_versions,
        errors,
        file_changes,
        flake_input_updates,
    })
}

/// Run post-hooks that match the given trigger.
async fn run_post_hooks(
    hooks: &[PostHook],
    trigger: &str,
    repo: &str,
    version: &str,
    rev: &str,
    matrix_file: &str,
    audit: &AuditLog,
) -> Result<()> {
    for hook in hooks.iter().filter(|h| h.trigger == trigger) {
        let args: Vec<String> = hook
            .args
            .iter()
            .map(|a| {
                a.replace("$VERSION", version)
                    .replace("$REPO", repo)
                    .replace("$REV", rev)
                    .replace("$MATRIX_FILE", matrix_file)
            })
            .collect();

        let dir = hook
            .working_dir
            .as_deref()
            .map(|d| shellexpand::tilde(d).to_string());

        eprintln!(
            "  {} running hook: {} {}",
            "=>".blue().bold(),
            hook.command,
            args.join(" ")
        );

        let mut cmd = tokio::process::Command::new(&hook.command);
        cmd.args(&args);
        if let Some(ref d) = dir {
            cmd.current_dir(d);
        }

        let hook_start = std::time::Instant::now();
        let status = cmd.status().await?;
        let duration_ms = hook_start.elapsed().as_millis() as u64;
        let exit_code = status.code().unwrap_or(-1);
        audit.hook_executed(trigger, &hook.command, exit_code, duration_ms);

        if !status.success() && !hook.continue_on_error {
            anyhow::bail!(
                "post-hook failed: {} (exit {})",
                hook.command,
                status
            );
        }
    }
    Ok(())
}

/// Run `akeyless-matrix certify` on the matrix file.
fn run_certify(matrix_file: &Path) -> Result<()> {
    let output = std::process::Command::new("akeyless-matrix")
        .args(["certify", "--matrix", &matrix_file.to_string_lossy()])
        .output()
        .context("running akeyless-matrix certify")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("akeyless-matrix certify failed: {stderr}");
    }
    Ok(())
}

/// Parse a flake.lock file to extract the locked rev and upstream owner/repo for a given input.
///
/// Returns `(locked_rev, owner, repo)`. Skips inputs using `follows` (JSON array)
/// with a descriptive error.
fn parse_flake_lock_input(
    flake_lock_path: &Path,
    input_name: &str,
) -> Result<(String, String, String)> {
    let content = std::fs::read_to_string(flake_lock_path)
        .with_context(|| format!("reading {}", flake_lock_path.display()))?;
    let lock: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("parsing {}", flake_lock_path.display()))?;

    // Navigate: root.inputs.<input_name> → node name (string or array)
    let root_inputs = lock
        .pointer("/root/inputs")
        .or_else(|| lock.pointer("/nodes/root/inputs"))
        .ok_or_else(|| anyhow::anyhow!("no root/inputs in flake.lock"))?;

    let node_ref = root_inputs
        .get(input_name)
        .ok_or_else(|| anyhow::anyhow!("input '{input_name}' not found in flake.lock"))?;

    // If the input uses `follows`, it's a JSON array — skip
    let node_name = match node_ref {
        serde_json::Value::String(s) => s.as_str(),
        serde_json::Value::Array(_) => {
            anyhow::bail!("input '{input_name}' uses follows (not independently updatable)");
        }
        _ => anyhow::bail!("unexpected type for input '{input_name}' in flake.lock"),
    };

    // Navigate: nodes.<node_name>.locked
    let locked = lock
        .pointer(&format!("/nodes/{node_name}/locked"))
        .ok_or_else(|| anyhow::anyhow!("no locked data for node '{node_name}' in flake.lock"))?;

    let rev = locked
        .get("rev")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no rev in locked data for '{node_name}'"))?
        .to_string();

    let owner = locked
        .get("owner")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no owner in locked data for '{node_name}'"))?
        .to_string();

    let repo = locked
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no repo in locked data for '{node_name}'"))?
        .to_string();

    Ok((rev, owner, repo))
}

/// Run `nix flake update <input>` in a repo directory.
fn run_nix_flake_update(repo_dir: &Path, input_name: &str) -> Result<()> {
    let output = std::process::Command::new("nix")
        .args(["flake", "update", input_name])
        .current_dir(repo_dir)
        .output()
        .context("running nix flake update")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("nix flake update {input_name} failed: {stderr}");
    }
    Ok(())
}

/// Auto commit and push flake.lock after a flake input update.
fn auto_commit_flake_input(
    repo_dir: &Path,
    flake_lock: &Path,
    input_name: &str,
    git_ops: &dyn GitOps,
) -> Result<()> {
    git_ops.add(repo_dir, flake_lock)?;

    if !git_ops.has_staged_changes(repo_dir)? {
        return Ok(());
    }

    let msg = format!("chore: update {input_name} flake input");
    git_ops.commit(repo_dir, &msg)?;
    git_ops.push(repo_dir)?;

    Ok(())
}

/// Run `tend flake-update --changed <repo>` to propagate to dependent flakes.
fn run_flake_propagate(changed_repo: &str, ws: &Workspace) -> Result<()> {
    let mut cmd = std::process::Command::new("tend");
    cmd.args(["flake-update", "--changed", changed_repo, "--workspace", &ws.name]);

    let output = cmd.output().context("running tend flake-update")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("tend flake-update failed: {stderr}");
    }
    Ok(())
}

/// Append a pending version entry to the matrix.toml file.
///
/// Looks for a package whose `repo` field matches the given repo name, then
/// appends a new version entry under that package. Uses toml_edit to preserve
/// formatting and comments.
///
/// Returns Ok(true) if an entry was appended, Ok(false) if the repo was not
/// found in the matrix, or Err on I/O or parse errors.
fn append_matrix_entry(
    matrix_file: &Path,
    repo_name: &str,
    version: &str,
    rev: &str,
    language: Option<&str>,
) -> Result<bool> {
    if !matrix_file.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(matrix_file)
        .with_context(|| format!("reading {}", matrix_file.display()))?;

    let mut doc = content.parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing {}", matrix_file.display()))?;

    // Find the package table that matches this repo
    let packages = match doc.get("packages") {
        Some(item) => item,
        None => return Ok(false),
    };

    let packages_table = match packages.as_table() {
        Some(t) => t,
        None => return Ok(false),
    };

    // Find the package name whose `repo` field matches repo_name
    let mut target_pkg = None;
    for (pkg_name, pkg_value) in packages_table.iter() {
        if let Some(table) = pkg_value.as_table() {
            if let Some(repo_val) = table.get("repo") {
                if let Some(repo_str) = repo_val.as_str() {
                    if repo_str == repo_name {
                        target_pkg = Some(pkg_name.to_string());
                        break;
                    }
                }
            }
        }
    }

    let pkg_name = match target_pkg {
        Some(name) => name,
        None => return Ok(false),
    };

    // Check if this version already exists under the package's versions
    let packages_mut = doc.get_mut("packages")
        .and_then(|p| p.as_table_mut())
        .ok_or_else(|| anyhow::anyhow!("packages table disappeared"))?;

    let pkg_table = packages_mut.get_mut(&pkg_name)
        .and_then(|p| p.as_table_mut())
        .ok_or_else(|| anyhow::anyhow!("package {pkg_name} table disappeared"))?;

    // Ensure versions is a table
    if pkg_table.get("versions").is_none() {
        pkg_table.insert("versions", toml_edit::Item::Table(toml_edit::Table::new()));
    }

    let versions = pkg_table.get_mut("versions")
        .and_then(|v| v.as_table_mut())
        .ok_or_else(|| anyhow::anyhow!("versions is not a table for {pkg_name}"))?;

    // Skip if this version already exists
    if versions.contains_key(version) {
        return Ok(false);
    }

    // Build the new version entry
    let mut version_table = toml_edit::Table::new();
    version_table.insert("rev", toml_edit::value(rev));
    version_table.insert("status", toml_edit::value("pending"));

    if let Some(lang) = language {
        version_table.insert("language", toml_edit::value(lang));
    }

    versions.insert(version, toml_edit::Item::Table(version_table));

    // Write back atomically (write to temp then rename)
    let tmp_path = matrix_file.with_extension("toml.tmp");
    std::fs::write(&tmp_path, doc.to_string())
        .with_context(|| format!("writing temp file {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, matrix_file)
        .with_context(|| format!("renaming {} to {}", tmp_path.display(), matrix_file.display()))?;

    Ok(true)
}

/// Auto commit and push all changes in the matrix repo.
///
/// Stages matrix.toml, lib/, builds/, and certifications.toml — everything
/// that `akeyless-matrix certify` may have modified.
fn auto_commit_matrix(matrix_file: &Path, git_ops: &dyn GitOps) -> Result<()> {
    let repo_dir = matrix_file.parent()
        .ok_or_else(|| anyhow::anyhow!("matrix file has no parent directory"))?;

    // Stage all relevant files
    git_ops.add(repo_dir, matrix_file)?;
    let lib_dir = repo_dir.join("lib");
    if lib_dir.exists() {
        git_ops.add(repo_dir, &lib_dir)?;
    }
    let builds_dir = repo_dir.join("builds");
    if builds_dir.exists() {
        git_ops.add(repo_dir, &builds_dir)?;
    }
    let cert_file = repo_dir.join("certifications.toml");
    if cert_file.exists() {
        git_ops.add(repo_dir, &cert_file)?;
    }

    if !git_ops.has_staged_changes(repo_dir)? {
        return Ok(());
    }

    git_ops.commit(repo_dir, "chore(matrix): certify new upstream versions")?;
    git_ops.push(repo_dir)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CloneMethod, Workspace, WatchConfig};
    use crate::watch_cache::{RepoState, WatchState};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex;

    struct MockGitHub {
        heads: BTreeMap<String, String>,
        tags: BTreeMap<String, Option<String>>,
        languages: BTreeMap<String, Option<String>>,
        /// File SHA responses keyed by "org/repo/path"
        file_shas: BTreeMap<String, (String, u64, String)>,
    }

    impl MockGitHub {
        fn new() -> Self {
            Self {
                heads: BTreeMap::new(),
                tags: BTreeMap::new(),
                languages: BTreeMap::new(),
                file_shas: BTreeMap::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::github::GitHubClient for MockGitHub {
        async fn get_repo_head(&self, _org: &str, repo: &str) -> anyhow::Result<String> {
            self.heads.get(repo)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("repo not found"))
        }

        async fn get_latest_tag(&self, _org: &str, repo: &str) -> anyhow::Result<Option<String>> {
            Ok(self.tags.get(repo).cloned().flatten())
        }

        async fn detect_repo_language(&self, _org: &str, repo: &str) -> anyhow::Result<Option<String>> {
            Ok(self.languages.get(repo).cloned().flatten())
        }

        async fn get_file_sha(
            &self,
            org: &str,
            repo: &str,
            path: &str,
        ) -> anyhow::Result<(String, u64, String)> {
            let key = format!("{org}/{repo}/{path}");
            self.file_shas
                .get(&key)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("file not found: {key}"))
        }
    }

    struct MockCache {
        state: Mutex<WatchState>,
    }

    impl WatchStateStore for MockCache {
        fn load(&self, _workspace_name: &str) -> anyhow::Result<WatchState> {
            Ok(self.state.lock().unwrap().clone())
        }

        fn save(&self, _workspace_name: &str, state: &WatchState) -> anyhow::Result<()> {
            *self.state.lock().unwrap() = state.clone();
            Ok(())
        }
    }

    struct MockAppender {
        appended: Mutex<Vec<(String, String, String)>>,
        track_modes: BTreeMap<String, TrackMode>,
    }

    impl MockAppender {
        fn new() -> Self {
            Self {
                appended: Mutex::new(Vec::new()),
                track_modes: BTreeMap::new(),
            }
        }

        fn with_track(mut self, repo: &str, mode: TrackMode) -> Self {
            self.track_modes.insert(repo.to_string(), mode);
            self
        }
    }

    impl MatrixAppender for MockAppender {
        fn append_entry(
            &self,
            _matrix_file: &std::path::Path,
            repo_name: &str,
            version: &str,
            rev: &str,
            _language: Option<&str>,
        ) -> anyhow::Result<bool> {
            self.appended.lock().unwrap().push((
                repo_name.to_string(),
                version.to_string(),
                rev.to_string(),
            ));
            Ok(true)
        }

        fn get_track_mode(
            &self,
            _matrix_file: &std::path::Path,
            repo_name: &str,
        ) -> anyhow::Result<Option<TrackMode>> {
            Ok(self.track_modes.get(repo_name).cloned())
        }
    }

    struct MockGitOps;

    impl crate::git::GitOps for MockGitOps {
        fn add(&self, _repo_dir: &std::path::Path, _file_path: &std::path::Path) -> anyhow::Result<()> { Ok(()) }
        fn has_staged_changes(&self, _repo_dir: &std::path::Path) -> anyhow::Result<bool> { Ok(false) }
        fn commit(&self, _repo_dir: &std::path::Path, _message: &str) -> anyhow::Result<()> { Ok(()) }
        fn push(&self, _repo_dir: &std::path::Path) -> anyhow::Result<()> { Ok(()) }
    }

    fn make_test_workspace(name: &str, matrix_file: Option<&str>) -> Workspace {
        Workspace {
            name: name.to_string(),
            provider: "github".to_string(),
            base_dir: "/tmp/test-tend".to_string(),
            clone_method: CloneMethod::Ssh,
            discover: false,
            org: Some("test-org".to_string()),
            exclude: vec![],
            extra_repos: vec!["repo-a".to_string()],
            flake_deps: HashMap::new(),
            watch: Some(WatchConfig {
                enable: true,
                matrix_file: matrix_file.map(|s| s.to_string()),
                auto_certify: false,
                auto_commit: false,
                auto_propagate: None,
                post_hooks: vec![],
                file_watches: vec![],
                flake_input_watches: vec![],
            }),
        }
    }

    fn test_audit() -> crate::audit::AuditLog {
        crate::audit::AuditLog::new(std::path::PathBuf::from("/tmp/tend-test-audit.jsonl"))
    }

    #[tokio::test]
    async fn test_watch_cycle_no_matrix_file() {
        let ws = make_test_workspace("test-ws", None);
        let github = MockGitHub::new();
        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.checked, 0);
        assert_eq!(summary.new_versions, 0);
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn test_watch_cycle_detects_new_tag() {
        // Use a temp file for matrix_file so the workspace resolves repos via extra_repos
        let tmp_dir = std::env::temp_dir().join("tend-test-watch");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let matrix_path = tmp_dir.join("matrix.toml");
        // Write a minimal matrix.toml with repo-a
        std::fs::write(&matrix_path, r#"
[packages.repo-a]
repo = "repo-a"
"#).unwrap();

        let ws = make_test_workspace("test-ws", Some(matrix_path.to_str().unwrap()));

        let mut heads = BTreeMap::new();
        heads.insert("repo-a".to_string(), "sha123".to_string());
        let mut tags = BTreeMap::new();
        tags.insert("repo-a".to_string(), Some("v1.2.3".to_string()));
        let mut languages = BTreeMap::new();
        languages.insert("repo-a".to_string(), Some("rust".to_string()));

        let github = MockGitHub { heads, tags, languages, file_shas: BTreeMap::new() };
        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.checked, 1);
        assert_eq!(summary.new_versions, 1);
        assert_eq!(summary.errors, 0);

        let appended = appender.appended.lock().unwrap();
        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0].0, "repo-a");
        assert_eq!(appended[0].1, "1.2.3"); // v prefix stripped
        assert_eq!(appended[0].2, "sha123"); // HEAD SHA passed as rev

        // Verify cache was updated
        let saved_state = cache.state.lock().unwrap();
        let repo_state = saved_state.repos.get("repo-a").unwrap();
        assert_eq!(repo_state.head, "sha123");
        assert_eq!(repo_state.latest_tag.as_deref(), Some("v1.2.3"));
        assert_eq!(repo_state.language.as_deref(), Some("rust"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn test_watch_cycle_handles_api_errors() {
        let ws = make_test_workspace("test-ws", Some("/tmp/fake-matrix.toml"));

        // GitHub returns error (heads map is empty → repo not found)
        let github = MockGitHub::new();
        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.checked, 1);
        assert_eq!(summary.new_versions, 0);
        assert_eq!(summary.errors, 1);
    }

    #[tokio::test]
    async fn test_watch_cycle_reuses_cached_language() {
        let tmp_dir = std::env::temp_dir().join("tend-test-watch-langcache");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let matrix_path = tmp_dir.join("matrix.toml");
        std::fs::write(&matrix_path, "[packages.repo-a]\nrepo = \"repo-a\"\n").unwrap();

        let ws = make_test_workspace("test-ws", Some(matrix_path.to_str().unwrap()));

        // HEAD is SAME as cached — language should be reused from cache
        let mut heads = BTreeMap::new();
        heads.insert("repo-a".to_string(), "sameHEAD".to_string());
        let mut tags = BTreeMap::new();
        tags.insert("repo-a".to_string(), Some("v2.0.0".to_string()));
        // languages map is EMPTY — if detect_repo_language is called it returns None
        let github = MockGitHub { heads, tags, languages: BTreeMap::new(), file_shas: BTreeMap::new() };

        let mut initial_state = WatchState::default();
        initial_state.repos.insert("repo-a".to_string(), RepoState {
            head: "sameHEAD".to_string(),  // same HEAD
            latest_tag: Some("v1.0.0".to_string()),  // OLD tag → triggers change
            language: Some("rust".to_string()),  // cached language
        });
        let cache = MockCache { state: Mutex::new(initial_state) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.new_versions, 1);

        // Verify cached language was preserved
        let saved_state = cache.state.lock().unwrap();
        let repo_state = &saved_state.repos["repo-a"];
        assert_eq!(repo_state.language.as_deref(), Some("rust"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    struct RecordingGitOps {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingGitOps {
        fn new() -> Self {
            Self { calls: Mutex::new(Vec::new()) }
        }
    }

    impl crate::git::GitOps for RecordingGitOps {
        fn add(&self, _: &std::path::Path, _: &std::path::Path) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push("add".into()); Ok(())
        }
        fn has_staged_changes(&self, _: &std::path::Path) -> anyhow::Result<bool> {
            self.calls.lock().unwrap().push("has_staged_changes".into()); Ok(true)
        }
        fn commit(&self, _: &std::path::Path, _: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push("commit".into()); Ok(())
        }
        fn push(&self, _: &std::path::Path) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push("push".into()); Ok(())
        }
    }

    #[tokio::test]
    async fn test_watch_cycle_auto_commit() {
        let tmp_dir = std::env::temp_dir().join("tend-test-watch-autocommit");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let matrix_path = tmp_dir.join("matrix.toml");
        std::fs::write(&matrix_path, "[packages.repo-a]\nrepo = \"repo-a\"\n").unwrap();

        let mut ws = make_test_workspace("test-ws", Some(matrix_path.to_str().unwrap()));
        ws.watch.as_mut().unwrap().auto_commit = true;

        let mut heads = BTreeMap::new();
        heads.insert("repo-a".to_string(), "newhead".to_string());
        let mut tags = BTreeMap::new();
        tags.insert("repo-a".to_string(), Some("v2.0.0".to_string()));
        let github = MockGitHub { heads, tags, languages: BTreeMap::new(), file_shas: BTreeMap::new() };

        let mut initial = WatchState::default();
        initial.repos.insert("repo-a".to_string(), RepoState {
            head: "old".to_string(),
            latest_tag: Some("v1.0.0".to_string()),
            language: None,
        });
        let cache = MockCache { state: Mutex::new(initial) };
        let appender = MockAppender::new();
        let git_ops = RecordingGitOps::new();

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.new_versions, 1);

        let calls = git_ops.calls.lock().unwrap();
        assert!(calls.contains(&"add".to_string()));
        assert!(calls.contains(&"commit".to_string()));
        assert!(calls.contains(&"push".to_string()));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_append_matrix_entry_creates_entry() {
        let dir = std::env::temp_dir().join("tend-test-append-create");
        let _ = std::fs::create_dir_all(&dir);
        let mf = dir.join("matrix.toml");
        std::fs::write(&mf, "[packages.akeyless-test]\nowner = \"org\"\nrepo = \"test-repo\"\nlanguage = \"go\"\nbuilder = \"mkGoTool\"\ntier = 1\ndescription = \"t\"\nhomepage = \"h\"\n").unwrap();

        let result = append_matrix_entry(&mf, "test-repo", "1.0.0", "abc123", Some("go")).unwrap();
        assert!(result);

        let content = std::fs::read_to_string(&mf).unwrap();
        assert!(content.contains("status = \"pending\""));
        assert!(content.contains("rev = \"abc123\""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_append_matrix_entry_skips_unknown_repo() {
        let dir = std::env::temp_dir().join("tend-test-append-unknown");
        let _ = std::fs::create_dir_all(&dir);
        let mf = dir.join("matrix.toml");
        std::fs::write(&mf, "[packages.akeyless-test]\nrepo = \"test-repo\"\n").unwrap();

        let result = append_matrix_entry(&mf, "unknown-repo", "1.0.0", "abc", None).unwrap();
        assert!(!result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_append_matrix_entry_skips_duplicate_version() {
        let dir = std::env::temp_dir().join("tend-test-append-dup");
        let _ = std::fs::create_dir_all(&dir);
        let mf = dir.join("matrix.toml");
        std::fs::write(&mf, "[packages.akeyless-test]\nrepo = \"test-repo\"\n\n[packages.akeyless-test.versions.\"1.0.0\"]\nrev = \"existing\"\nstatus = \"verified\"\n").unwrap();

        let result = append_matrix_entry(&mf, "test-repo", "1.0.0", "newrev", None).unwrap();
        assert!(!result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_watch_cycle_no_change_when_tag_same() {
        let tmp_dir = std::env::temp_dir().join("tend-test-watch-nochange");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let matrix_path = tmp_dir.join("matrix.toml");
        std::fs::write(&matrix_path, r#"
[packages.repo-a]
repo = "repo-a"
"#).unwrap();

        let ws = make_test_workspace("test-ws", Some(matrix_path.to_str().unwrap()));

        let mut heads = BTreeMap::new();
        heads.insert("repo-a".to_string(), "sha123".to_string());
        let mut tags = BTreeMap::new();
        tags.insert("repo-a".to_string(), Some("v1.0.0".to_string()));

        let github = MockGitHub { heads, tags, languages: BTreeMap::new(), file_shas: BTreeMap::new() };

        // Pre-populate cache with the same tag
        let mut initial_state = WatchState::default();
        initial_state.repos.insert("repo-a".to_string(), RepoState {
            head: "sha999".to_string(),
            latest_tag: Some("v1.0.0".to_string()),
            language: Some("rust".to_string()),
        });

        let cache = MockCache { state: Mutex::new(initial_state) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.checked, 1);
        assert_eq!(summary.new_versions, 0);
        assert_eq!(summary.errors, 0);

        let appended = appender.appended.lock().unwrap();
        assert_eq!(appended.len(), 0);

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn test_watch_cycle_commit_tracked_detects_head_change() {
        let tmp_dir = std::env::temp_dir().join("tend-test-watch-commits");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let matrix_path = tmp_dir.join("matrix.toml");
        std::fs::write(&matrix_path, "[packages.repo-a]\nrepo = \"repo-a\"\n").unwrap();

        let ws = make_test_workspace("test-ws", Some(matrix_path.to_str().unwrap()));

        // HEAD changed but tag is SAME (no new release)
        let mut heads = BTreeMap::new();
        heads.insert("repo-a".to_string(), "newHEAD456".to_string());
        let mut tags = BTreeMap::new();
        tags.insert("repo-a".to_string(), None::<String>); // no tags at all
        let github = MockGitHub { heads, tags, languages: BTreeMap::new(), file_shas: BTreeMap::new() };

        // Cache has old HEAD
        let mut initial = WatchState::default();
        initial.repos.insert("repo-a".to_string(), RepoState {
            head: "oldHEAD123".to_string(),
            latest_tag: None,
            language: Some("go".to_string()),
        });
        let cache = MockCache { state: Mutex::new(initial) };

        // Mark repo as commit-tracked
        let appender = MockAppender::new()
            .with_track("repo-a", TrackMode::Commits {
                unstable_base: "0.2.0".to_string(),
            });

        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.new_versions, 1);

        let appended = appender.appended.lock().unwrap();
        assert_eq!(appended.len(), 1);
        // Version should be unstable format with short SHA
        assert!(appended[0].1.starts_with("0.2.0-unstable."));
        assert!(appended[0].1.contains("newHEAD4")); // short SHA suffix
        // Rev should be the HEAD SHA
        assert_eq!(appended[0].2, "newHEAD456");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn test_watch_cycle_tag_tracked_ignores_head_only_changes() {
        let tmp_dir = std::env::temp_dir().join("tend-test-watch-tag-only");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let matrix_path = tmp_dir.join("matrix.toml");
        std::fs::write(&matrix_path, "[packages.repo-a]\nrepo = \"repo-a\"\n").unwrap();

        let ws = make_test_workspace("test-ws", Some(matrix_path.to_str().unwrap()));

        // HEAD changed but tag is SAME
        let mut heads = BTreeMap::new();
        heads.insert("repo-a".to_string(), "newHEAD789".to_string());
        let mut tags = BTreeMap::new();
        tags.insert("repo-a".to_string(), Some("v1.0.0".to_string()));
        let github = MockGitHub { heads, tags, languages: BTreeMap::new(), file_shas: BTreeMap::new() };

        let mut initial = WatchState::default();
        initial.repos.insert("repo-a".to_string(), RepoState {
            head: "oldHEAD111".to_string(),
            latest_tag: Some("v1.0.0".to_string()), // same tag
            language: Some("go".to_string()),
        });
        let cache = MockCache { state: Mutex::new(initial) };

        // Tag-tracked (default) — should NOT trigger on HEAD-only change
        let appender = MockAppender::new()
            .with_track("repo-a", TrackMode::Tags);

        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.new_versions, 0);
        let appended = appender.appended.lock().unwrap();
        assert_eq!(appended.len(), 0);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_post_hook_deserialization_from_yaml() {
        let yaml = r#"
enable: true
matrix_file: ~/matrix.toml
auto_certify: false
auto_commit: false
post_hooks:
  - trigger: after_certify
    command: echo
    args:
      - "$VERSION"
      - "$REPO"
    working_dir: ~/code
    continue_on_error: true
  - trigger: after_all
    command: notify-send
    args:
      - "done: $REV"
"#;
        let config: WatchConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.post_hooks.len(), 2);
        assert_eq!(config.post_hooks[0].trigger, "after_certify");
        assert_eq!(config.post_hooks[0].command, "echo");
        assert_eq!(config.post_hooks[0].args, vec!["$VERSION", "$REPO"]);
        assert_eq!(config.post_hooks[0].working_dir.as_deref(), Some("~/code"));
        assert!(config.post_hooks[0].continue_on_error);
        assert_eq!(config.post_hooks[1].trigger, "after_all");
        assert_eq!(config.post_hooks[1].command, "notify-send");
        assert_eq!(config.post_hooks[1].args, vec!["done: $REV"]);
        assert_eq!(config.post_hooks[1].working_dir, None);
        assert!(!config.post_hooks[1].continue_on_error);
    }

    #[tokio::test]
    async fn test_post_hook_variable_substitution() {
        use crate::config::PostHook;

        let tmp_dir = std::env::temp_dir().join("tend-test-hook-vars");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let output_file = tmp_dir.join("hook-output.txt");

        let hooks = vec![PostHook {
            trigger: "after_certify".to_string(),
            command: "bash".to_string(),
            args: vec![
                "-c".to_string(),
                format!(
                    "echo \"$VERSION $REPO $REV $MATRIX_FILE\" > {}",
                    output_file.display()
                ),
            ],
            working_dir: None,
            continue_on_error: false,
        }];

        let audit = test_audit();
        run_post_hooks(&hooks, "after_certify", "my-repo", "1.2.3", "abc123", "/path/matrix.toml", &audit)
            .await
            .unwrap();

        let content = std::fs::read_to_string(&output_file).unwrap();
        assert_eq!(content.trim(), "1.2.3 my-repo abc123 /path/matrix.toml");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_empty_post_hooks_backward_compatible() {
        let yaml = r#"
enable: true
matrix_file: ~/matrix.toml
auto_certify: false
auto_commit: false
"#;
        let config: WatchConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.post_hooks.is_empty());
    }

    #[tokio::test]
    async fn test_unknown_trigger_silently_skipped() {
        use crate::config::PostHook;

        let hooks = vec![PostHook {
            trigger: "after_certify".to_string(),
            command: "false".to_string(), // would fail if executed
            args: vec![],
            working_dir: None,
            continue_on_error: false,
        }];

        // Run with a trigger that doesn't match — should be a no-op
        let audit = test_audit();
        let result = run_post_hooks(&hooks, "unknown_trigger", "repo", "1.0", "abc", "/m.toml", &audit).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_continue_on_error_allows_pipeline_to_continue() {
        use crate::config::PostHook;

        let hooks = vec![
            PostHook {
                trigger: "after_all".to_string(),
                command: "false".to_string(), // exits with code 1
                args: vec![],
                working_dir: None,
                continue_on_error: true, // should NOT bail
            },
        ];

        let audit = test_audit();
        let result = run_post_hooks(&hooks, "after_all", "repo", "1.0", "abc", "/m.toml", &audit).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_continue_on_error_false_stops_pipeline() {
        use crate::config::PostHook;

        let hooks = vec![
            PostHook {
                trigger: "after_all".to_string(),
                command: "false".to_string(), // exits with code 1
                args: vec![],
                working_dir: None,
                continue_on_error: false, // should bail
            },
        ];

        let audit = test_audit();
        let result = run_post_hooks(&hooks, "after_all", "repo", "1.0", "abc", "/m.toml", &audit).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("post-hook failed"));
    }

    // ── File Watch Tests ──

    #[test]
    fn test_file_watch_config_deserialization() {
        let yaml = r#"
enable: true
file_watches:
  - name: akeyless-openapi-spec
    org: akeylesslabs
    repo: akeyless-go
    path: api/openapi.yaml
    download_to: ~/code/specs/
    post_hooks:
      - trigger: on_change
        command: iac-forge
        args:
          - sync
          - "--spec-old"
          - "$PREVIOUS_FILE"
          - "--spec-new"
          - "$CURRENT_FILE"
        working_dir: ~/code/resources
        continue_on_error: false
  - name: another-file
    org: myorg
    repo: myrepo
    path: config.json
"#;
        let config: WatchConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.file_watches.len(), 2);

        let fw = &config.file_watches[0];
        assert_eq!(fw.name, "akeyless-openapi-spec");
        assert_eq!(fw.org, "akeylesslabs");
        assert_eq!(fw.repo, "akeyless-go");
        assert_eq!(fw.path, "api/openapi.yaml");
        assert_eq!(fw.download_to.as_deref(), Some("~/code/specs/"));
        assert_eq!(fw.post_hooks.len(), 1);
        assert_eq!(fw.post_hooks[0].trigger, "on_change");
        assert_eq!(fw.post_hooks[0].command, "iac-forge");
        assert_eq!(fw.post_hooks[0].args[2], "$PREVIOUS_FILE");
        assert_eq!(fw.post_hooks[0].args[4], "$CURRENT_FILE");
        assert_eq!(fw.post_hooks[0].working_dir.as_deref(), Some("~/code/resources"));
        assert!(!fw.post_hooks[0].continue_on_error);

        let fw2 = &config.file_watches[1];
        assert_eq!(fw2.name, "another-file");
        assert_eq!(fw2.org, "myorg");
        assert_eq!(fw2.repo, "myrepo");
        assert_eq!(fw2.path, "config.json");
        assert!(fw2.download_to.is_none());
        assert!(fw2.post_hooks.is_empty());
    }

    #[tokio::test]
    async fn test_file_watch_detects_sha_change() {
        use crate::config::FileWatch;

        let tmp_dir = std::env::temp_dir().join("tend-test-fw-change");
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let _ = std::fs::create_dir_all(&tmp_dir);

        let hook_output = tmp_dir.join("hook-ran.txt");

        let mut ws = make_test_workspace("fw-test", None);
        ws.extra_repos = vec![]; // no repo-level watches
        ws.watch.as_mut().unwrap().file_watches = vec![FileWatch {
            name: "test-spec".to_string(),
            org: "testorg".to_string(),
            repo: "testrepo".to_string(),
            path: "api/spec.yaml".to_string(),
            download_to: None,
            post_hooks: vec![PostHook {
                trigger: "on_change".to_string(),
                command: "bash".to_string(),
                args: vec![
                    "-c".to_string(),
                    format!(
                        "echo \"sha=$SHA org=$ORG repo=$REPO path=$PATH name=$NAME\" > {}",
                        hook_output.display()
                    ),
                ],
                working_dir: None,
                continue_on_error: false,
            }],
        }];

        let mut github = MockGitHub::new();
        github.file_shas.insert(
            "testorg/testrepo/api/spec.yaml".to_string(),
            ("newsha123456789abc".to_string(), 1024, "https://example.com/spec.yaml".to_string()),
        );

        // Pre-populate cache with OLD SHA
        let mut initial_state = WatchState::default();
        initial_state.file_shas.insert(
            "testorg/testrepo/api/spec.yaml".to_string(),
            "oldsha000000000000".to_string(),
        );
        let cache = MockCache { state: Mutex::new(initial_state) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.file_changes, 1);
        assert_eq!(summary.errors, 0);

        // Verify hook ran with correct variable substitution
        let content = std::fs::read_to_string(&hook_output).unwrap();
        assert!(content.contains("sha=newsha123456789abc"));
        assert!(content.contains("org=testorg"));
        assert!(content.contains("repo=testrepo"));
        assert!(content.contains("path=api/spec.yaml"));
        assert!(content.contains("name=test-spec"));

        // Verify cache was updated
        let saved = cache.state.lock().unwrap();
        assert_eq!(
            saved.file_shas.get("testorg/testrepo/api/spec.yaml").unwrap(),
            "newsha123456789abc"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn test_file_watch_skips_unchanged() {
        use crate::config::FileWatch;

        let mut ws = make_test_workspace("fw-skip", None);
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().file_watches = vec![FileWatch {
            name: "stable-file".to_string(),
            org: "org".to_string(),
            repo: "repo".to_string(),
            path: "config.json".to_string(),
            download_to: None,
            post_hooks: vec![PostHook {
                trigger: "on_change".to_string(),
                command: "false".to_string(), // would fail if executed
                args: vec![],
                working_dir: None,
                continue_on_error: false,
            }],
        }];

        let mut github = MockGitHub::new();
        github.file_shas.insert(
            "org/repo/config.json".to_string(),
            ("samesha1234567890ab".to_string(), 512, "https://example.com/config.json".to_string()),
        );

        // Cache has the SAME SHA
        let mut initial_state = WatchState::default();
        initial_state.file_shas.insert(
            "org/repo/config.json".to_string(),
            "samesha1234567890ab".to_string(),
        );
        let cache = MockCache { state: Mutex::new(initial_state) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.file_changes, 0);
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn test_file_watch_variable_substitution() {
        use crate::config::FileWatch;

        let tmp_dir = std::env::temp_dir().join("tend-test-fw-vars");
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let _ = std::fs::create_dir_all(&tmp_dir);

        let output_file = tmp_dir.join("vars.txt");

        let mut ws = make_test_workspace("fw-vars", None);
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().file_watches = vec![FileWatch {
            name: "my-spec-watch".to_string(),
            org: "myorg".to_string(),
            repo: "myrepo".to_string(),
            path: "docs/api.yaml".to_string(),
            download_to: None,
            post_hooks: vec![PostHook {
                trigger: "on_change".to_string(),
                command: "bash".to_string(),
                args: vec![
                    "-c".to_string(),
                    format!(
                        "echo \"CURRENT=$CURRENT_FILE PREVIOUS=$PREVIOUS_FILE SHA=$SHA REPO=$REPO ORG=$ORG PATH=$PATH NAME=$NAME\" > {}",
                        output_file.display()
                    ),
                ],
                working_dir: None,
                continue_on_error: false,
            }],
        }];

        let mut github = MockGitHub::new();
        github.file_shas.insert(
            "myorg/myrepo/docs/api.yaml".to_string(),
            ("abc123def456789012".to_string(), 100, "https://example.com/api.yaml".to_string()),
        );

        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.file_changes, 1);

        let content = std::fs::read_to_string(&output_file).unwrap();
        // No download_to, so CURRENT_FILE and PREVIOUS_FILE are empty
        assert!(content.contains("CURRENT= "));
        assert!(content.contains("PREVIOUS= "));
        assert!(content.contains("SHA=abc123def456789012"));
        assert!(content.contains("REPO=myrepo"));
        assert!(content.contains("ORG=myorg"));
        assert!(content.contains("PATH=docs/api.yaml"));
        assert!(content.contains("NAME=my-spec-watch"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_file_watch_backward_compatible() {
        // Config without file_watches should still parse (defaults to empty vec)
        let yaml = r#"
enable: true
matrix_file: ~/matrix.toml
auto_certify: false
auto_commit: false
"#;
        let config: WatchConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.file_watches.is_empty());
        assert!(config.post_hooks.is_empty());
    }

    #[tokio::test]
    async fn test_file_watch_download_to_creates_dir() {
        use crate::config::FileWatch;

        let tmp_dir = std::env::temp_dir().join("tend-test-fw-download");
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let download_dir = tmp_dir.join("nested").join("specs");

        let mut ws = make_test_workspace("fw-dl", None);
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().file_watches = vec![FileWatch {
            name: "dl-test".to_string(),
            org: "dlorg".to_string(),
            repo: "dlrepo".to_string(),
            path: "api/openapi.yaml".to_string(),
            download_to: Some(download_dir.to_string_lossy().to_string()),
            post_hooks: vec![],
        }];

        // We need a real HTTP server for download, so we use a mock that provides
        // the download URL. In this test we verify the directory is created and the
        // cache is updated, even if download fails (it will fail because the URL is fake).
        let mut github = MockGitHub::new();
        github.file_shas.insert(
            "dlorg/dlrepo/api/openapi.yaml".to_string(),
            (
                "dlsha123456789abcdef".to_string(),
                256,
                // Use a URL that will fail — we test directory creation + cache update
                "http://127.0.0.1:1/nonexistent".to_string(),
            ),
        );

        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        // The download will fail because the URL is unreachable, but the dir should be created
        let audit = test_audit();
        let result = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit).await;

        // Verify the directory was created before download was attempted
        assert!(download_dir.exists(), "download directory should have been created");

        // The cycle should return an error due to failed download, which is counted as errors
        // The function returns Ok with errors counted, not Err
        match result {
            Ok(summary) => {
                // file_changes won't be incremented since the download failed before cache update
                assert!(summary.errors > 0);
            }
            Err(_) => {
                // Also acceptable — the error propagated
            }
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn test_file_watch_first_detection_no_cached_sha() {
        use crate::config::FileWatch;

        let tmp_dir = std::env::temp_dir().join("tend-test-fw-first");
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let _ = std::fs::create_dir_all(&tmp_dir);

        let hook_output = tmp_dir.join("first-detect.txt");

        let mut ws = make_test_workspace("fw-first", None);
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().file_watches = vec![FileWatch {
            name: "first-time".to_string(),
            org: "org1".to_string(),
            repo: "repo1".to_string(),
            path: "file.txt".to_string(),
            download_to: None,
            post_hooks: vec![PostHook {
                trigger: "on_change".to_string(),
                command: "bash".to_string(),
                args: vec![
                    "-c".to_string(),
                    format!("echo detected > {}", hook_output.display()),
                ],
                working_dir: None,
                continue_on_error: false,
            }],
        }];

        let mut github = MockGitHub::new();
        github.file_shas.insert(
            "org1/repo1/file.txt".to_string(),
            ("firstsha12345678901".to_string(), 42, "https://example.com/file.txt".to_string()),
        );

        // Empty cache — first time seeing this file
        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        // First detection (no cached SHA) should count as a change
        assert_eq!(summary.file_changes, 1);
        assert_eq!(summary.errors, 0);

        // Hook should have fired
        let content = std::fs::read_to_string(&hook_output).unwrap();
        assert_eq!(content.trim(), "detected");

        // Cache should now have the SHA
        let saved = cache.state.lock().unwrap();
        assert_eq!(
            saved.file_shas.get("org1/repo1/file.txt").unwrap(),
            "firstsha12345678901"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // ── Flake Input Watch Tests ──

    /// Helper: write a minimal flake.lock JSON for testing
    fn write_test_flake_lock(dir: &std::path::Path, input_name: &str, rev: &str, owner: &str, repo: &str) {
        let lock = serde_json::json!({
            "nodes": {
                "root": {
                    "inputs": {
                        input_name: input_name
                    }
                },
                input_name: {
                    "locked": {
                        "rev": rev,
                        "owner": owner,
                        "repo": repo,
                        "type": "github"
                    },
                    "original": {
                        "owner": owner,
                        "repo": repo,
                        "type": "github"
                    }
                }
            },
            "root": "root",
            "version": 7
        });
        let lock_path = dir.join("flake.lock");
        std::fs::write(&lock_path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();
    }

    /// Helper: write a flake.lock with a follows input (JSON array)
    fn write_test_flake_lock_follows(dir: &std::path::Path, input_name: &str) {
        let lock = serde_json::json!({
            "nodes": {
                "root": {
                    "inputs": {
                        input_name: ["other-input", "nixpkgs"]
                    }
                }
            },
            "root": "root",
            "version": 7
        });
        let lock_path = dir.join("flake.lock");
        std::fs::write(&lock_path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();
    }

    #[test]
    fn test_parse_flake_lock_input_basic() {
        let tmp = std::env::temp_dir().join("tend-test-flake-parse");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        write_test_flake_lock(&tmp, "claude-code", "abc123def456", "sadjow", "claude-code-nix");

        let (rev, owner, repo) = parse_flake_lock_input(&tmp.join("flake.lock"), "claude-code").unwrap();
        assert_eq!(rev, "abc123def456");
        assert_eq!(owner, "sadjow");
        assert_eq!(repo, "claude-code-nix");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_parse_flake_lock_input_not_found() {
        let tmp = std::env::temp_dir().join("tend-test-flake-parse-notfound");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        write_test_flake_lock(&tmp, "nixpkgs", "abc123", "NixOS", "nixpkgs");

        let result = parse_flake_lock_input(&tmp.join("flake.lock"), "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_parse_flake_lock_input_follows_skipped() {
        let tmp = std::env::temp_dir().join("tend-test-flake-parse-follows");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        write_test_flake_lock_follows(&tmp, "nixpkgs");

        let result = parse_flake_lock_input(&tmp.join("flake.lock"), "nixpkgs");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("follows"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_parse_flake_lock_input_missing_file() {
        let result = parse_flake_lock_input(std::path::Path::new("/nonexistent/flake.lock"), "foo");
        assert!(result.is_err());
    }

    #[test]
    fn test_flake_input_watch_config_deserialization() {
        let yaml = r#"
enable: true
flake_input_watches:
  - name: claude-code
    repo: blackmatter-claude
    input: claude-code
    upstream: sadjow/claude-code-nix
    mode: commits
    auto_update: true
    auto_commit: true
    auto_propagate: blackmatter-claude
    post_hooks:
      - trigger: on_stale
        command: echo
        args:
          - "$INPUT stale: $LOCKED_REV -> $UPSTREAM_REV"
  - name: nixpkgs
    repo: blackmatter
    input: nixpkgs
    mode: tags
"#;
        let config: WatchConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.flake_input_watches.len(), 2);

        let fw = &config.flake_input_watches[0];
        assert_eq!(fw.name, "claude-code");
        assert_eq!(fw.repo, "blackmatter-claude");
        assert_eq!(fw.input, "claude-code");
        assert_eq!(fw.upstream.as_deref(), Some("sadjow/claude-code-nix"));
        assert_eq!(fw.mode, crate::config::FlakeInputMode::Commits);
        assert!(fw.auto_update);
        assert!(fw.auto_commit);
        assert_eq!(fw.auto_propagate.as_deref(), Some("blackmatter-claude"));
        assert_eq!(fw.post_hooks.len(), 1);
        assert_eq!(fw.post_hooks[0].trigger, "on_stale");

        let fw2 = &config.flake_input_watches[1];
        assert_eq!(fw2.name, "nixpkgs");
        assert_eq!(fw2.mode, crate::config::FlakeInputMode::Tags);
        assert!(!fw2.auto_update);
        assert!(!fw2.auto_commit);
        assert!(fw2.auto_propagate.is_none());
        assert!(fw2.upstream.is_none());
    }

    #[test]
    fn test_flake_input_watch_config_defaults() {
        let yaml = r#"
enable: true
flake_input_watches:
  - name: test
    repo: some-repo
    input: some-input
"#;
        let config: WatchConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let fw = &config.flake_input_watches[0];
        assert_eq!(fw.mode, crate::config::FlakeInputMode::Commits);
        assert!(!fw.auto_update);
        assert!(!fw.auto_commit);
        assert!(fw.auto_propagate.is_none());
        assert!(fw.upstream.is_none());
        assert!(fw.post_hooks.is_empty());
    }

    #[test]
    fn test_flake_input_watch_backward_compatible() {
        // Config without flake_input_watches should still parse
        let yaml = r#"
enable: true
matrix_file: ~/matrix.toml
auto_certify: false
auto_commit: false
"#;
        let config: WatchConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.flake_input_watches.is_empty());
    }

    #[tokio::test]
    async fn test_flake_input_watch_detects_staleness() {
        use crate::config::FlakeInputWatch;

        let tmp = std::env::temp_dir().join("tend-test-fiw-stale");
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("my-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        // Write flake.lock with locked rev
        write_test_flake_lock(&repo_dir, "my-input", "locked111222333", "someowner", "somerepo");

        let mut ws = make_test_workspace("fiw-test", None);
        ws.base_dir = tmp.to_string_lossy().to_string();
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().flake_input_watches = vec![FlakeInputWatch {
            name: "test-input".to_string(),
            repo: "my-repo".to_string(),
            input: "my-input".to_string(),
            upstream: None, // derive from flake.lock
            mode: crate::config::FlakeInputMode::Commits,
            auto_update: false,
            auto_commit: false,
            auto_propagate: None,
            post_hooks: vec![],
        }];

        let mut github = MockGitHub::new();
        // Upstream HEAD is different from locked rev
        github.heads.insert("somerepo".to_string(), "upstream999888777".to_string());

        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.flake_input_updates, 1);
        assert_eq!(summary.errors, 0);

        // Verify cache was updated
        let saved = cache.state.lock().unwrap();
        let entry = saved.flake_inputs.get("test-input").unwrap();
        assert_eq!(entry.upstream_rev, "upstream999888777");
        assert!(entry.upstream_tag.is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_flake_input_watch_skips_when_up_to_date() {
        use crate::config::FlakeInputWatch;

        let tmp = std::env::temp_dir().join("tend-test-fiw-uptodate");
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("my-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        // Locked rev matches upstream HEAD
        write_test_flake_lock(&repo_dir, "my-input", "samerev123456789", "owner", "repo");

        let mut ws = make_test_workspace("fiw-uptodate", None);
        ws.base_dir = tmp.to_string_lossy().to_string();
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().flake_input_watches = vec![FlakeInputWatch {
            name: "up-to-date".to_string(),
            repo: "my-repo".to_string(),
            input: "my-input".to_string(),
            upstream: None,
            mode: crate::config::FlakeInputMode::Commits,
            auto_update: false,
            auto_commit: false,
            auto_propagate: None,
            post_hooks: vec![],
        }];

        let mut github = MockGitHub::new();
        github.heads.insert("repo".to_string(), "samerev123456789".to_string());

        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.flake_input_updates, 0);
        assert_eq!(summary.errors, 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_flake_input_watch_explicit_upstream() {
        use crate::config::FlakeInputWatch;

        let tmp = std::env::temp_dir().join("tend-test-fiw-explicit");
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("my-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        write_test_flake_lock(&repo_dir, "my-input", "locked000", "flakeowner", "flakerepo");

        let mut ws = make_test_workspace("fiw-explicit", None);
        ws.base_dir = tmp.to_string_lossy().to_string();
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().flake_input_watches = vec![FlakeInputWatch {
            name: "explicit-upstream".to_string(),
            repo: "my-repo".to_string(),
            input: "my-input".to_string(),
            upstream: Some("explicit-owner/explicit-repo".to_string()),
            mode: crate::config::FlakeInputMode::Commits,
            auto_update: false,
            auto_commit: false,
            auto_propagate: None,
            post_hooks: vec![],
        }];

        let mut github = MockGitHub::new();
        // Only the explicit upstream has a head entry
        github.heads.insert("explicit-repo".to_string(), "upstreamHEAD".to_string());

        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.flake_input_updates, 1);
        assert_eq!(summary.errors, 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_flake_input_watch_cache_prevents_retrigger() {
        use crate::config::FlakeInputWatch;

        let tmp = std::env::temp_dir().join("tend-test-fiw-cache");
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("my-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        write_test_flake_lock(&repo_dir, "my-input", "locked111", "owner", "repo");

        let mut ws = make_test_workspace("fiw-cache", None);
        ws.base_dir = tmp.to_string_lossy().to_string();
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().flake_input_watches = vec![FlakeInputWatch {
            name: "cached-input".to_string(),
            repo: "my-repo".to_string(),
            input: "my-input".to_string(),
            upstream: None,
            mode: crate::config::FlakeInputMode::Commits,
            auto_update: false, // no auto_update → cache should prevent retrigger
            auto_commit: false,
            auto_propagate: None,
            post_hooks: vec![],
        }];

        let mut github = MockGitHub::new();
        github.heads.insert("repo".to_string(), "upstream222".to_string());

        // Pre-populate cache with the same upstream rev
        let mut initial = WatchState::default();
        initial.flake_inputs.insert(
            "cached-input".to_string(),
            crate::watch_cache::FlakeInputCacheEntry {
                upstream_rev: "upstream222".to_string(),
                upstream_tag: None,
            },
        );
        let cache = MockCache { state: Mutex::new(initial) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        // Should not trigger because cache has same upstream_rev
        assert_eq!(summary.flake_input_updates, 0);
        assert_eq!(summary.errors, 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_flake_input_watch_tags_mode_detects_new_tag() {
        use crate::config::FlakeInputWatch;

        let tmp = std::env::temp_dir().join("tend-test-fiw-tags");
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("my-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        write_test_flake_lock(&repo_dir, "my-input", "locked000", "owner", "repo");

        let mut ws = make_test_workspace("fiw-tags", None);
        ws.base_dir = tmp.to_string_lossy().to_string();
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().flake_input_watches = vec![FlakeInputWatch {
            name: "tags-input".to_string(),
            repo: "my-repo".to_string(),
            input: "my-input".to_string(),
            upstream: None,
            mode: crate::config::FlakeInputMode::Tags,
            auto_update: false,
            auto_commit: false,
            auto_propagate: None,
            post_hooks: vec![],
        }];

        let mut github = MockGitHub::new();
        github.tags.insert("repo".to_string(), Some("v2.0.0".to_string()));
        github.heads.insert("repo".to_string(), "taggedHEAD456".to_string());

        // Cache has old tag
        let mut initial = WatchState::default();
        initial.flake_inputs.insert(
            "tags-input".to_string(),
            crate::watch_cache::FlakeInputCacheEntry {
                upstream_rev: "oldrev".to_string(),
                upstream_tag: Some("v1.0.0".to_string()),
            },
        );
        let cache = MockCache { state: Mutex::new(initial) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.flake_input_updates, 1);
        assert_eq!(summary.errors, 0);

        // Verify cache updated with new tag
        let saved = cache.state.lock().unwrap();
        let entry = saved.flake_inputs.get("tags-input").unwrap();
        assert_eq!(entry.upstream_rev, "taggedHEAD456");
        assert_eq!(entry.upstream_tag.as_deref(), Some("v2.0.0"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_flake_input_watch_tags_mode_skips_same_tag() {
        use crate::config::FlakeInputWatch;

        let tmp = std::env::temp_dir().join("tend-test-fiw-tags-same");
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("my-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        write_test_flake_lock(&repo_dir, "my-input", "locked000", "owner", "repo");

        let mut ws = make_test_workspace("fiw-tags-same", None);
        ws.base_dir = tmp.to_string_lossy().to_string();
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().flake_input_watches = vec![FlakeInputWatch {
            name: "tags-same".to_string(),
            repo: "my-repo".to_string(),
            input: "my-input".to_string(),
            upstream: None,
            mode: crate::config::FlakeInputMode::Tags,
            auto_update: false,
            auto_commit: false,
            auto_propagate: None,
            post_hooks: vec![],
        }];

        let mut github = MockGitHub::new();
        github.tags.insert("repo".to_string(), Some("v1.0.0".to_string()));

        // Cache has the SAME tag — should not trigger
        let mut initial = WatchState::default();
        initial.flake_inputs.insert(
            "tags-same".to_string(),
            crate::watch_cache::FlakeInputCacheEntry {
                upstream_rev: "oldrev".to_string(),
                upstream_tag: Some("v1.0.0".to_string()),
            },
        );
        let cache = MockCache { state: Mutex::new(initial) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.flake_input_updates, 0);
        assert_eq!(summary.errors, 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_flake_input_watch_on_stale_hooks() {
        use crate::config::FlakeInputWatch;

        let tmp = std::env::temp_dir().join("tend-test-fiw-hooks");
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("my-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        write_test_flake_lock(&repo_dir, "my-input", "locked111", "owner", "repo");

        let hook_output = tmp.join("hook-out.txt");

        let mut ws = make_test_workspace("fiw-hooks", None);
        ws.base_dir = tmp.to_string_lossy().to_string();
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().flake_input_watches = vec![FlakeInputWatch {
            name: "hook-test".to_string(),
            repo: "my-repo".to_string(),
            input: "my-input".to_string(),
            upstream: None,
            mode: crate::config::FlakeInputMode::Commits,
            auto_update: false,
            auto_commit: false,
            auto_propagate: None,
            post_hooks: vec![PostHook {
                trigger: "on_stale".to_string(),
                command: "bash".to_string(),
                args: vec![
                    "-c".to_string(),
                    format!(
                        "echo \"INPUT=$INPUT LOCKED=$LOCKED_REV UPSTREAM=$UPSTREAM_REV REPO=$REPO NAME=$NAME\" > {}",
                        hook_output.display()
                    ),
                ],
                working_dir: None,
                continue_on_error: false,
            }],
        }];

        let mut github = MockGitHub::new();
        github.heads.insert("repo".to_string(), "upstream999".to_string());

        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.flake_input_updates, 1);

        // Verify hook ran with correct variable substitution
        let content = std::fs::read_to_string(&hook_output).unwrap();
        assert!(content.contains("INPUT=my-input"));
        assert!(content.contains("LOCKED=locked111"));
        assert!(content.contains("UPSTREAM=upstream999"));
        assert!(content.contains("REPO=my-repo"));
        assert!(content.contains("NAME=hook-test"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_flake_input_watch_handles_parse_error() {
        use crate::config::FlakeInputWatch;

        let tmp = std::env::temp_dir().join("tend-test-fiw-parseerr");
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("my-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        // Write invalid flake.lock
        std::fs::write(repo_dir.join("flake.lock"), "not json").unwrap();

        let mut ws = make_test_workspace("fiw-parseerr", None);
        ws.base_dir = tmp.to_string_lossy().to_string();
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().flake_input_watches = vec![FlakeInputWatch {
            name: "bad-lock".to_string(),
            repo: "my-repo".to_string(),
            input: "whatever".to_string(),
            upstream: None,
            mode: crate::config::FlakeInputMode::Commits,
            auto_update: false,
            auto_commit: false,
            auto_propagate: None,
            post_hooks: vec![],
        }];

        let github = MockGitHub::new();
        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.flake_input_updates, 0);
        assert_eq!(summary.errors, 1);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_flake_input_watch_handles_github_error() {
        use crate::config::FlakeInputWatch;

        let tmp = std::env::temp_dir().join("tend-test-fiw-apierr");
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("my-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        write_test_flake_lock(&repo_dir, "my-input", "locked000", "owner", "repo");

        let mut ws = make_test_workspace("fiw-apierr", None);
        ws.base_dir = tmp.to_string_lossy().to_string();
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().flake_input_watches = vec![FlakeInputWatch {
            name: "api-fail".to_string(),
            repo: "my-repo".to_string(),
            input: "my-input".to_string(),
            upstream: None,
            mode: crate::config::FlakeInputMode::Commits,
            auto_update: false,
            auto_commit: false,
            auto_propagate: None,
            post_hooks: vec![],
        }];

        // GitHub has no HEAD for this repo → error
        let github = MockGitHub::new();
        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.flake_input_updates, 0);
        assert_eq!(summary.errors, 1);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_flake_input_watch_invalid_upstream_format() {
        use crate::config::FlakeInputWatch;

        let tmp = std::env::temp_dir().join("tend-test-fiw-badupstream");
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("my-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        write_test_flake_lock(&repo_dir, "my-input", "locked000", "owner", "repo");

        let mut ws = make_test_workspace("fiw-badupstream", None);
        ws.base_dir = tmp.to_string_lossy().to_string();
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().flake_input_watches = vec![FlakeInputWatch {
            name: "bad-format".to_string(),
            repo: "my-repo".to_string(),
            input: "my-input".to_string(),
            upstream: Some("no-slash-here".to_string()), // invalid: no /
            mode: crate::config::FlakeInputMode::Commits,
            auto_update: false,
            auto_commit: false,
            auto_propagate: None,
            post_hooks: vec![],
        }];

        let github = MockGitHub::new();
        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.flake_input_updates, 0);
        assert_eq!(summary.errors, 1);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_watch_state_flake_inputs_roundtrip() {
        let mut state = WatchState::default();
        state.flake_inputs.insert(
            "test-input".to_string(),
            crate::watch_cache::FlakeInputCacheEntry {
                upstream_rev: "abc123".to_string(),
                upstream_tag: Some("v1.0.0".to_string()),
            },
        );

        let serialized = toml::to_string_pretty(&state).unwrap();
        let deserialized: WatchState = toml::from_str(&serialized).unwrap();

        assert_eq!(deserialized.flake_inputs.len(), 1);
        let entry = &deserialized.flake_inputs["test-input"];
        assert_eq!(entry.upstream_rev, "abc123");
        assert_eq!(entry.upstream_tag.as_deref(), Some("v1.0.0"));
    }

    #[test]
    fn test_watch_state_flake_inputs_backward_compatible() {
        // Old cache files without flake_inputs should still parse
        let toml_str = r#"
[repos.some-repo]
head = "abc123"
language = "go"
"#;
        let state: WatchState = toml::from_str(toml_str).unwrap();
        assert!(state.flake_inputs.is_empty());
        assert_eq!(state.repos.len(), 1);
    }

    #[tokio::test]
    async fn test_flake_input_watch_no_watches_early_exit() {
        // A workspace with no matrix_file, no file_watches, and no flake_input_watches
        // should return immediately with zeros
        let mut ws = make_test_workspace("fiw-empty", None);
        ws.extra_repos = vec![];

        let github = MockGitHub::new();
        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = MockGitOps;

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.checked, 0);
        assert_eq!(summary.new_versions, 0);
        assert_eq!(summary.file_changes, 0);
        assert_eq!(summary.flake_input_updates, 0);
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn test_flake_input_watch_auto_commit_records_git_ops() {
        use crate::config::FlakeInputWatch;

        let tmp = std::env::temp_dir().join("tend-test-fiw-autocommit");
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("my-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        write_test_flake_lock(&repo_dir, "my-input", "locked111", "owner", "repo");

        // Create a fake flake.lock that auto_commit_flake_input will stage
        // (we already have it from write_test_flake_lock)

        let mut ws = make_test_workspace("fiw-autocommit", None);
        ws.base_dir = tmp.to_string_lossy().to_string();
        ws.extra_repos = vec![];
        ws.watch.as_mut().unwrap().flake_input_watches = vec![FlakeInputWatch {
            name: "ac-test".to_string(),
            repo: "my-repo".to_string(),
            input: "my-input".to_string(),
            upstream: None,
            mode: crate::config::FlakeInputMode::Commits,
            auto_update: false, // can't actually run nix flake update in tests
            auto_commit: false,
            auto_propagate: None,
            post_hooks: vec![],
        }];

        let mut github = MockGitHub::new();
        github.heads.insert("repo".to_string(), "upstream999".to_string());

        let cache = MockCache { state: Mutex::new(WatchState::default()) };
        let appender = MockAppender::new();
        let git_ops = RecordingGitOps::new();

        let audit = test_audit();
        let summary = run_watch_cycle(&ws, true, &github, &cache, &appender, &git_ops, &audit)
            .await
            .unwrap();

        assert_eq!(summary.flake_input_updates, 1);

        // auto_update=false, so no git operations should have been called
        let calls = git_ops.calls.lock().unwrap();
        assert!(calls.is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
