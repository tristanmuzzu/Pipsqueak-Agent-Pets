//! Passive follower of Codex Desktop's local task stream. This is an internal,
//! versioned protocol: unknown versions, revision gaps and disconnects discard
//! the projection and leave the rollout adapter in charge. Never sends task,
//! tool, approval or ownership requests. Conversation content is not retained.
#[cfg(unix)]
use crate::state;
use crate::state::Session;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(any(unix, test))]
const MAX_FRAME: usize = 16 * 1024 * 1024;
#[derive(Default)]
struct Bridge {
    connected: bool,
    desired: HashSet<String>,
    tasks: HashMap<String, Projection>,
}
struct Projection {
    #[cfg(any(unix, test))]
    owner: String,
    #[cfg(any(unix, test))]
    revision: u64,
    status: Value,
    changed_ms: u64,
}
fn bridge() -> &'static Mutex<Bridge> {
    static BRIDGE: OnceLock<Mutex<Bridge>> = OnceLock::new();
    BRIDGE.get_or_init(|| Mutex::new(Bridge::default()))
}

pub fn summary() -> Value {
    let b = bridge().lock().unwrap_or_else(|e| e.into_inner());
    json!({"connected": b.connected, "desired_tasks": b.desired.len(), "observed_tasks": b.tasks.values().filter(|p| matches!(p.status["type"].as_str(), Some("active" | "idle"))).count(),
        "active_tasks": b.tasks.values().filter(|p| p.status["type"] == "active").count()})
}

pub fn decorate(sessions: &mut [Session]) {
    let mut b = bridge().lock().unwrap_or_else(|e| e.into_inner());
    b.desired = sessions
        .iter()
        .filter(|s| !s.chat_id.is_empty())
        .take(32)
        .map(|s| s.chat_id.clone())
        .collect();
    for s in sessions {
        if let Some(p) = b.tasks.get(&s.chat_id) {
            p.decorate(s);
        }
    }
}
impl Projection {
    fn decorate(&self, s: &mut Session) {
        let kind = self.status["type"].as_str().unwrap_or("");
        if kind != "active" && kind != "idle" {
            return;
        }
        if !s.codex_question_pending {
            s.waiting_since = 0;
            s.waiting_reason.clear();
        }
        s.pending_since = 0;
        s.pending_tool.clear();
        s.pending_detail.clear();
        s.pending_risk.clear();
        s.pending_agent.clear();
        s.stalled = false;
        if kind == "active" {
            s.state = "running".into();
            s.outcome.clear();
            s.outcome_ms = 0;
            let flags = self.status["activeFlags"].as_array();
            let has = |flag: &str| flags.is_some_and(|f| f.iter().any(|v| v == flag));
            let reason = if has("waitingOnApproval") {
                "Approval needed in Codex"
            } else if has("waitingOnUserInput") {
                "Codex has a question for you"
            } else {
                ""
            };
            if !reason.is_empty() && !s.codex_question_pending {
                s.waiting_since = self.changed_ms;
                s.waiting_reason = reason.into();
            }
        } else {
            // Idle proves work stopped, not that it succeeded. Only a rollout
            // task_complete can set Done.
            s.state = "idle".into();
        }
    }
}

