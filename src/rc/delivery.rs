//! Durable outbound delivery, independent of the Socket Mode reader. SQLite is
//! the queue: a wakeup is only a hint, so losing one (or restarting) loses no work.

use anyhow::Result;
use chrono::{Duration, Utc};
use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration as Wait;

use super::daemon::log;
use super::slack::{self, Slack};
use super::store::{self, StateDb};

pub(super) struct Worker {
    wake: SyncSender<()>,
    thread: JoinHandle<()>,
}

impl Worker {
    pub(super) fn start(token: String) -> Result<Self> {
        let state = store::open_state()?;
        store::reset_sessions(&state)?;
        let (wake, receiver) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("nibble-rc-delivery".into())
            .spawn(move || run(state, Slack::new(&token), receiver))?;
        Ok(Self { wake, thread })
    }

    pub(super) fn wake(&self) {
        let _ = self.wake.try_send(());
    }

    pub(super) fn finished(&self) -> bool {
        self.thread.is_finished()
    }
}

fn run(state: StateDb, bot: Slack, receiver: Receiver<()>) {
    loop {
        if let Err(e) = pass(&state, &bot) {
            log(&format!("delivery queue failed: {e:#}"));
        }
        match receiver.recv_timeout(Wait::from_secs(1)) {
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            _ => continue,
        }
    }
}

