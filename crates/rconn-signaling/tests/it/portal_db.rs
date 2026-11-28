//! The database foundation: importing a 0.1.x data dir, the master-key envelope, the
//! computer inventory (org hosts only), old hosts' HostHello, and the CLI around them.

use futures_util::{SinkExt, StreamExt};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::fleet::{FleetCa, HostKey, host_proof_transcript};
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_core::protocol::{CHALLENGE_LEN, ClientMessage, HOST_CAPABILITIES, ServerMessage};
use rconn_signaling::accounts::Accounts;
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::db::{Db, HostRow, Role};
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::hosts::DbInventory;
use rconn_signaling::import::{self, LEGACY_FILES};
use rconn_signaling::public::PublicHosts;
use rconn_signaling::reload::{Seen, reload_once};
use rconn_signaling::secrets::MasterKey;
use rconn_signaling::store::{DataDir, hex};
use rconn_signaling::{AppState, router};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
const SID: &str = "wss://portal-db.test";

async fn send_raw(ws: &mut Ws, text: String) {
    ws.send(Message::Text(text.into())).await.unwrap();
}

async fn send(ws: &mut Ws, m: &ClientMessage) {
    send_raw(ws, m.to_json().unwrap()).await;
}

async fn recv(ws: &mut Ws) -> Option<ServerMessage> {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timeout")
        {
            Some(Ok(Message::Text(t))) => return Some(ServerMessage::from_json(&t).unwrap()),
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => {}
        }
    }
}

/// Finish a host handshake whose hello (`hello_json`, with client nonce `cn`) is sent raw.
async fn host_auth(
    url: &str,
    id: HostId,
    key: &HostKey,
    cert: &str,
    cn: &[u8],
    hello_json: String,
) -> (Ws, Option<ServerMessage>) {
    let (mut ws, _) = connect_async(url).await.unwrap();
    send_raw(&mut ws, hello_json).await;
    let Some(ServerMessage::Challenge { nonce }) = recv(&mut ws).await else {
        panic!("no challenge")
    };
    let sig = key
        .sign(&host_proof_transcript(SID, id, cert, cn, &nonce))
        .unwrap();
    send(&mut ws, &ClientMessage::HostProof { signature: sig }).await;
    let verdict = recv(&mut ws).await;
    (ws, verdict)
}

/// A current host: HostHello with os, version and capabilities.
async fn new_host(url: &str, ca: &FleetCa, label: &str) -> (HostId, Ws) {
    let id = HostId::new();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_host_cert(id, &key).unwrap();
    let cn = vec![9u8; CHALLENGE_LEN];
    let hello = ClientMessage::HostHello {
        protocol_version: PROTOCOL_VERSION,
        host_id: id,
        cert_pem: cert.clone(),
        client_nonce: cn.clone(),
        label: Some(label.into()),
        os: Some("linux ubuntu 24.04".into()),
        version: Some("0.1.3".into()),
        capabilities: HOST_CAPABILITIES.iter().map(|c| (*c).into()).collect(),
    };
    let (ws, verdict) = host_auth(url, id, &key, &cert, &cn, hello.to_json().unwrap()).await;
    assert!(
        matches!(verdict, Some(ServerMessage::Welcome { .. })),
        "{verdict:?}"
    );
    (id, ws)
}

