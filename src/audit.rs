use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use serde::Serialize;

/// Structured audit event for the evolution log.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent {
    pub timestamp: String,
    pub event: String,
    #[serde(flatten)]
    pub data: serde_json::Value,
}

/// Audit logger that appends JSON Lines to a file.
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        Self { path }
    }

    /// Default audit log location: ~/.local/share/tend/audit.jsonl
    pub fn default_path() -> Self {
        let dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from(".local/share"))
            .join("tend");
        Self::new(dir.join("audit.jsonl"))
    }

    /// Return the path of the audit log file.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Log an event. Appends a single JSON line.
    pub fn log(&self, event: &str, data: serde_json::Value) {
        let entry = AuditEvent {
            timestamp: chrono::Utc::now().to_rfc3339(),
            event: event.to_string(),
            data,
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            if let Ok(mut file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    /// Log a version detection event.
    pub fn version_detected(
        &self,
        org: &str,
        repo: &str,
        version: &str,
        rev: &str,
        tracking: &str,
    ) {
        self.log(
            "version_detected",
            serde_json::json!({
                "org": org,
                "repo": repo,
                "version": version,
                "rev": rev,
                "tracking": tracking,
            }),
        );
    }

    /// Log a matrix entry append event.
    pub fn matrix_entry_appended(&self, package: &str, version: &str, status: &str) {
        self.log(
            "matrix_entry_appended",
            serde_json::json!({
                "package": package,
                "version": version,
                "status": status,
            }),
        );
    }

    /// Log a hook execution event.
    pub fn hook_executed(&self, trigger: &str, command: &str, exit_code: i32, duration_ms: u64) {
        self.log(
            "hook_executed",
            serde_json::json!({
                "trigger": trigger,
                "command": command,
                "exit_code": exit_code,
                "duration_ms": duration_ms,
            }),
        );
    }

    /// Log a file change detection event.
    pub fn file_change_detected(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        old_sha: Option<&str>,
        new_sha: &str,
        file_size: u64,
    ) {
        self.log(
            "file_change_detected",
            serde_json::json!({
                "org": org,
                "repo": repo,
                "path": path,
                "old_sha": old_sha,
                "new_sha": new_sha,
                "file_size": file_size,
            }),
        );
    }

    /// Log a spec download event.
    pub fn spec_downloaded(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        sha: &str,
        local_path: &str,
        size: u64,
    ) {
        self.log(
            "spec_downloaded",
            serde_json::json!({
                "org": org,
                "repo": repo,
                "path": path,
                "sha": sha,
                "local_path": local_path,
                "size": size,
            }),
        );
    }

    /// Log a commit push event.
    pub fn commit_pushed(&self, repo: &str, commit: &str, message: &str) {
        self.log(
            "commit_pushed",
            serde_json::json!({
                "repo": repo,
                "commit": commit,
                "message": message,
            }),
        );
    }

    /// Log a flake input staleness detection event.
    pub fn flake_input_stale(
        &self,
        name: &str,
        repo: &str,
        input: &str,
        locked_rev: &str,
        upstream_rev: &str,
    ) {
        self.log(
            "flake_input_stale",
            serde_json::json!({
                "name": name,
                "repo": repo,
                "input": input,
                "locked_rev": locked_rev,
                "upstream_rev": upstream_rev,
            }),
        );
    }

    /// Log a flake refresh event.
    pub fn flake_refreshed(
        &self,
        workspace: &str,
        repo: &str,
        updated: bool,
        duration_ms: u64,
    ) {
        self.log(
            "flake_refreshed",
            serde_json::json!({
                "workspace": workspace,
                "repo": repo,
                "updated": updated,
                "duration_ms": duration_ms,
            }),
        );
    }

    /// Log a certify completion event.
    pub fn certify_complete(&self, package: &str, version: &str, status: &str, duration_ms: u64) {
        self.log(
            "certify_complete",
            serde_json::json!({
                "package": package,
                "version": version,
                "status": status,
                "duration_ms": duration_ms,
            }),
        );
    }

    /// Log a nix-audit completion event (convergence loop integration).
    pub fn nix_audit_completed(&self, total: usize, passing: usize, findings: usize) {
        self.log(
            "nix_audit_completed",
            serde_json::json!({
                "total_repos": total,
                "passing_repos": passing,
                "total_findings": findings,
                "compliance_ratio": if total > 0 { passing as f64 / total as f64 } else { 1.0 },
            }),
        );
    }

    /// Log a nix-audit auto-fix event.
    pub fn nix_audit_fixed(&self, repo: &str, category: &str, message: &str) {
        self.log(
            "nix_audit_fixed",
            serde_json::json!({
                "repo": repo,
                "category": category,
                "message": message,
            }),
        );
    }

    /// Log convergence achievement (compliance_ratio reached 1.0).
    pub fn convergence_achieved(&self, compliance_ratio: f64) {
        self.log(
            "convergence_achieved",
            serde_json::json!({
                "compliance_ratio": compliance_ratio,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    fn temp_audit_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join("tend-audit-test");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!(
            "audit-{}-{n}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn test_audit_log_creates_file() {
        let path = temp_audit_path();
        let audit = AuditLog::new(path.clone());
        audit.log("test_event", serde_json::json!({"key": "value"}));
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_audit_log_appends_valid_json() {
        let path = temp_audit_path();
        let audit = AuditLog::new(path.clone());
        audit.log("test_event", serde_json::json!({"key": "value"}));

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert!(parsed.get("timestamp").is_some());
        assert_eq!(parsed["event"], "test_event");
        assert_eq!(parsed["key"], "value");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_audit_log_multiple_events() {
        let path = temp_audit_path();
        let audit = AuditLog::new(path.clone());
        audit.log("event_1", serde_json::json!({"a": 1}));
        audit.log("event_2", serde_json::json!({"b": 2}));
        audit.log("event_3", serde_json::json!({"c": 3}));

        let file = std::fs::File::open(&path).unwrap();
        let reader = std::io::BufReader::new(file);
        let lines: Vec<String> = reader.lines().map(|l| l.unwrap()).collect();
        assert_eq!(lines.len(), 3);

        // Each line is valid JSON
        for line in &lines {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(parsed.get("timestamp").is_some());
            assert!(parsed.get("event").is_some());
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_audit_log_event_fields() {
        let path = temp_audit_path();
        let audit = AuditLog::new(path.clone());
        audit.version_detected("akeylesslabs", "akeyless-go", "5.0.23", "abc123", "tags");

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["event"], "version_detected");
        assert_eq!(parsed["org"], "akeylesslabs");
        assert_eq!(parsed["repo"], "akeyless-go");
        assert_eq!(parsed["version"], "5.0.23");
        assert_eq!(parsed["rev"], "abc123");
        assert_eq!(parsed["tracking"], "tags");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_audit_log_default_path() {
        let audit = AuditLog::default_path();
        let path = audit.path();
        // Should end with tend/audit.jsonl
        assert!(path.ends_with("tend/audit.jsonl"));
    }

    #[test]
    fn test_audit_log_convenience_methods() {
        let path = temp_audit_path();
        let audit = AuditLog::new(path.clone());

        audit.matrix_entry_appended("akeyless-go-sdk", "5.0.23", "pending");
        audit.hook_executed("on_change", "iac-forge", 0, 10000);
        audit.file_change_detected("akeylesslabs", "akeyless-go", "api/openapi.yaml", Some("abc"), "def", 4247417);
        audit.spec_downloaded("akeylesslabs", "akeyless-go", "api/openapi.yaml", "def", "/tmp/spec.yaml", 4247417);
        audit.commit_pushed("blackmatter-akeyless", "abc123", "chore: certify akeyless-go-sdk 5.0.23");
        audit.certify_complete("akeyless-go-sdk", "5.0.23", "verified", 25000);

        let file = std::fs::File::open(&path).unwrap();
        let reader = std::io::BufReader::new(file);
        let lines: Vec<String> = reader.lines().map(|l| l.unwrap()).collect();
        assert_eq!(lines.len(), 6);

        // Verify each line parses and has the right event type
        let events: Vec<String> = lines
            .iter()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["event"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            events,
            vec![
                "matrix_entry_appended",
                "hook_executed",
                "file_change_detected",
                "spec_downloaded",
                "commit_pushed",
                "certify_complete",
            ]
        );
        let _ = std::fs::remove_file(&path);
    }
}