fn pass(state: &StateDb, bot: &Slack) -> Result<()> {
    let mut blocked = HashSet::new();
    let mut attempted = 0;
    // Small batches avoid monopolising SQLite on output-heavy turns. Preserve
    // channel ordering, but a retrying channel must not starve other channels.
    for mut delivery in store::deliveries(state)?.into_iter().filter(|d| !d.failed) {
        if delivery.method.starts_with("agents.sessions.") && store::sessions_disabled(state)? {
            store::delete_delivery(state, &delivery.id)?;
            continue;
        }
        if blocked.contains(&delivery.channel) {
            continue;
        }
        if delivery.due > Utc::now() {
            blocked.insert(delivery.channel.clone());
            continue;
        }
        if attempted >= 32 {
            break;
        }
        attempted += 1;
        let params: Vec<_> = delivery
            .params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        match bot.call(&delivery.method, &params) {
            Ok(_) => store::delete_delivery(state, &delivery.id)?,
            Err(e) if slack::error_code(&e) == Some("already_reacted") => {
                store::delete_delivery(state, &delivery.id)?;
            }
            Err(e) => {
                delivery.attempts = delivery.attempts.saturating_add(1);
                delivery.last_error = Some(format!("{e:#}"));
                if let Some(minimum) = slack::retry_delay(&e) {
                    let backoff = 1_u64 << delivery.attempts.min(6);
                    // Honour Retry-After instead of clamping it to 30s. Bound
                    // only absurd values to keep timestamp arithmetic safe.
                    let seconds = minimum.max(backoff).min(31_536_000);
                    delivery.due = Utc::now() + Duration::seconds(seconds as i64);
                    blocked.insert(delivery.channel.clone());
                    log(&format!(
                        "delivery {} failed: {e:#}; retrying in {seconds}s",
                        delivery.id
                    ));
                } else {
                    delivery.failed = true;
                    log(&format!(
                        "delivery {} failed permanently: {e:#}; retained in rc state",
                        delivery.id
                    ));
                }
                store::save_delivery(state, &delivery)?;
                if delivery.method.starts_with("agents.sessions.")
                    && super::daemon::sessions_api_unavailable(slack::error_code(&e))
                {
                    // Preserve the prior behavior: unsupported agent APIs must
                    // not accumulate failures on every turn.
                    store::disable_sessions(state)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::store::Delivery;
    use super::*;

    fn server(
        responses: Vec<(u16, &'static str, Option<&'static str>)>,
    ) -> (Slack, JoinHandle<Vec<String>>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = format!("http://{}", server.server_addr());
        let thread = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for (code, body, retry_after) in responses {
                let mut request = server
                    .recv_timeout(Wait::from_secs(5))
                    .unwrap()
                    .expect("request");
                let mut input = String::new();
                request.as_reader().read_to_string(&mut input).unwrap();
                bodies.push(input);
                let mut response = tiny_http::Response::from_string(body).with_status_code(code);
                if let Some(wait) = retry_after {
                    response
                        .add_header(tiny_http::Header::from_bytes("Retry-After", wait).unwrap());
                }
                request.respond(response).unwrap();
            }
            bodies
        });
        (Slack::for_test(address), thread)
    }

    fn message(text: &str) -> Delivery {
        Delivery::new(
            "D1",
            "chat.postMessage",
            &[("channel", "D1"), ("text", text)],
        )
    }

    #[test]
    fn transient_failure_survives_restart_and_only_unsent_chunks_retry() {
        let dir = tempfile::tempdir().unwrap();
        let state = store::open_state_in(dir.path()).unwrap();
        let first = message("chunk-one");
        let second = message("chunk-two");
        store::enqueue(&state, &[first, second]).unwrap();
        let (bot, thread) = server(vec![
            (200, "{\"ok\":true}", None),
            (503, "unavailable", None),
        ]);
        pass(&state, &bot).unwrap();
        let bodies = thread.join().unwrap();
        assert!(bodies[0].contains("chunk-one"));
        assert!(bodies[1].contains("chunk-two"));
        drop(state);
        let state = store::open_state_in(dir.path()).unwrap();
        let mut pending = store::deliveries(&state).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempts, 1);
        pending[0].due = Utc::now();
        store::save_delivery(&state, &pending[0]).unwrap();
        let (bot, thread) = server(vec![(200, "{\"ok\":true}", None)]);
        pass(&state, &bot).unwrap();
        assert!(thread.join().unwrap()[0].contains("chunk-two"));
        assert!(store::deliveries(&state).unwrap().is_empty());
    }

    #[test]
    fn rate_limit_is_scheduled_without_sleeping_and_preserves_order() {
        let dir = tempfile::tempdir().unwrap();
        let state = store::open_state_in(dir.path()).unwrap();
        store::enqueue(&state, &[message("one"), message("two")]).unwrap();
        let (bot, thread) = server(vec![(429, "limited", Some("120"))]);
        pass(&state, &bot).unwrap();
        assert_eq!(thread.join().unwrap().len(), 1);
        let pending = store::deliveries(&state).unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending[0].due > Utc::now() + Duration::seconds(110));
        assert_eq!(pending[1].attempts, 0);
    }

    #[test]
    fn permanent_error_is_retained_without_blocking_later_messages() {
        let dir = tempfile::tempdir().unwrap();
        let state = store::open_state_in(dir.path()).unwrap();
        store::enqueue(&state, &[message("bad"), message("good")]).unwrap();
        let (bot, thread) = server(vec![
            (200, "{\"ok\":false,\"error\":\"invalid_blocks\"}", None),
            (200, "{\"ok\":true}", None),
        ]);
        pass(&state, &bot).unwrap();
        assert_eq!(thread.join().unwrap().len(), 2);
        let pending = store::deliveries(&state).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].failed);
    }

    #[test]
    fn slow_delivery_does_not_block_producers_or_lose_wakeups() {
        let dir = tempfile::tempdir().unwrap();
        let state = store::open_state_in(dir.path()).unwrap();
        store::enqueue(&state, &[message("slow")]).unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let bot = Slack::for_test(format!("http://{}", server.server_addr()));
        let worker_state = store::open_state_in(dir.path()).unwrap();
        let (wake, receiver) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || run(worker_state, bot, receiver));
        let first = server.recv_timeout(Wait::from_secs(5)).unwrap().unwrap();
        // While HTTP is stalled, the producer can atomically checkpoint a
        // transcript burst. A saturated wake channel must never lose records.
        let link = crate::rc::test_support::link("burst", "1.1");
        let burst: Vec<_> = (0..100).map(|i| message(&format!("burst-{i}"))).collect();
        store::save_relay(&state, &link, &burst).unwrap();
        wake.try_send(()).unwrap();
        assert!(wake.try_send(()).is_err());
        assert_eq!(store::deliveries(&state).unwrap().len(), 101);
        first
            .respond(tiny_http::Response::from_string("{\"ok\":true}"))
            .unwrap();
        for _ in 0..100 {
            server
                .recv_timeout(Wait::from_secs(5))
                .unwrap()
                .unwrap()
                .respond(tiny_http::Response::from_string("{\"ok\":true}"))
                .unwrap();
        }
        drop(wake);
        worker.join().unwrap();
        assert!(store::deliveries(&state).unwrap().is_empty());
    }

    #[test]
    fn connection_refused_keeps_delivery_pending() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        let state = store::open_state_in(dir.path()).unwrap();
        store::enqueue(&state, &[message("retained")]).unwrap();
        pass(&state, &Slack::for_test(format!("http://{address}"))).unwrap();
        let pending = store::deliveries(&state).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(!pending[0].failed);
        assert_eq!(pending[0].attempts, 1);
    }
}