/// Wait until the inventory writer has produced a row matching `ok`.
async fn wait_for_row(db: &Db, org: OrgId, id: HostId, ok: impl Fn(&HostRow) -> bool) -> HostRow {
    for _ in 0..250 {
        if let Some(row) = db.host(org, id).await.unwrap()
            && ok(&row)
        {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("inventory row for {id} never matched");
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn import_moves_files_into_the_database_once_and_keeps_ids() {
    let dir = tempfile::tempdir().unwrap();
    let store = DataDir::new(dir.path());
    let ca = store.init(&[]).unwrap();

    // The files a 0.1.x server left behind.
    let mut accounts = Accounts::default();
    let alice = accounts.add("alice", "correct horse battery").unwrap();
    write(
        &dir.path().join("accounts.json"),
        &serde_json::to_string(&accounts).unwrap(),
    );
    let token = "0123456789abcdef-static-operator-token";
    let op = OperatorId::new();
    let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    write(
        &dir.path().join("operators.json"),
        &serde_json::json!({ hex(&digest): op }).to_string(),
    );
    let (org_host, public_host) = (HostId::new(), HostId::new());
    write(
        &dir.path().join("public-hosts.jsonl"),
        &format!("{{\"host_id\":\"{public_host}\",\"enrolled\":1,\"owner\":null}}\n"),
    );
    write(
        &dir.path().join("revoked.json"),
        &serde_json::to_string(&[org_host, public_host]).unwrap(),
    );

    let key_path = dir.path().join("keys").join("master.key");
    let (db, master, report) = import::open(&store, &key_path).await.unwrap();
    assert_eq!(report.org_created, Some(ca.org_id()));
    assert_eq!(
        (report.users, report.accounts, report.tokens, report.revoked),
        (2, 1, 1, 2)
    );
    assert_eq!(report.renamed.len(), 3);
    for f in LEGACY_FILES {
        assert!(!dir.path().join(f).exists(), "{f} renamed");
        assert!(dir.path().join(format!("{f}.imported")).exists(), "{f}");
    }
    assert!(
        dir.path().join("ca.key.pem").exists() && dir.path().join("ca.pem").exists(),
        "CA files stay for older tools"
    );
    assert!(
        dir.path().join("public-hosts.jsonl").exists(),
        "the public registry stays a file"
    );

    // Ids carried over: org from ca.pem, operator ids become user ids.
    let org = db.require_legacy_org().await.unwrap();
    assert_eq!(org.id, ca.org_id());
    let imported = db.accounts(org.id).await.unwrap();
    assert_eq!(imported.0["alice"].operator_id, alice);
    assert_eq!(
        imported.verify("alice", "correct horse battery"),
        Some(alice)
    );
    assert_eq!(
        db.role(org.id, alice).await.unwrap(),
        Some(Role::Technician)
    );
    assert_eq!(
        db.operator_tokens().await.unwrap().get(&digest),
        Some(&OperatorIdentity {
            operator_id: op,
            org_id: org.id
        })
    );
    assert_eq!(
        db.revoked_hosts().await.unwrap(),
        HashSet::from([org_host, public_host])
    );
    assert!(
        db.host(org.id, org_host)
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_some()
    );
    assert_eq!(
        db.host_count().await.unwrap(),
        1,
        "the computer without an account is not inventory"
    );

    // The CA key is sealed in the database and opens to the file's key.
    let sealed = org.ca_key_sealed.as_deref().unwrap();
    assert!(!String::from_utf8_lossy(sealed).contains("PRIVATE KEY"));
    assert_eq!(
        db.org_ca(org.id, &master).await.unwrap().key_pem(),
        ca.key_pem()
    );

    // Twice is a no-op.
    let version = db.config_version().await.unwrap();
    let (db, _, again) = import::open(&store, &key_path).await.unwrap();
    assert!(again.is_noop(), "{again:?}");
    assert_eq!(db.config_version().await.unwrap(), version);

    // An old script writes a file again: nothing is duplicated, the file moves aside.
    write(
        &dir.path().join("revoked.json"),
        &serde_json::to_string(&[org_host]).unwrap(),
    );
    let (db, _, third) = import::open(&store, &key_path).await.unwrap();
    assert_eq!(third.revoked, 0);
    assert!(dir.path().join("revoked.json.imported.2").exists());
    assert_eq!(db.host_count().await.unwrap(), 1);
}

#[test]
fn envelope_round_trips_and_fails_for_another_org_key_or_tampering() {
    let key = MasterKey::generate();
    let (a, b) = (OrgId::new(), OrgId::new());
    let secret = b"-----BEGIN PRIVATE KEY----- the CA key";
    let sealed = key.seal(a, secret).unwrap();
    assert_eq!(key.open(a, &sealed).unwrap(), secret);
    assert!(!sealed.windows(11).any(|w| w == b"PRIVATE KEY"));
    assert_ne!(
        sealed,
        key.seal(a, secret).unwrap(),
        "fresh nonce each time"
    );
    assert!(
        key.open(b, &sealed).is_err(),
        "org id is the associated data"
    );
    assert!(MasterKey::generate().open(a, &sealed).is_err(), "wrong key");
    let mut tampered = sealed.clone();
    *tampered.last_mut().unwrap() ^= 1;
    assert!(key.open(a, &tampered).is_err());
    assert!(key.open(a, &sealed[..10]).is_err());
}

#[tokio::test]
async fn master_key_is_private_and_a_lost_one_is_only_replaced_when_nothing_is_lost() {
    let dir = tempfile::tempdir().unwrap();
    let store = DataDir::new(dir.path().join("data"));
    store.init(&[]).unwrap();
    let key_path = dir.path().join("master.key");
    let (db, _, _) = import::open(&store, &key_path).await.unwrap();
    let org = db.require_legacy_org().await.unwrap().id;
    let text = std::fs::read_to_string(&key_path).unwrap();
    assert_eq!(text.trim().len(), 64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let db_mode = std::fs::metadata(db.path()).unwrap().permissions().mode();
        assert_eq!(db_mode & 0o077, 0, "database is owner-only");
    }
    assert_eq!(
        rconn_signaling::secrets::default_master_key_path(store.path()),
        std::path::absolute(dir.path().join("master.key")).unwrap(),
        "default: next to the data dir"
    );
    drop(db);

    // Key lost, ca.key.pem still there: sealed again under a new key.
    std::fs::remove_file(&key_path).unwrap();
    let (db, master, _) = import::open(&store, &key_path).await.unwrap();
    assert_ne!(std::fs::read_to_string(&key_path).unwrap(), text);
    db.org_ca(org, &master).await.unwrap();
    drop(db);

    // Another key (a container started without its key volume): sealed again, too.
    std::fs::write(&key_path, format!("{}\n", hex(&[7u8; 32]))).unwrap();
    let (db, master, _) = import::open(&store, &key_path).await.unwrap();
    db.org_ca(org, &master).await.unwrap();
    drop(db);

    // Once ca.key.pem is gone, the sealed key is the only copy: a wrong key never
    // overwrites it, and a missing key stops the server instead of losing the CA.
    std::fs::remove_file(store.path().join("ca.key.pem")).unwrap();
    std::fs::write(&key_path, format!("{}\n", hex(&[8u8; 32]))).unwrap();
    let (db, master, _) = import::open(&store, &key_path).await.unwrap();
    assert!(db.org_ca(org, &master).await.is_err());
    assert!(
        db.org_ca(org, &MasterKey::from_bytes([7u8; 32]))
            .await
            .is_ok(),
        "still sealed under the right key"
    );
    drop(db);
    std::fs::remove_file(&key_path).unwrap();
    let err = import::open(&store, &key_path).await.unwrap_err();
    assert!(format!("{err:#}").contains("master key"), "{err:#}");
    assert!(!key_path.exists(), "no new key that would hide the problem");
}

struct Server {
    url: String,
    db: Db,
    org: OrgId,
    ca: FleetCa,
    public_ca: FleetCa,
    state: AppState,
    auth: Arc<FleetAuthenticator>,
    _dir: tempfile::TempDir,
}

/// A server on a fresh data dir: the org from the database, plus computers without an
/// account (public CA), with the database inventory.
async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = DataDir::new(dir.path().join("data"));
    store.init(&[]).unwrap();
    let (db, master, _) = import::open(&store, &dir.path().join("master.key"))
        .await
        .unwrap();
    let org = db.require_legacy_org().await.unwrap().id;
    let ca = db.org_ca(org, &master).await.unwrap();
    let public_ca = FleetCa::generate(OrgId::new()).unwrap();
    let public = PublicHosts::new(
        FleetCa::from_pem(public_ca.cert_pem(), public_ca.key_pem()).unwrap(),
        None,
        None,
    )
    .unwrap();
    let auth = Arc::new(FleetAuthenticator::new(SID).with_org_ca(public.org_id(), public.ca_pem()));
    let state = AppState::new(auth.clone());
    state.allow_enrollment(org, FleetCa::from_pem(ca.cert_pem(), ca.key_pem()).unwrap());
    state.allow_public_hosts(public);
    state.record_hosts_in(Arc::new(DbInventory::spawn(db.clone())));
    reload_once(&db, &auth, &state, &mut Seen::default())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/ws", listener.local_addr().unwrap());
    let st = state.clone();
    tokio::spawn(async move { axum::serve(listener, router(st)).await.unwrap() });
    Server {
        url,
        db,
        org,
        ca,
        public_ca,
        state,
        auth,
        _dir: dir,
    }
}

#[tokio::test]
async fn inventory_records_org_hosts_and_never_public_ones() {
    let s = start().await;

    // An org host comes online: its row has what it reported.
    let (id, ws) = new_host(&s.url, &s.ca, "  BOX-1\n").await;
    let row = wait_for_row(&s.db, s.org, id, |r| r.last_seen.is_some()).await;
    assert_eq!(row.hostname.as_deref(), Some("BOX-1"));
    assert_eq!(row.os.as_deref(), Some("linux ubuntu 24.04"));
    assert_eq!(row.version.as_deref(), Some("0.1.3"));
    let mut caps: Vec<String> = HOST_CAPABILITIES.iter().map(|c| (*c).into()).collect();
    caps.sort();
    assert_eq!(row.capabilities, caps);
    assert!(row.revoked_at.is_none());

    // A computer without an account comes and goes.
    let (public_id, public_ws) = new_host(&s.url, &s.public_ca, "ANON").await;
    drop(public_ws);

    // The org host goes offline: last_seen moves (reset it first to see the write).
    sqlx::query("UPDATE hosts SET last_seen = 0 WHERE id = ?")
        .bind(id.to_string())
        .execute(s.db.pool())
        .await
        .unwrap();
    drop(ws);
    wait_for_row(&s.db, s.org, id, |r| r.last_seen.is_some_and(|t| t > 0)).await;

    // The writer is in order, so the public host's events were handled before that.
    assert_eq!(s.db.host_count().await.unwrap(), 1);
    let public_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hosts WHERE id = ?")
        .bind(public_id.to_string())
        .fetch_one(s.db.pool())
        .await
        .unwrap();
    assert_eq!(
        public_rows, 0,
        "computers without an account are never inventory"
    );
    let other_org = OrgId::new();
    assert!(
        s.db.host(other_org, id).await.unwrap().is_none(),
        "another org's id answers nothing"
    );
}

#[tokio::test]
async fn revoking_in_the_database_evicts_a_live_host_and_app_enrollment_is_recorded() {
    let s = start().await;
    let (id, mut ws) = new_host(&s.url, &s.ca, "BOX-2").await;
    assert_eq!(s.state.online_hosts().await, 1);

    // Revoked by the CLI (another process, same database), picked up by the reload loop.
    let other = Db::open(s.db.path()).await.unwrap();
    other.revoke_host(id, false).await.unwrap();
    let mut seen = Seen::default();
    assert!(
        reload_once(&s.db, &s.auth, &s.state, &mut seen)
            .await
            .unwrap()
    );
    assert_eq!(s.state.online_hosts().await, 0, "evicted at once");
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Error { .. })
    ));
    let row = s.db.host(s.org, id).await.unwrap().unwrap();
    assert!(row.revoked_at.is_some());

    // An operator turns a computer into a host from the app: recorded with who did it.
    let (op, token) = s.db.add_operator(s.org).await.unwrap();
    reload_once(&s.db, &s.auth, &s.state, &mut seen)
        .await
        .unwrap();
    let (mut ws, _) = connect_async(&s.url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::OperatorHello {
            protocol_version: PROTOCOL_VERSION,
            token,
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Welcome { .. })
    ));
    let key = HostKey::generate().unwrap();
    send(
        &mut ws,
        &ClientMessage::EnrollHost {
            public_key_der: key.public_der(),
        },
    )
    .await;
    let Some(ServerMessage::HostEnrolled { host_id, .. }) = recv(&mut ws).await else {
        panic!("not enrolled")
    };
    let row = wait_for_row(&s.db, s.org, host_id, |r| r.enrolled_at.is_some()).await;
    assert_eq!(row.enrolled_by, Some(op.to_string()));
}