/// Apply only the tiny runtime-status subtree. Still follow every revision:
/// skipping an unrelated patch must not look like a lost status update.
#[cfg(any(unix, test))]
fn patch_status(status: &mut Value, patch: &Value) -> Result<(), ()> {
    let path = patch["path"].as_array().ok_or(())?;
    let op = patch["op"].as_str().ok_or(())?;
    if path.is_empty() {
        if op != "replace" {
            return Err(());
        }
        *status = patch["value"]["threadRuntimeStatus"].clone();
    } else if path[0] == "threadRuntimeStatus" {
        if path.len() == 1 {
            *status = if op == "remove" {
                Value::Null
            } else {
                patch["value"].clone()
            };
        } else if path.len() == 2 {
            let key = path[1].as_str().ok_or(())?;
            let obj = status.as_object_mut().ok_or(())?;
            if op == "remove" {
                obj.remove(key);
            } else if op == "replace" || op == "add" {
                obj.insert(key.into(), patch["value"].clone());
            } else {
                return Err(());
            }
        } else if path.len() == 3 && path[1] == "activeFlags" {
            let index = path[2].as_u64().ok_or(())? as usize;
            let flags = status["activeFlags"].as_array_mut().ok_or(())?;
            match op {
                "add" if index <= flags.len() => flags.insert(index, patch["value"].clone()),
                "replace" if index < flags.len() => flags[index] = patch["value"].clone(),
                "remove" if index < flags.len() => {
                    flags.remove(index);
                }
                _ => return Err(()),
            }
        } else {
            return Err(());
        }
    }
    Ok(())
}
#[cfg(any(unix, test))]
fn valid_status(status: &Value) -> bool {
    matches!(
        status["type"].as_str(),
        Some("active" | "idle" | "notLoaded" | "systemError")
    ) && (status["type"] != "active" || status["activeFlags"].is_array())
}
impl Bridge {
    /// Returns an ID needing a fresh snapshot after an incompatible change.
    #[cfg(any(unix, test))]
    fn receive(&mut self, message: &Value, now: u64) -> Option<String> {
        let params = &message["params"];
        match message["method"].as_str()? {
            "client-status-changed" if params["status"] == "disconnected" => {
                self.tasks.retain(|_, p| p.owner != params["clientId"]);
                None
            }
            "thread-stream-state-changed" if params["hostId"] == "local" => {
                let id = params["conversationId"].as_str()?.to_string();
                if !self.desired.contains(&id) {
                    return None;
                }
                let owner = message["sourceClientId"].as_str().unwrap_or("");
                let change = &params["change"];
                let revision = change["revision"].as_u64();
                let result = (|| {
                    if message["version"] != 11 {
                        return Err(());
                    }
                    let revision = revision.ok_or(())?;
                    let old = self.tasks.get(&id);
                    let status = match change["type"].as_str() {
                        Some("snapshot") => {
                            change["conversationState"]["threadRuntimeStatus"].clone()
                        }
                        Some("patches") => {
                            let old = old.ok_or(())?;
                            if old.owner != owner
                                || change["baseRevision"].as_u64() != Some(old.revision)
                                || revision <= old.revision
                            {
                                return Err(());
                            }
                            let mut status = old.status.clone();
                            for patch in change["patches"].as_array().ok_or(())? {
                                patch_status(&mut status, patch)?;
                            }
                            status
                        }
                        _ => return Err(()),
                    };
                    if !valid_status(&status) {
                        return Err(());
                    }
                    let changed_ms = old
                        .filter(|p| p.status == status)
                        .map_or(now, |p| p.changed_ms);
                    self.tasks.insert(
                        id.clone(),
                        Projection {
                            owner: owner.into(),
                            revision,
                            status,
                            changed_ms,
                        },
                    );
                    Ok(())
                })();
                if result.is_err() {
                    self.tasks.remove(&id);
                    Some(id)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
    #[cfg(any(unix, test))]
    fn disconnected(&mut self) {
        self.connected = false;
        self.tasks.clear();
    }
}

#[cfg(any(unix, test))]
fn follow(client: &str, id: &str, following: bool) -> Value {
    json!({"type":"broadcast","method":"thread-stream-following-changed",
        "sourceClientId":client,"version":1,
        "params":{"conversationId":id,"hostId":"local","following":following}})
}
#[cfg(any(unix, test))]
fn initialize(request: &str, client: &str) -> Value {
    json!({"type":"request","requestId":request,"sourceClientId":client,"version":1,
        "method":"initialize","params":{"clientType":"pipsqueak"}})
}
#[cfg(any(unix, test))]
fn frame(message: &Value) -> Vec<u8> {
    let bytes = serde_json::to_vec(message).unwrap_or_default();
    let mut out = (bytes.len() as u32).to_le_bytes().to_vec();
    out.extend(bytes);
    out
}
#[cfg(any(unix, test))]
fn next_frame(buffer: &mut Vec<u8>) -> Result<Option<Value>, String> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes(buffer[..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err("task snapshot exceeds limit".into());
    }
    if buffer.len() < len + 4 {
        return Ok(None);
    }
    let message = serde_json::from_slice(&buffer[4..len + 4]).map_err(|_| "invalid task frame")?;
    buffer.drain(..len + 4);
    Ok(Some(message))
}

pub fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        #[cfg(unix)]
        std::thread::spawn(|| loop {
            let _ = observe();
            bridge()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .disconnected();
            std::thread::sleep(Duration::from_secs(5));
        });
    });
}

#[cfg(unix)]
fn observe() -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::net::UnixStream;
    let path = crate::codex::home().join("ipc/ipc.sock");
    let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    let parent = std::fs::metadata(path.parent().unwrap()).map_err(|e| e.to_string())?;
    extern "C" {
        fn getuid() -> u32;
    }
    // The existing app-owned, private socket is the only endpoint we use.
    if !meta.file_type().is_socket()
        || meta.uid() != unsafe { getuid() }
        || parent.uid() != meta.uid()
        || parent.mode() & 0o077 != 0
    {
        return Err("Codex socket is not private to this user".into());
    }
    let stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
    observe_stream(stream)
}

