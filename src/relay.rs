use std::{sync::Arc, collections::{HashSet, HashMap}, time::Instant};
use futures::{channel::mpsc::{channel, Sender}, StreamExt};
use metrics::{increment_counter, histogram};
use serde::Deserialize;
use serde_json::json;
use sigh::PrivateKey;
use tokio::{
    sync::mpsc::Receiver,
};
use crate::{db::Database, send, actor};

#[derive(Deserialize)]
struct Post<'a> {
    pub url: Option<&'a str>,
    pub uri: &'a str,
    pub tags: Option<Vec<Tag<'a>>>,
}

impl Post<'_> {
    pub fn host(&self) -> Option<String> {
        reqwest::Url::parse(self.url?)
            .ok()
            .and_then(|url| url.domain()
                      .map(str::to_lowercase)
            )
    }

    pub fn tags(&self) -> Vec<String> {
        match &self.tags {
            None =>
                vec![],
            Some(tags) =>
                tags.iter()
                .map(|tag| tag.name.to_string())
                .collect()
        }
    }

    fn relay_target_kinds(&self) -> impl Iterator<Item = actor::ActorKind> {
        self.host()
            .into_iter()
            .map(actor::ActorKind::InstanceRelay)
            .chain(
                self.tags()
                    .into_iter()
                    .map(|ref s| actor::ActorKind::from_tag(s))
            )
    }

    pub fn relay_targets(&self, hostname: Arc<String>) -> impl Iterator<Item = actor::Actor> {
        self.relay_target_kinds()
            .map(move |kind| actor::Actor {
                host: hostname.clone(),
                kind,
            })
    }
}

#[derive(Deserialize)]
struct Tag<'a> {
    pub name: &'a str,
}

struct Job {
    body: Arc<Vec<u8>>,
    key_id: String,
    private_key: Arc<PrivateKey>,
}

fn spawn_worker(client: Arc<reqwest::Client>, inbox: String) -> Sender<Job> {
    let (tx, mut rx) = channel(1024);

    tokio::spawn(async move {
        while let Some(Job { key_id, private_key, body }) = rx.next().await {
            tracing::debug!("relay to {}", inbox);
            if let Err(e) = send::send_raw(
                &client, &inbox,
                &key_id, &private_key, body
            ).await {
                tracing::error!("relay::send {:?}", e);
            } else {
                // success
                systemd::daemon::notify(
                    false, [
                        (systemd::daemon::STATE_WATCHDOG, "1")
                    ].iter()
                ).unwrap();
            }
        }

        panic!("Worker dead");
    });

    tx
}

pub fn spawn(
    client: Arc<reqwest::Client>,
    hostname: Arc<String>,
    database: Database,
    private_key: PrivateKey,
    mut stream_rx: Receiver<String>
) {
    let private_key = Arc::new(private_key);

    tokio::spawn(async move {
        let mut workers = HashMap::new();

        while let Some(data) = stream_rx.recv().await {
            let t1 = Instant::now();
            let post: Post = match serde_json::from_str(&data) {
                Ok(post) => post,
                Err(e) => {
                    tracing::error!("parse error: {}", e);
                    tracing::trace!("data: {}", data);
                    continue;
                }
            };
            let post_url = match post.url {
                Some(url) => url,
                // skip reposts
                None => {
                    increment_counter!("relay_posts_total", "action" => "skip");
                    continue;
                }
            };
            let mut seen_actors = HashSet::new();
            let mut seen_inboxes = HashSet::new();
            for actor in post.relay_targets(hostname.clone()) {
                if seen_actors.contains(&actor) {
                    continue;
                }

                let actor_id = actor.uri();
                let body = json!({
                    "@context": "https://www.w3.org/ns/activitystreams",
                    "type": "Announce",
                    "actor": &actor_id,
                    "to": ["https://www.w3.org/ns/activitystreams#Public"],
                    "object": &post.uri,
                    "id": &post_url,
                });
                let body = Arc::new(
                    serde_json::to_vec(&body)
                        .unwrap()
                );
                for inbox in database.get_following_inboxes(&actor_id).await.unwrap() {
                    if seen_inboxes.contains(&inbox) {
                        continue;
                    }
                    seen_inboxes.insert(inbox.clone());

                    let job = Job {
                        body: body.clone(),
                        key_id: actor.key_id(),
                        private_key: private_key.clone(),
                    };
                    let tx = workers.entry(inbox.clone())
                        .or_insert_with(|| spawn_worker(client.clone(), inbox.clone()));
                    let _ = tx.try_send(job);
                }

                seen_actors.insert(actor);
            }
            if seen_inboxes.is_empty() {
                increment_counter!("relay_posts_total", "action" => "no_relay");
            } else {
                increment_counter!("relay_posts_total", "action" => "relay");
            }
            let t2 = Instant::now();
            histogram!("relay_post_duration", t2 - t1);
        }
    });
}