/// A HostHello exactly as 0.1.x hosts send it: no os, version or capabilities.
fn hello_0_1(id: HostId, cert: &str, cn: &[u8]) -> String {
    format!(
        r#"{{"type":"host_hello","protocol_version":{PROTOCOL_VERSION},"host_id":"{id}","cert_pem":{},"client_nonce":{},"label":"OLD-PC"}}"#,
        serde_json::to_string(cert).unwrap(),
        serde_json::to_string(cn).unwrap(),
    )
}

#[tokio::test]
async fn hosts_from_0_1_still_connect_and_are_inventoried() {
    let s = start().await;
    let id = HostId::new();
    let key = HostKey::generate().unwrap();
    let cert = s.ca.issue_host_cert(id, &key).unwrap();
    let cn = vec![3u8; CHALLENGE_LEN];
    let json = hello_0_1(id, &cert, &cn);

    let ClientMessage::HostHello {
        label,
        os,
        version,
        capabilities,
        ..
    } = ClientMessage::from_json(&json).unwrap()
    else {
        panic!("not a HostHello")
    };
    assert_eq!(label.as_deref(), Some("OLD-PC"));
    assert_eq!((os, version, capabilities), (None, None, vec![]));

    let (_ws, verdict) = host_auth(&s.url, id, &key, &cert, &cn, json).await;
    assert!(
        matches!(verdict, Some(ServerMessage::Welcome { .. })),
        "{verdict:?}"
    );
    let row = wait_for_row(&s.db, s.org, id, |r| r.last_seen.is_some()).await;
    assert_eq!(row.hostname.as_deref(), Some("OLD-PC"));
    assert_eq!((row.os, row.version), (None, None));
    assert!(row.capabilities.is_empty());

    // And a current host's hello leaves the 0.1.x fields where they were.
    let new = ClientMessage::HostHello {
        protocol_version: PROTOCOL_VERSION,
        host_id: id,
        cert_pem: cert.clone(),
        client_nonce: cn.clone(),
        label: Some("OLD-PC".into()),
        os: Some("macos 15.3".into()),
        version: Some("0.1.3".into()),
        capabilities: vec!["host_name".into()],
    };
    let v: serde_json::Value = serde_json::from_str(&new.to_json().unwrap()).unwrap();
    let old: serde_json::Value = serde_json::from_str(&hello_0_1(id, &cert, &cn)).unwrap();
    for field in [
        "type",
        "protocol_version",
        "host_id",
        "cert_pem",
        "client_nonce",
        "label",
    ] {
        assert_eq!(v[field], old[field], "{field}");
    }
}