#[cfg(unix)]
fn observe_stream(mut stream: std::os::unix::net::UnixStream) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let send =
        |stream: &mut UnixStream, v: Value| stream.write_all(&frame(&v)).map_err(|e| e.to_string());
    let mut client = String::new();
    let mut sequence = 0;
    let mut pending_ping = Some(("pet-0".to_string(), Instant::now()));
    send(&mut stream, initialize("pet-0", "not-initialized"))?;
    let mut ping_at = Instant::now();
    let mut subscribed = HashSet::<String>::new();
    let mut requested = HashMap::<String, Instant>::new();
    let mut buffer = Vec::new();
    let mut bytes = [0; 65536];
    loop {
        if pending_ping
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5))
        {
            return Err("Codex connection did not answer".into());
        }
        if !client.is_empty() {
            if pending_ping.is_none() && ping_at.elapsed() > Duration::from_secs(15) {
                sequence += 1;
                let request = format!("pet-{sequence}");
                send(&mut stream, initialize(&request, &client))?;
                pending_ping = Some((request, Instant::now()));
            }
            let (desired, observed) = {
                let b = bridge().lock().unwrap_or_else(|e| e.into_inner());
                (
                    b.desired.clone(),
                    b.tasks.keys().cloned().collect::<HashSet<_>>(),
                )
            };
            for id in subscribed.difference(&desired) {
                send(&mut stream, follow(&client, id, false))?;
            }
            for id in &desired {
                let retry = !observed.contains(id)
                    && requested
                        .get(id)
                        .map_or(true, |at| at.elapsed() > Duration::from_secs(30));
                if !subscribed.contains(id) || retry {
                    if subscribed.contains(id) {
                        send(&mut stream, follow(&client, id, false))?;
                    }
                    send(&mut stream, follow(&client, id, true))?;
                    requested.insert(id.clone(), Instant::now());
                }
            }
            requested.retain(|id, _| desired.contains(id));
            subscribed = desired;
            bridge()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .tasks
                .retain(|id, _| subscribed.contains(id));
        }
        match stream.read(&mut bytes) {
            Ok(0) => return Err("Codex disconnected".into()),
            Ok(n) => buffer.extend_from_slice(&bytes[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(e) => return Err(e.to_string()),
        }
        while let Some(message) = next_frame(&mut buffer)? {
            if message["type"] == "response"
                && pending_ping
                    .as_ref()
                    .is_some_and(|(id, _)| message["requestId"] == *id)
            {
                if message["resultType"] != "success" {
                    return Err("Codex registration failed".into());
                }
                client = message["result"]["clientId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or("missing client id")?
                    .into();
                pending_ping = None;
                ping_at = Instant::now();
                bridge().lock().unwrap_or_else(|e| e.into_inner()).connected = true;
            } else if message["type"] == "client-discovery-request" {
                send(
                    &mut stream,
                    json!({"type":"client-discovery-response","requestId":message["requestId"],"response":{"canHandle":false}}),
                )?;
            } else if message["type"] == "broadcast" {
                if message["method"] == "ipc-connection-reset" {
                    return Err("Codex reset connection".into());
                }
                if message["method"] == "thread-stream-following-status-requested" {
                    if let Some(id) = message["params"]["conversationId"]
                        .as_str()
                        .filter(|id| subscribed.contains(*id))
                    {
                        if message["params"]["hostId"] == "local" {
                            send(&mut stream, follow(&client, id, true))?;
                        }
                    }
                } else {
                    // Missing/unsupported snapshots retry at most every 30s.
                    bridge()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .receive(&message, state::now_ms());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(status: Value) -> Value {
        json!({"type":"broadcast","method":"thread-stream-state-changed","version":11,
            "sourceClientId":"owner","params":{"hostId":"local","conversationId":"task",
            "change":{"type":"snapshot","revision":1,"conversationState":{"threadRuntimeStatus":status,"turnHistory":["not retained"]}}}})
    }
    fn change(base: u64, revision: u64, patches: Value) -> Value {
        let mut v = snapshot(Value::Null);
        v["params"]["change"] =
            json!({"type":"patches","baseRevision":base,"revision":revision,"patches":patches});
        v
    }
    fn fixture() -> Bridge {
        Bridge {
            desired: HashSet::from(["task".into()]),
            ..Default::default()
        }
    }
    #[test]
    fn approval_resolution_and_disconnect_never_invent_done() {
        let mut b = fixture();
        b.receive(
            &snapshot(json!({"type":"active","activeFlags":["waitingOnApproval"]})),
            1000,
        );
        let mut s = Session::default();
        b.tasks["task"].decorate(&mut s);
        assert_eq!(s.waiting_since, 1000);
        assert!(s.waiting_reason.contains("Approval"));
        b.receive(
            &change(
                1,
                2,
                json!([{"op":"remove","path":["threadRuntimeStatus","activeFlags",0]}]),
            ),
            2000,
        );
        b.tasks["task"].decorate(&mut s);
        assert_eq!(s.waiting_since, 0);
        assert_eq!(s.state, "running");
        b.receive(
            &change(
                2,
                3,
                json!([{"op":"replace","path":["threadRuntimeStatus"],"value":{"type":"idle"}}]),
            ),
            3000,
        );
        b.tasks["task"].decorate(&mut s);
        assert_eq!(s.state, "idle");
        assert!(s.outcome.is_empty());
        b.disconnected();
        assert!(b.tasks.is_empty());
        assert!(!b.connected);
    }
    #[test]
    fn unrelated_patches_preserve_wait_age_but_gaps_drop_status() {
        let mut b = fixture();
        b.receive(
            &snapshot(json!({"type":"active","activeFlags":["waitingOnUserInput"]})),
            1000,
        );
        b.receive(
            &change(
                1,
                2,
                json!([{"op":"replace","path":["turnHistory",0],"value":"private"}]),
            ),
            2000,
        );
        assert_eq!(b.tasks["task"].changed_ms, 1000);
        assert_eq!(b.tasks["task"].status.as_object().unwrap().len(), 2);
        assert_eq!(
            b.receive(&change(1, 3, json!([])), 3000),
            Some("task".into())
        );
        assert!(b.tasks.is_empty());
    }

    #[test]
    fn async_question_survives_live_working_idle_and_blocking_flags() {
        for status in [
            json!({"type":"active", "activeFlags":[]}),
            json!({"type":"active", "activeFlags":["waitingOnApproval"]}),
            json!({"type":"active", "activeFlags":["waitingOnUserInput"]}),
            json!({"type":"idle"}),
        ] {
            let mut b = fixture();
            b.receive(&snapshot(status), 2000);
            let mut s = Session {
                codex_question_pending: true,
                waiting_since: 1000,
                waiting_reason: "Which folder?".into(),
                ..Default::default()
            };
            b.tasks["task"].decorate(&mut s);
            assert_eq!(s.waiting_since, 1000);
            assert_eq!(s.waiting_reason, "Which folder?");
        }
    }
    #[test]
    fn wrong_host_version_and_owner_are_not_authoritative() {
        let mut b = fixture();
        let mut v = snapshot(json!({"type":"active","activeFlags":[]}));
        v["params"]["hostId"] = json!("remote");
        b.receive(&v, 1);
        assert!(b.tasks.is_empty());
        v["params"]["hostId"] = json!("local");
        b.receive(&v, 2);
        assert_eq!(b.tasks.len(), 1);
        v["version"] = json!(12);
        b.receive(&v, 3);
        assert!(b.tasks.is_empty());
        v["version"] = json!(11);
        b.receive(&v, 4);
        b.receive(&json!({"method":"client-status-changed","params":{"status":"disconnected","clientId":"owner"}}),5);
        assert!(b.tasks.is_empty());
    }
    #[test]
    fn partial_consecutive_and_oversized_frames() {
        let v = initialize("id", "not-initialized");
        let encoded = frame(&v);
        let mut buf = encoded[..3].to_vec();
        assert!(next_frame(&mut buf).unwrap().is_none());
        buf.extend_from_slice(&encoded[3..encoded.len() - 1]);
        assert!(next_frame(&mut buf).unwrap().is_none());
        buf.push(*encoded.last().unwrap());
        buf.extend(frame(&follow("client", "task", true)));
        assert_eq!(next_frame(&mut buf).unwrap(), Some(v));
        assert!(next_frame(&mut buf).unwrap().is_some());
        assert!(buf.is_empty());
        assert!(next_frame(&mut ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec()).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn passive_socket_client_observes_and_never_handles_requests() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        let (client, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        *bridge().lock().unwrap() = fixture();
        let worker = std::thread::spawn(move || {
            let result = observe_stream(client);
            bridge().lock().unwrap().disconnected();
            result
        });
        let read = |server: &mut UnixStream| {
            let mut size = [0; 4];
            server.read_exact(&mut size).unwrap();
            let mut bytes = vec![0; u32::from_le_bytes(size) as usize];
            server.read_exact(&mut bytes).unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        };
        let init = read(&mut server);
        assert_eq!(init["method"], "initialize");
        server
            .write_all(&frame(
                &json!({"type":"response","requestId":init["requestId"],
            "resultType":"success","result":{"clientId":"pet-client"}}),
            ))
            .unwrap();
        let subscription = read(&mut server);
        assert_eq!(subscription, follow("pet-client", "task", true));
        let msg = frame(&snapshot(
            json!({"type":"active","activeFlags":["waitingOnApproval"]}),
        ));
        server.write_all(&msg[..3]).unwrap();
        server.write_all(&msg[3..]).unwrap();
        server
            .write_all(&frame(
                &json!({"type":"client-discovery-request","requestId":"other",
            "request":{"method":"dangerous-method"}}),
            ))
            .unwrap();
        let response = read(&mut server);
        assert_eq!(
            response,
            json!({"type":"client-discovery-response","requestId":"other","response":{"canHandle":false}})
        );
        let b = bridge().lock().unwrap();
        assert!(b.connected);
        assert_eq!(
            b.tasks["task"].status["activeFlags"][0],
            "waitingOnApproval"
        );
        drop(b);
        drop(server);
        assert!(worker.join().unwrap().is_err());
        assert!(!summary()["connected"].as_bool().unwrap());
        assert_eq!(summary()["observed_tasks"], 0);
    }
}
