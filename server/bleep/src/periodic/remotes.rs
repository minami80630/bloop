use std::{
    ops::Not,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use chrono::Utc;
use jsonwebtokens_cognito::KeySet;
use notify_debouncer_mini::{
    new_debouncer_opt,
    notify::{Config, RecommendedWatcher, RecursiveMode},
    DebounceEventResult, Debouncer,
};
use rand::{distributions, thread_rng, Rng};
use tokio::{task::JoinHandle, time::sleep};
use tracing::{debug, error, info, warn};

use crate::{
    env::Feature,
    remotes::{
        self,
        github::{self, Auth},
        CognitoGithubTokenBundle,
    },
    repo::{Backend, RepoRef, SyncStatus},
    Application,
};

const POLL_INTERVAL_MINUTE: &[Duration] = &[
    Duration::from_secs(60),
    Duration::from_secs(3 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(20 * 60),
    Duration::from_secs(30 * 60),
];

pub(crate) async fn sync_github_status(app: Application) {
    const POLL_PERIOD: Duration = POLL_INTERVAL_MINUTE[1];
    const LIVENESS: Duration = Duration::from_secs(1);

    let timeout = || async {
        sleep(LIVENESS).await;
    };

    let timeout_or_update = |last_poll: SystemTime, handle: flume::Receiver<()>| async move {
        loop {
            tokio::select! {
                _ = sleep(POLL_PERIOD) => {
                    debug!("timeout expired; refreshing repositories");
                    return SystemTime::now();
                },
                result = handle.recv_async() => {
                    let now = SystemTime::now();
                    match result {
                        Ok(_) if now.duration_since(last_poll).unwrap() > POLL_PERIOD => {
                            debug!("github credentials changed; refreshing repositories");
                            return now;
                        }
                        Ok(_) => {
                            continue;
                        }
                        Err(flume::RecvError::Disconnected) => {
                            return SystemTime::now();
                        }
                    };
                }
            }
        }
    };

    // In case this is a GitHub App installation, we get the
    // credentials from CLI/config
    update_credentials(&app).await;

    let mut last_poll = UNIX_EPOCH;
    loop {
        let Some(github) = app.credentials.github() else {
            timeout().await;
            continue;
	};
        debug!("credentials exist");

        let Ok(repos) = github.current_repo_list().await else {
            timeout().await;
            continue;
	};
        debug!("repo list updated");

        let updated = app.credentials.github_updated().unwrap();
        let new = github.update_repositories(repos);

        // store the updated credentials here
        app.credentials.set_github(new);

        // then retrieve username & other maintenance
        update_credentials(&app).await;

        // swallow the event that's generated from this update
        _ = updated.recv_async().await;
        last_poll = timeout_or_update(last_poll, updated).await;
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RefreshedAccessToken {
    access_token: String,
}

async fn update_credentials(app: &Application) {
    if app.env.allow(Feature::GithubOrgInstallation) {
        match app.credentials.github().and_then(|c| c.expiry()) {
            // If we have a valid token, do nothing.
            Some(expiry) if expiry > Utc::now() + chrono::Duration::minutes(10) => {}

            _ => {
                if let Err(e) = remotes::github::refresh_github_installation_token(app).await {
                    error!(?e, "failed to get GitHub token");
                }
                if app.credentials.github().is_none() {
                    error!("Error in the matrix");
                }
                info!("Github installation token refreshed!")
            }
        }
    }

    if app.env.allow(Feature::CognitoUserAuth) {
        let Some(github::State {
            auth: github::Auth::OAuth(ref creds),
            ..
        }) = app.credentials.github()
	else {
	    return;
	};

        let cognito_pool_id = app.config.cognito_userpool_id.as_ref().unwrap();
        let (region, _pool_id) = cognito_pool_id.split_once('_').unwrap();
        let keyset = KeySet::new(region, cognito_pool_id).unwrap();
        let verifier = keyset
            .new_access_token_verifier(&[app.config.cognito_client_id.as_ref().unwrap()])
            .build()
            .unwrap();

        let rotate_access_key = match keyset.verify(&creds.access_token, &verifier).await {
            Ok(serde_json::Value::Object(claims)) => {
                let Some(exp) = claims.get("exp").and_then(serde_json::Value::as_u64)
		else {
		    return;
		};

                let expiry_time = UNIX_EPOCH + Duration::from_secs(exp);
                expiry_time - Duration::from_secs(600) < SystemTime::now()
            }
            Ok(_) => {
                error!("invalid access key material; rotating");
                true
            }
            Err(err) => {
                warn!(?err, "failed to validate access token; rotating");
                true
            }
        };

        if rotate_access_key {
            let query_url = format!(
                "{url_base}/refresh_token?refresh_token={token}",
                url_base = app
                    .config
                    .cognito_mgmt_url
                    .as_ref()
                    .expect("auth not configured"),
                token = creds.refresh_token
            );

            let response = match reqwest::get(&query_url).await {
                Ok(res) => res.text().await,
                Err(err) => {
                    warn!(?err, "refreshing bloop token failed");
                    return;
                }
            }
            .context("body");

            let tokens: RefreshedAccessToken = match response
                .and_then(|r| serde_json::from_str(&r).context(format!("json: {r}")))
            {
                Ok(tokens) => tokens,
                Err(err) => {
                    // This is sort-of a wild assumption here, BUT hear me out.
                    //
                    // Refresh tokens are encrypted by Cognito, so
                    // this process can't check expiry.
                    //
                    // Assuming there's a successful HTTP response
                    // (`reqwest::get` above),
                    //
                    // AND the received body can't be decoded,
                    // THEN the server sent a payload that is either:
                    //
                    //  a) unintelligible (eg. "Internal Server Error")
                    //  b) there's some weird network issue at play
                    //     that means we can only partially decode the payload
                    //
                    // IF we ignore b) as something unlikely,
                    // AND we consider all a) events to correspond to
                    // refresh token expiration.
                    //
                    // THEN we log the user out.
                    //
                    error!(?err, "failed to refresh access token. forcing re-login");

                    if app.credentials.remove(&Backend::Github).is_some() {
                        app.credentials.store().unwrap();
                    }

                    return;
                }
            };

            app.credentials
                .set_github(github::State::with_auth(Auth::OAuth(
                    CognitoGithubTokenBundle {
                        access_token: tokens.access_token,
                        refresh_token: creds.refresh_token.clone(),
                        github_access_token: creds.github_access_token.clone(),
                    },
                )));

            app.credentials.store().unwrap();
            info!("new bloop access keys saved");
        }

        let github_expired = if let Some(github) = app.credentials.github() {
            let username = github.validate().await;
            if let Ok(Some(ref user)) = username {
                debug!(?user, "updated user");
                app.credentials.set_user(user.into()).await;
                if let Err(err) = app.credentials.store() {
                    error!(?err, "failed to save user credentials");
                }
            }

            username.is_err()
        } else {
            true
        };

        if github_expired && app.credentials.remove(&Backend::Github).is_some() {
            app.credentials.store().unwrap();
            debug!("github oauth is invalid; credentials removed");
        }
    }
}

pub(crate) async fn check_repo_updates(app: Application) {
    while app.credentials.github().is_none() {
        sleep(Duration::from_millis(100)).await
    }

    let handles: Arc<scc::HashMap<RepoRef, JoinHandle<_>>> = Arc::default();
    loop {
        app.repo_pool
            .scan_async(|reporef, repo| match handles.entry(reporef.to_owned()) {
                scc::hash_map::Entry::Occupied(value) => {
                    if value.get().is_finished() {
                        _ = value.remove_entry();
                    }
                }
                scc::hash_map::Entry::Vacant(vacant) => {
                    if repo.sync_status.indexable() {
                        vacant.insert_entry(tokio::spawn(periodic_repo_poll(
                            app.clone(),
                            reporef.to_owned(),
                        )));
                    }
                }
            })
            .await;

        sleep(Duration::from_secs(5)).await
    }
}

// We only return Option<()> here so we can clean up a bunch of error
// handling code with `?`
//
// In reality this doesn't carry any meaning currently
async fn periodic_repo_poll(app: Application, reporef: RepoRef) -> Option<()> {
    debug!(?reporef, "monitoring repo for changes");
    let mut poller = Poller::start(&app, &reporef)?;

    loop {
        use SyncStatus::*;
        let (last_updated, status) = check_repo(&app, &reporef)?;
        if status.indexable().not() {
            warn!(?status, "skipping indexing of repo");
            return None;
        }

        debug!("starting sync");
        if let Err(err) = app.write_index().block_until_synced(reporef.clone()).await {
            error!(?err, ?reporef, "failed to sync & index repo");
            return None;
        }

        debug!("sync done");
        let (updated, status) = check_repo(&app, &reporef)?;
        if status.indexable().not() {
            warn!(?status, ?reporef, "terminating monitoring for repo");
            return None;
        }

        if last_updated == updated && status == Done {
            let poll_interval = poller.increase_interval();

            debug!(
                ?reporef,
                ?poll_interval,
                "repo unchanged, increasing backoff"
            )
        } else {
            let poll_interval = poller.reset_interval();

            debug!(
                ?reporef,
                ?last_updated,
                ?updated,
                ?poll_interval,
                "repo updated"
            )
        }

        let timeout = sleep(poller.jittery_interval());
        tokio::select!(
            _ = timeout => {
                debug!(?reporef, "reindexing");
                continue;
            },
            _ = poller.git_change() => {
                debug!(?reporef, "git changes triggered reindexing");
                continue;
            }
        );
    }
}

struct Poller {
    poll_interval_index: usize,
    minimum_interval_index: usize,
    git_events: flume::Receiver<()>,
    debouncer: Option<Debouncer<RecommendedWatcher>>,
}

impl Poller {
    fn start(app: &Application, reporef: &RepoRef) -> Option<Self> {
        let mut poll_interval_index = 0;
        let mut minimum_interval_index = 0;

        let (tx, rx) = flume::bounded(10);

        let mut _debouncer = None;
        if app.config.disable_fsevents.not() && reporef.backend() == Backend::Local {
            let git_path = app
                .repo_pool
                .read(reporef, |_, v| v.disk_path.join(".git"))?;

            let mut debouncer = debounced_events(tx);
            debouncer
                .watcher()
                .watch(&git_path, RecursiveMode::Recursive)
                .map_err(|e| {
                    let d = git_path.display();
                    error!(error = %e, path = %d, "path does not exist anymore");
                })
                .ok()?;
            _debouncer = Some(debouncer);

            info!(?reporef, ?git_path, "will reindex repo on git changes");

            poll_interval_index = POLL_INTERVAL_MINUTE.len() - 1;
            minimum_interval_index = POLL_INTERVAL_MINUTE.len() - 1;
        }

        Some(Self {
            poll_interval_index,
            minimum_interval_index,
            debouncer: _debouncer,
            git_events: rx,
        })
    }

    fn increase_interval(&mut self) -> Duration {
        self.poll_interval_index =
            (self.poll_interval_index + 1).min(POLL_INTERVAL_MINUTE.len() - 1);
        self.interval()
    }

    fn reset_interval(&mut self) -> Duration {
        self.poll_interval_index = self.minimum_interval_index;
        self.interval()
    }

    fn interval(&self) -> Duration {
        POLL_INTERVAL_MINUTE[self.poll_interval_index]
    }

    fn jittery_interval(&self) -> Duration {
        let poll_interval = self.interval();

        // add random jitter to avoid contention when jobs start at the same time
        let jitter = thread_rng().sample(distributions::Uniform::new(
            10,
            30 + poll_interval.as_secs() / 2,
        ));
        poll_interval + Duration::from_secs(jitter)
    }

    async fn git_change(&mut self) {
        if self.debouncer.is_some() {
            _ = self.git_events.recv_async().await;
            _ = self.git_events.drain().collect::<Vec<_>>();
        } else {
            loop {
                futures::pending!()
            }
        }
    }
}

fn check_repo(app: &Application, reporef: &RepoRef) -> Option<(u64, SyncStatus)> {
    app.repo_pool.read(reporef, |_, repo| {
        (repo.last_commit_unix_secs, repo.sync_status.clone())
    })
}

fn debounced_events(tx: flume::Sender<()>) -> Debouncer<RecommendedWatcher> {
    new_debouncer_opt(
        Duration::from_secs(5),
        None,
        move |event: DebounceEventResult| match event {
            Ok(events) if events.is_empty().not() => {
                if let Err(e) = tx.send(()) {
                    error!("{e}");
                }
            }
            Ok(_) => debug!("no events received from debouncer"),
            Err(err) => {
                error!(?err, "repository monitoring");
            }
        },
        Config::default().with_compare_contents(true),
    )
    .unwrap()
}