#[tokio::test]
async fn set_owner_links_an_account_and_demotes_the_previous_owner() {
    let s = start().await;
    let alice =
        s.db.add_account(s.org, "alice", "correct horse battery")
            .await
            .unwrap();
    let (owner, demoted) =
        s.db.set_owner(s.org, "Boss@Example.com", Some("alice"))
            .await
            .unwrap();
    assert_eq!((owner, demoted), (alice, vec![]));
    assert_eq!(s.db.role(s.org, alice).await.unwrap(), Some(Role::Owner));
    assert_eq!(
        s.db.set_owner(s.org, "boss@example.com", None)
            .await
            .unwrap()
            .0,
        alice,
        "emails match without regard to case"
    );

    let (bob, demoted) =
        s.db.set_owner(s.org, "bob@example.com", None)
            .await
            .unwrap();
    assert_ne!(bob, alice);
    assert_eq!(demoted, vec![alice]);
    assert_eq!(s.db.role(s.org, alice).await.unwrap(), Some(Role::Admin));
    assert_eq!(s.db.role(s.org, bob).await.unwrap(), Some(Role::Owner));
    assert!(s.db.set_owner(s.org, "not an email", None).await.is_err());
    assert_eq!(
        s.db.role(OrgId::new(), bob).await.unwrap(),
        None,
        "no role in an org they aren't in"
    );
}

