use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use super::{App, GIT_REMOTE_STATUS_REFRESH_INTERVAL, GIT_REPO_DISCOVERY_REFRESH_INTERVAL};
use crate::events::AppEvent;
use crate::workspace::{
    GitStatusCacheEntry, GitStatusRefreshDemand, PaneGitStatus, WorkspaceGitStatus,
};

/// What consumes a refreshed Git snapshot: a workspace row or one pane.
#[derive(Clone, Debug, PartialEq, Eq)]
enum GitRefreshTarget {
    Workspace {
        workspace_id: String,
        resolved_identity_cwd: PathBuf,
    },
    Pane {
        pane_id: crate::layout::PaneId,
        cwd: PathBuf,
    },
}

impl GitRefreshTarget {
    fn cwd(&self) -> &std::path::Path {
        match self {
            Self::Workspace {
                resolved_identity_cwd,
                ..
            } => resolved_identity_cwd,
            Self::Pane { cwd, .. } => cwd,
        }
    }
}

/// One cwd that needs a refreshed Git snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GitRefreshItem {
    target: GitRefreshTarget,
    cache_key_hint: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitRefreshJob {
    cache_key: PathBuf,
    cached: Option<GitStatusCacheEntry>,
    targets: Vec<GitRefreshTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitRefreshOutput {
    workspace_results: Vec<WorkspaceGitStatus>,
    pane_results: Vec<PaneGitStatus>,
    cache_updates: Vec<(PathBuf, GitStatusCacheEntry)>,
}

impl App {
    pub(crate) fn start_git_status_refresh_if_due(&mut self, now: Instant) {
        let Some(deadline) = self.git_refresh_deadline() else {
            return;
        };

        if now < deadline {
            return;
        }

        let refresh_repo_discovery = self.git_identity_refresh_requested
            || now.saturating_duration_since(self.last_git_repo_discovery_refresh)
                >= GIT_REPO_DISCOVERY_REFRESH_INTERVAL;
        let mut demand = self.git_refresh_demand();
        if self.git_identity_refresh_requested {
            demand.branch = true;
        }
        let items = self.git_refresh_items(refresh_repo_discovery, demand);

        if items.is_empty() {
            self.last_git_remote_status_refresh = now;
            self.git_identity_refresh_requested = false;
            return;
        }

        self.git_refresh_in_flight = true;
        let event_tx = self.event_tx.clone();
        let cache = self.git_status_cache.clone();
        self.git_identity_refresh_requested = false;
        if refresh_repo_discovery {
            self.last_git_repo_discovery_refresh = now;
        }
        std::thread::spawn(move || {
            let output = refresh_git_statuses_with_cache_and_demand(items, &cache, demand);
            let _ = event_tx.blocking_send(AppEvent::GitStatusRefreshed {
                workspace_results: output.workspace_results,
                pane_results: output.pane_results,
                cache_updates: output.cache_updates,
            });
        });
    }

    pub(crate) fn request_git_identity_refresh(&mut self, now: Instant) {
        self.git_identity_refresh_requested = true;
        self.mark_git_status_refresh_due(now);
    }

    pub(crate) fn mark_git_status_refresh_due(&mut self, now: Instant) {
        self.git_status_cache
            .retain(|_, entry| entry.fingerprint.is_some());
        if self.git_refresh_in_flight {
            self.git_refresh_due_after_in_flight = true;
            return;
        }
        self.last_git_remote_status_refresh = now
            .checked_sub(GIT_REMOTE_STATUS_REFRESH_INTERVAL)
            .unwrap_or(now);
        self.git_refresh_due_after_in_flight = false;
    }

    pub(crate) fn git_refresh_deadline(&self) -> Option<Instant> {
        (!self.git_refresh_in_flight
            && !self.state.workspaces.is_empty()
            && (self.git_identity_refresh_requested || !self.git_refresh_demand().is_empty()))
        .then_some(self.last_git_remote_status_refresh + GIT_REMOTE_STATUS_REFRESH_INTERVAL)
    }

    fn git_refresh_demand(&self) -> GitStatusRefreshDemand {
        let mut demand = GitStatusRefreshDemand::default();
        for token in self.state.sidebar_spaces.rows.iter().flatten() {
            match token.parts().0 {
                crate::config::SpaceSidebarToken::Branch => demand.branch = true,
                crate::config::SpaceSidebarToken::GitStatus => demand.ahead_behind = true,
                _ => {}
            }
        }
        // Agent rows consume Git context too. Every configured layout counts,
        // including per-agent overrides, so a `branch` token on an agent row keeps
        // the refresh demand alive when no space row needs it. Without this, a
        // branch configured only on agent rows would be filled once by the cwd
        // identity refresh and then never follow a branch switch.
        //
        // `repo` and `worktree` read the pane's repository context, which rides the
        // same pane refresh, and `demand.branch` is also the gate that collects pane
        // targets at all. Leaving them out would let a layout that shows only those
        // tokens silently never render: `git_refresh_items` would collect no pane.
        for token in std::iter::once(&self.state.sidebar_agents.rows)
            .chain(self.state.sidebar_agents.rows_by_agent.values())
            .flatten()
            .flatten()
        {
            if matches!(
                token.parts().0,
                crate::config::AgentSidebarToken::Branch
                    | crate::config::AgentSidebarToken::Repo
                    | crate::config::AgentSidebarToken::Worktree
            ) {
                demand.branch = true;
            }
        }
        demand
    }

    fn git_refresh_items(
        &self,
        refresh_repo_discovery: bool,
        demand: GitStatusRefreshDemand,
    ) -> Vec<GitRefreshItem> {
        let mut items = Vec::new();
        for ws in &self.state.workspaces {
            let Some(cwd) =
                ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
            else {
                continue;
            };
            let cache_key_hint = (!refresh_repo_discovery && ws.cached_identity_cwd == cwd)
                .then(|| ws.cached_git_status_key.clone());
            items.push(GitRefreshItem {
                target: GitRefreshTarget::Workspace {
                    workspace_id: ws.id.clone(),
                    resolved_identity_cwd: cwd,
                },
                cache_key_hint,
            });
        }
        // Pane context answers the agent `branch`, `repo`, and `worktree` tokens.
        // Collect it only while something displays those, so configurations that do
        // not use them keep the previous per-workspace cost.
        if demand.branch {
            items.extend(self.pane_git_refresh_items());
        }
        items
    }

    fn pane_git_refresh_items(&self) -> Vec<GitRefreshItem> {
        let mut items = Vec::new();
        for ws in &self.state.workspaces {
            for tab in &ws.tabs {
                for pane_id in tab.panes.keys().copied() {
                    let Some(cwd) =
                        ws.pane_git_cwd(pane_id, &self.state.terminals, &self.terminal_runtimes)
                    else {
                        continue;
                    };
                    items.push(GitRefreshItem {
                        target: GitRefreshTarget::Pane { pane_id, cwd },
                        // A pane's cwd can change without the workspace identity
                        // changing, so never trust a cached key hint here.
                        cache_key_hint: None,
                    });
                }
            }
        }
        items
    }
}

fn deduplicate_git_refresh_items(
    items: Vec<GitRefreshItem>,
    cache: &HashMap<PathBuf, GitStatusCacheEntry>,
) -> Vec<GitRefreshJob> {
    let mut indexes = HashMap::<PathBuf, usize>::new();
    let mut jobs = Vec::<GitRefreshJob>::new();

    for item in items {
        let reconcile = item.cache_key_hint.is_none();
        let cache_key = match item.cache_key_hint {
            Some(hint) => hint,
            None => crate::workspace::git_status_cache_key(item.target.cwd())
                .unwrap_or_else(|| item.target.cwd().to_path_buf()),
        };
        if let Some(&index) = indexes.get(&cache_key) {
            jobs[index].cached = jobs[index].cached.take().filter(|_| !reconcile);
            jobs[index].targets.push(item.target);
            continue;
        }

        let cached = cache.get(&cache_key).filter(|_| !reconcile).cloned();
        indexes.insert(cache_key.clone(), jobs.len());
        jobs.push(GitRefreshJob {
            cache_key,
            cached,
            targets: vec![item.target],
        });
    }

    jobs
}

fn refresh_git_statuses_with_cache_and_demand(
    items: Vec<GitRefreshItem>,
    cache: &HashMap<PathBuf, GitStatusCacheEntry>,
    demand: GitStatusRefreshDemand,
) -> GitRefreshOutput {
    let mut workspace_results = Vec::new();
    let mut pane_results = Vec::new();
    let mut cache_updates = Vec::new();

    for job in deduplicate_git_refresh_items(items, cache) {
        let (snapshot, cache_entry) = crate::workspace::git_status_snapshot_for_cwd_with_demand(
            &job.cache_key,
            job.cached.as_ref(),
            demand,
        );
        if let Some(cache_entry) = cache_entry {
            cache_updates.push((job.cache_key.clone(), cache_entry));
        }
        for target in job.targets {
            match target {
                GitRefreshTarget::Workspace {
                    workspace_id,
                    resolved_identity_cwd,
                } => workspace_results.push(snapshot.clone().into_workspace_status(
                    workspace_id,
                    resolved_identity_cwd,
                    job.cache_key.clone(),
                    demand,
                )),
                GitRefreshTarget::Pane { pane_id, cwd } => pane_results.push(PaneGitStatus {
                    pane_id,
                    cwd,
                    demand,
                    branch: snapshot.branch.clone(),
                    repo_name: snapshot.space.as_ref().map(|space| space.repo_name.clone()),
                    is_linked_worktree: snapshot
                        .space
                        .as_ref()
                        .is_some_and(|space| space.is_linked_worktree),
                }),
            }
        }
    }

    GitRefreshOutput {
        workspace_results,
        pane_results,
        cache_updates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

    /// The cwd a workspace-targeted refresh item was collected for.
    fn workspace_target_cwd(item: &GitRefreshItem) -> &std::path::Path {
        match &item.target {
            GitRefreshTarget::Workspace {
                resolved_identity_cwd,
                ..
            } => resolved_identity_cwd,
            other => panic!("expected a workspace target, got {other:?}"),
        }
    }

    #[test]
    fn git_refresh_deduplicates_workspaces_with_same_cache_key() {
        let repo =
            std::env::temp_dir().join(format!("herdr-git-refresh-dedupe-{}", std::process::id()));
        let nested = repo.join("nested");
        let other = repo.join("other");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        std::fs::create_dir_all(&other).expect("create other dir");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .expect("run git init");

        let output = refresh_git_statuses_with_cache_and_demand(
            vec![
                GitRefreshItem {
                    target: GitRefreshTarget::Workspace {
                        workspace_id: "one".into(),
                        resolved_identity_cwd: nested.clone(),
                    },
                    cache_key_hint: None,
                },
                GitRefreshItem {
                    target: GitRefreshTarget::Workspace {
                        workspace_id: "two".into(),
                        resolved_identity_cwd: other.clone(),
                    },
                    cache_key_hint: None,
                },
            ],
            &HashMap::new(),
            GitStatusRefreshDemand::ALL,
        );

        assert_eq!(output.cache_updates.len(), 1);
        assert_eq!(
            output.cache_updates[0].0,
            std::fs::canonicalize(&repo).expect("canonical repo path")
        );
        assert_eq!(output.workspace_results.len(), 2);
        assert_eq!(output.workspace_results[0].workspace_id, "one");
        assert_eq!(output.workspace_results[0].resolved_identity_cwd, nested);
        assert_eq!(output.workspace_results[1].workspace_id, "two");
        assert_eq!(output.workspace_results[1].resolved_identity_cwd, other);

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn shared_root_repo_refresh_keeps_workspace_specific_fallback_labels() {
        let cache_key = PathBuf::from("/");
        let cached = GitStatusCacheEntry {
            fingerprint: None,
            retry_after: Some(Instant::now() + std::time::Duration::from_secs(30)),
            snapshot: crate::workspace::WorkspaceGitStatusSnapshot {
                auto_label: "/".into(),
                branch: Some("main".into()),
                ahead_behind: None,
                space: Some(crate::workspace::GitSpaceMetadata {
                    key: "/.git".into(),
                    checkout_key: "/".into(),
                    repo_name: "repo".into(),
                    repo_root: cache_key.clone(),
                    is_linked_worktree: false,
                }),
            },
        };
        let items = ["alpha", "beta"]
            .into_iter()
            .map(|name| GitRefreshItem {
                target: GitRefreshTarget::Workspace {
                    workspace_id: name.into(),
                    resolved_identity_cwd: cache_key.join(name),
                },
                cache_key_hint: Some(cache_key.clone()),
            })
            .collect();

        let output = refresh_git_statuses_with_cache_and_demand(
            items,
            &HashMap::from([(cache_key, cached)]),
            GitStatusRefreshDemand::ALL,
        );

        assert_eq!(output.cache_updates.len(), 1);
        assert_eq!(output.workspace_results.len(), 2);
        assert_eq!(output.workspace_results[0].auto_label, "alpha");
        assert_eq!(output.workspace_results[1].auto_label, "beta");
        assert_eq!(output.workspace_results[0].branch.as_deref(), Some("main"));
        assert_eq!(output.workspace_results[1].branch.as_deref(), Some("main"));
    }

    #[test]
    fn git_refresh_item_collection_does_not_discover_uncached_cwd() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = std::env::temp_dir().join(format!("herdr-uncached-cwd-{}", std::process::id()));
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.tabs.clear();
        app.state.workspaces.push(ws);

        let items = app.git_refresh_items(false, app.git_refresh_demand());

        assert_eq!(items.len(), 1);
        assert_eq!(workspace_target_cwd(&items[0]), cwd);
        assert_eq!(items[0].cache_key_hint, None);
    }

    #[test]
    fn git_refresh_item_collection_reuses_matching_cached_key() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = PathBuf::from("/repo/deep/nested");
        let cache_key = PathBuf::from("/repo");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.cached_identity_cwd = cwd;
        ws.cached_git_status_key = cache_key.clone();
        ws.tabs.clear();
        app.state.workspaces.push(ws);

        let items = app.git_refresh_items(false, app.git_refresh_demand());

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].cache_key_hint, Some(cache_key));
    }

    #[test]
    fn periodic_repo_discovery_ignores_cached_key_hints() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = PathBuf::from("/repo/deep/nested");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.cached_identity_cwd = cwd;
        ws.cached_git_status_key = PathBuf::from("/repo");
        ws.tabs.clear();
        app.state.workspaces.push(ws);

        let items = app.git_refresh_items(true, app.git_refresh_demand());

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].cache_key_hint, None);
        let cache_key = workspace_target_cwd(&items[0]).to_path_buf();
        let cached = GitStatusCacheEntry {
            fingerprint: None,
            retry_after: None,
            snapshot: crate::workspace::WorkspaceGitStatusSnapshot {
                auto_label: "stale".into(),
                branch: None,
                ahead_behind: None,
                space: None,
            },
        };
        let jobs = deduplicate_git_refresh_items(items, &HashMap::from([(cache_key, cached)]));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cached, None);
    }

    #[test]
    fn cwd_identity_refresh_runs_once_without_sidebar_git_tokens() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();

        app.request_git_identity_refresh(now);

        assert!(app.git_refresh_deadline().is_some());
        app.start_git_status_refresh_if_due(now);
        assert!(app.git_refresh_in_flight);
        assert!(!app.git_identity_refresh_requested);
    }

    #[test]
    fn due_git_refresh_does_not_start_without_sidebar_consumer() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();
        app.last_git_remote_status_refresh = now - GIT_REMOTE_STATUS_REFRESH_INTERVAL;

        app.start_git_status_refresh_if_due(now);

        assert!(!app.git_refresh_in_flight);
        assert!(app.event_rx.try_recv().is_err());
    }

    #[test]
    fn git_refresh_demand_matches_sidebar_rows() {
        let cases = [
            (
                crate::config::SpaceSidebarToken::Workspace,
                GitStatusRefreshDemand::default(),
            ),
            (
                crate::config::SpaceSidebarToken::Branch,
                GitStatusRefreshDemand {
                    branch: true,
                    ahead_behind: false,
                },
            ),
            (
                crate::config::SpaceSidebarToken::GitStatus,
                GitStatusRefreshDemand {
                    branch: false,
                    ahead_behind: true,
                },
            ),
        ];

        for (token, expected) in cases {
            let mut config = crate::config::Config::default();
            config.ui.sidebar.spaces.rows = vec![vec![token.clone()]];
            let mut app = test_app(&config);
            app.state.workspaces.push(Workspace::test_new("test"));

            assert_eq!(app.git_refresh_demand(), expected, "token: {token:?}");
            assert_eq!(
                app.git_refresh_deadline().is_some(),
                !expected.is_empty(),
                "token: {token:?}"
            );
        }
    }

    #[test]
    fn agent_branch_token_alone_keeps_periodic_git_refresh_alive() {
        // Space rows carry no Git token, so only the agent row can create demand.
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        config.ui.sidebar.agents.rows = vec![
            vec![crate::config::AgentSidebarToken::Workspace],
            vec![crate::config::AgentSidebarToken::Branch],
        ];
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));

        assert_eq!(
            app.git_refresh_demand(),
            GitStatusRefreshDemand {
                branch: true,
                ahead_behind: false,
            }
        );
        assert!(app.git_refresh_deadline().is_some());
    }

    #[test]
    fn agent_repo_token_alone_keeps_pane_git_refresh_alive() {
        // `repo` is resolved from the pane's repository context, so it needs the
        // same pane refresh `branch` does. A layout showing only `repo` must still
        // collect pane targets, or the token would render empty forever.
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        config.ui.sidebar.agents.rows = vec![
            vec![crate::config::AgentSidebarToken::Workspace],
            vec![crate::config::AgentSidebarToken::Repo],
        ];
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));

        assert_eq!(
            app.git_refresh_demand(),
            GitStatusRefreshDemand {
                branch: true,
                ahead_behind: false,
            }
        );
        assert!(app.git_refresh_deadline().is_some());
    }

    #[test]
    fn agent_worktree_token_alone_keeps_pane_git_refresh_alive() {
        // The pane-level `worktree` marker is read from the same pane refresh as
        // `repo`, so a marker-only layout needs pane targets collected too.
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        config.ui.sidebar.agents.rows = vec![
            vec![crate::config::AgentSidebarToken::Workspace],
            vec![crate::config::AgentSidebarToken::Worktree],
        ];
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));

        assert_eq!(
            app.git_refresh_demand(),
            GitStatusRefreshDemand {
                branch: true,
                ahead_behind: false,
            }
        );
        assert!(app.git_refresh_deadline().is_some());
    }

    #[test]
    fn agent_row_git_demand_ignores_rows_without_git_tokens() {
        // A layout with no Git token must not resurrect a refresh that has no
        // consumer.
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        config.ui.sidebar.agents.rows = vec![
            vec![crate::config::AgentSidebarToken::Workspace],
            vec![crate::config::AgentSidebarToken::Machine],
        ];
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));

        assert_eq!(app.git_refresh_demand(), GitStatusRefreshDemand::default());
        assert!(app.git_refresh_deadline().is_none());
    }

    #[test]
    fn per_agent_override_rows_contribute_git_demand() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        config.ui.sidebar.agents.rows = vec![vec![crate::config::AgentSidebarToken::Workspace]];
        config.ui.sidebar.agents.rows_by_agent.insert(
            "claude".into(),
            vec![vec![crate::config::AgentSidebarToken::Branch]],
        );
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));

        assert_eq!(
            app.git_refresh_demand(),
            GitStatusRefreshDemand {
                branch: true,
                ahead_behind: false,
            }
        );
    }

    #[test]
    fn unnamed_linked_worktree_does_not_force_periodic_branch_refresh() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        let mut child = Workspace::test_new("test");
        child.custom_name = None;
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo".into(),
            label: "repo".into(),
            repo_root: "/repo".into(),
            checkout_path: "/repo-worktree".into(),
            is_linked_worktree: true,
        });
        app.state.workspaces.push(child);

        assert_eq!(app.git_refresh_deadline(), None);
    }

    #[test]
    fn custom_named_linked_worktree_does_not_require_branch_refresh() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        let mut child = Workspace::test_new("custom");
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo".into(),
            label: "repo".into(),
            repo_root: "/repo".into(),
            checkout_path: "/repo-worktree".into(),
            is_linked_worktree: true,
        });
        app.state.workspaces.push(child);

        assert_eq!(app.git_refresh_deadline(), None);
    }

    #[test]
    fn headless_deadline_can_suppress_git_refresh_timer() {
        let mut app = test_app(&crate::config::Config::default());
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();
        app.last_git_remote_status_refresh = now - GIT_REMOTE_STATUS_REFRESH_INTERVAL;

        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, false),
            None
        );
        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, true),
            Some(now)
        );
    }

    #[test]
    fn explicit_git_refresh_invalidates_cached_non_git_results() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = std::env::temp_dir().join(format!("herdr-git-miss-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let (_, entry) = crate::workspace::git_status_snapshot_for_cwd_with_demand(
            &cwd,
            None,
            GitStatusRefreshDemand::ALL,
        );
        app.git_status_cache
            .insert(cwd.clone(), entry.expect("non-Git cache entry"));

        app.mark_git_status_refresh_due(Instant::now());

        assert!(app.git_status_cache.is_empty());
        std::fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn git_refresh_due_request_survives_in_flight_refresh() {
        let mut app = test_app(&crate::config::Config::default());
        let now = Instant::now();
        app.git_refresh_in_flight = true;

        app.mark_git_status_refresh_due(now);
        assert!(app.git_refresh_due_after_in_flight);

        app.handle_internal_event(AppEvent::GitStatusRefreshed {
            workspace_results: Vec::new(),
            pane_results: Vec::new(),
            cache_updates: Vec::new(),
        });

        assert!(!app.git_refresh_in_flight);
        assert!(!app.git_refresh_due_after_in_flight);
        assert_eq!(app.git_refresh_deadline(), None);

        app.state.workspaces.push(Workspace::test_new("test"));
        let deadline = app
            .git_refresh_deadline()
            .expect("refresh should be due once a workspace exists");
        assert!(deadline <= Instant::now());
    }

    fn test_app(config: &crate::config::Config) -> super::super::App {
        super::super::App::new(
            config,
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    /// Point a workspace's root pane at `cwd` so `pane_git_cwd` resolves there.
    fn app_with_pane_at(
        config: &crate::config::Config,
        cwd: &std::path::Path,
    ) -> super::super::App {
        let cwd = cwd.to_path_buf();
        let mut app = test_app(config);
        let mut ws = Workspace::test_new("pane-cwd");
        ws.identity_cwd = cwd.clone();
        ws.cached_identity_cwd = cwd.clone();
        let root_pane = ws.tabs[0].root_pane;
        let terminal_id = ws.tabs[0]
            .terminal_id(root_pane)
            .expect("root pane terminal")
            .clone();
        app.state.terminals.insert(
            terminal_id.clone(),
            crate::terminal::TerminalState::new(terminal_id, cwd.clone()),
        );
        app.state.workspaces.push(ws);
        app
    }

    #[test]
    fn pane_refresh_reports_each_checkouts_own_branch_and_worktree() {
        let (base, repo, linked) =
            crate::workspace::test_support::create_repo_with_linked_worktree("pane-git-refresh");
        let main_pane = crate::layout::PaneId::alloc();
        let linked_pane = crate::layout::PaneId::alloc();
        let items = vec![
            GitRefreshItem {
                target: GitRefreshTarget::Pane {
                    pane_id: main_pane,
                    cwd: repo.clone(),
                },
                cache_key_hint: None,
            },
            GitRefreshItem {
                target: GitRefreshTarget::Pane {
                    pane_id: linked_pane,
                    cwd: linked.clone(),
                },
                cache_key_hint: None,
            },
        ];

        let output = refresh_git_statuses_with_cache_and_demand(
            items,
            &HashMap::new(),
            GitStatusRefreshDemand::ALL,
        );

        // No workspace row is produced for either pane.
        assert!(output.workspace_results.is_empty());
        assert_eq!(output.pane_results.len(), 2);
        // Each worktree has its own HEAD and index, so they keep separate status
        // caches. Deduplication instead benefits panes sharing one checkout.
        assert_eq!(output.cache_updates.len(), 2);
        let main = output
            .pane_results
            .iter()
            .find(|result| result.pane_id == main_pane)
            .expect("main checkout pane result");
        let worktree = output
            .pane_results
            .iter()
            .find(|result| result.pane_id == linked_pane)
            .expect("linked checkout pane result");
        // The two panes are in the same repository but report different branches.
        assert_ne!(main.branch, worktree.branch, "{:?}", output.pane_results);
        // They share the repository name, which is what a row showing the repo
        // rather than the branch renders.
        let repo_name = main.repo_name.clone().expect("main repo name");
        assert_eq!(worktree.repo_name.as_deref(), Some(repo_name.as_str()));
        // Worktree provenance comes from the checkout on disk, not from Herdr, so a
        // `git worktree add` checkout created outside Herdr is recognized.
        assert!(!main.is_linked_worktree);
        assert!(worktree.is_linked_worktree);

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn pane_refresh_items_are_collected_only_when_something_displays_branch() {
        let base =
            std::env::temp_dir().join(format!("herdr-pane-git-demand-{}", std::process::id()));
        let checkout = base.join("work");
        std::fs::create_dir_all(&checkout).expect("create checkout dir");

        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        config.ui.sidebar.agents.rows = vec![vec![crate::config::AgentSidebarToken::Workspace]];
        let mut app = app_with_pane_at(&config, &checkout);

        let without_branch = app.git_refresh_items(false, app.git_refresh_demand());
        assert!(
            without_branch
                .iter()
                .all(|item| matches!(item.target, GitRefreshTarget::Workspace { .. })),
            "panes must not be inspected when no row shows a branch"
        );

        // Adding the agent `branch` token brings pane context into the refresh.
        app.state.sidebar_agents.rows = vec![vec![crate::config::AgentSidebarToken::Branch]];
        let demand = app.git_refresh_demand();
        let with_branch = app.git_refresh_items(false, demand);
        let pane_items = with_branch
            .iter()
            .filter(|item| matches!(item.target, GitRefreshTarget::Pane { .. }))
            .count();
        assert_eq!(pane_items, 1, "the agent branch row needs pane context");

        let _ = std::fs::remove_dir_all(base);
    }
}