fn cli(data: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_rconn-signaling"))
        .args(args)
        .env("RCONN_DATA_DIR", data)
        .env_remove("RCONN_MASTER_KEY_FILE")
        .env_remove("RCONN_PLAIN")
        .env("RUST_LOG", "warn")
        .output()
        .unwrap()
}

fn stdout(o: &std::process::Output) -> String {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[tokio::test]
async fn cli_writes_to_the_database_and_prints_the_pin() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");

    // No TLS identity: behind a proxy, nothing to pin.
    std::fs::create_dir_all(&data).unwrap();
    assert!(stdout(&cli(&data, &["pin"])).contains("no pin needed"));

    let init = stdout(&cli(&data, &["init", "--hostname", "rconn.test"]));
    assert!(dir.path().join("master.key").exists(), "{init}");
    assert!(!cli(&data, &["init"]).status.success(), "init only once");
    let pin = stdout(&cli(&data, &["pin"]));
    let fp = DataDir::new(&data).tls_fingerprint().unwrap().unwrap();
    assert_eq!(pin.trim(), fp);
    assert!(init.contains(&fp), "same pin as init printed");
    assert!(stdout(&cli(&data, &["pin", "--plain"])).contains("no pin needed"));

    let added = stdout(&cli(&data, &["add-operator"]));
    let token = added
        .lines()
        .find_map(|l| l.strip_prefix("token (shown once): "))
        .unwrap()
        .to_owned();
    let out = dir.path().join("host1");
    stdout(&cli(
        &data,
        &["enroll-host", "--out", out.to_str().unwrap()],
    ));
    let host: HostId = HostId(
        std::fs::read_to_string(out.join("host.id"))
            .unwrap()
            .parse()
            .unwrap(),
    );
    assert!(stdout(&cli(&data, &["revoke-host", &host.to_string()])).contains("revoked"));
    assert!(stdout(&cli(&data, &["revoke-host", &host.to_string()])).contains("already"));
    assert!(
        stdout(&cli(
            &data,
            &["org", "set-owner", "--email", "owner@example.com"]
        ))
        .contains("owner@example.com")
    );

    let db = Db::open_in(&data).await.unwrap();
    let org = db.require_legacy_org().await.unwrap().id;
    let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    assert_eq!(
        db.operator_tokens().await.unwrap()[&digest].org_id,
        org,
        "token stored by digest"
    );
    let row = db.host(org, host).await.unwrap().unwrap();
    assert!(row.enrolled_at.is_some() && row.revoked_at.is_some());
    assert!(!data.join("operators.json").exists() && !data.join("revoked.json").exists());
}
