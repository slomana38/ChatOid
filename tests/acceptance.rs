use anyhow::Result;
use chatoid::{
    agent::{self, FilePermit, Settings},
    identity::{Identity, Role},
    protocol::{Action, Decision, Draft, Grant, Message, Request, Status, Sync, Ticket},
    transport::{Connection, RunningHost, start},
};
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

fn human() -> Result<Identity> {
    Identity::generate(Role::Human, "deniz", "Deniz")
}
fn agent_identity() -> Result<Identity> {
    Identity::generate(Role::Agent, "poyraz", "Poyraz")
}

fn host(owner: &Identity) -> Result<(TempDir, RunningHost, Connection)> {
    let temp = tempfile::tempdir()?;
    let bind = std::env::var("CHATOID_TEST_BIND").unwrap_or_else(|_| "127.0.0.1:0".into());
    let running = start(temp.path(), bind.parse()?, owner)?;
    let connection = Connection::new(running.address.clone())?;
    Ok((temp, running, connection))
}

fn redeem(connection: &mut Connection, actor: &Identity, ticket: &Ticket) -> Result<Request> {
    connection.call(
        actor,
        Action::Redeem {
            id: ticket.id,
            code: ticket.code.clone(),
            grant: ticket.grant.clone(),
        },
    )
}

fn decide(
    connection: &mut Connection,
    actor: &Identity,
    request: &Request,
    accepted: bool,
    grant: Option<Grant>,
) -> Result<Request> {
    connection.call(
        actor,
        Action::Decide {
            decision: Decision::issue(actor, request.terms.data.clone(), accepted, grant)?,
        },
    )
}

fn sync(connection: &mut Connection, actor: &Identity, cursor: i64) -> Result<Sync> {
    connection.call(
        actor,
        Action::Sync {
            after: cursor,
            grants: vec![],
        },
    )
}

fn draft(room: Uuid, text: &str, reply: Option<i64>) -> Draft {
    Draft {
        id: Uuid::new_v4(),
        room,
        text: text.into(),
        reply,
        invitation: None,
    }
}

fn send(connection: &mut Connection, actor: &Identity, draft: Draft) -> Result<Message> {
    connection.call(actor, Action::Send { draft })
}

#[test]
fn local_keys_and_role_separation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let human = human()?;
    let agent = agent_identity()?;
    human.save(
        &temp.path().join("deniz.identity"),
        Some("local-test-passphrase"),
    )?;
    agent.save(&temp.path().join("poyraz.key"), None)?;
    assert_eq!(
        Identity::load(
            &temp.path().join("deniz.identity"),
            Role::Human,
            Some("local-test-passphrase")
        )?
        .id(),
        human.id()
    );
    assert_eq!(
        Identity::load(&temp.path().join("poyraz.key"), Role::Agent, None)?.id(),
        agent.id()
    );
    assert!(
        Identity::load(
            &temp.path().join("deniz.identity"),
            Role::Human,
            Some("wrong password")
        )
        .is_err()
    );
    assert!(
        Identity::load(
            &temp.path().join("poyraz.key"),
            Role::Human,
            Some("local-test-passphrase")
        )
        .is_err()
    );
    assert!(Identity::load(&temp.path().join("deniz.identity"), Role::Agent, None).is_err());
    Ok(())
}

#[test]
fn pinned_https_friendship_agent_operator_and_restart() -> Result<()> {
    let human = human()?;
    let agent = agent_identity()?;
    let operator = Identity::generate(Role::Human, "elya", "Elya")?;
    let (directory, host, mut sender) = host(&human)?;
    let address = host.address.clone();
    let mut recipient = Connection::new(address.clone())?;
    let mut observer = Connection::new(address.clone())?;
    let (_, mut ticket) = sender.offer(&human, agent.card.clone(), None)?;
    assert!(sender.offer(&human, agent.card.clone(), None).is_err());
    assert!(redeem(&mut observer, &operator, &ticket).is_err());
    let request = redeem(&mut recipient, &agent, &ticket)?;
    let grant = Grant::issue(&agent, operator.card.clone(), &request.terms.data)?;
    ticket.grant = Some(grant.clone());
    redeem(&mut observer, &operator, &ticket)?;
    let (one, two) = std::thread::scope(|scope| {
        let a = scope.spawn(|| decide(&mut recipient, &agent, &request, true, None));
        let b = scope.spawn(|| decide(&mut observer, &operator, &request, true, Some(grant)));
        (a.join().unwrap(), b.join().unwrap())
    });
    let (one, two) = (one?, two?);
    assert_eq!(one.status, Status::Accepted);
    assert_eq!(one.decision, two.decision);
    let room = ticket.id;
    let first = draft(room, "Kurulum hazır mı?", None);
    let receipt = send(&mut sender, &human, first.clone())?;
    assert_eq!(send(&mut sender, &human, first.clone())?.seq, receipt.seq);
    let reply = send(
        &mut recipient,
        &agent,
        draft(room, "Hazır.", Some(receipt.seq)),
    )?;
    assert_eq!(reply.reply.as_ref().unwrap().id, receipt.seq);
    assert_eq!(sync(&mut recipient, &agent, 0)?.messages.len(), 2);
    assert!(sync(&mut observer, &operator, 0)?.messages.is_empty());
    assert!(
        observer
            .call::<Vec<Message>>(
                &operator,
                Action::History {
                    room,
                    before: i64::MAX
                }
            )
            .is_err()
    );
    let mut wrong = address.clone();
    wrong.pin = chatoid::identity::encode([9; 32]);
    assert!(sync(&mut Connection::new(wrong)?, &human, 0).is_err());
    drop(host);
    std::thread::sleep(std::time::Duration::from_millis(350));
    let _restarted = start(directory.path(), address.socket, &human)?;
    assert_eq!(send(&mut sender, &human, first)?.seq, receipt.seq);
    assert_eq!(sync(&mut sender, &human, 0)?.messages.len(), 2);
    Ok(())
}

#[test]
fn cross_host_group_requires_separate_acceptance_and_enforces_history() -> Result<()> {
    let human = human()?;
    let agent = agent_identity()?;
    let (_first_dir, first, mut agent_host) = host(&agent)?;
    let mut human_guest = Connection::new(first.address.clone())?;
    let (_, ticket) = agent_host.offer(&agent, human.card.clone(), None)?;
    let friend = redeem(&mut human_guest, &human, &ticket)?;
    let accepted = decide(&mut human_guest, &human, &friend, true, None)?;
    let friendship = accepted.friendship().unwrap();
    drop(first);

    let (_second_dir, second, mut human_host) = host(&human)?;
    let mut agent_guest = Connection::new(second.address.clone())?;
    let room = Uuid::new_v4();
    assert_eq!(
        human_host.call::<Uuid>(
            &human,
            Action::Group {
                id: room,
                name: "Proje ekibi".into()
            }
        )?,
        room
    );
    assert!(
        agent_guest
            .call::<Uuid>(
                &agent,
                Action::Group {
                    id: Uuid::new_v4(),
                    name: "Yetkisiz".into()
                }
            )
            .is_err()
    );
    let before = send(&mut human_host, &human, draft(room, "Üyelik öncesi", None))?;
    let (_, invite) =
        human_host.offer(&human, agent.card.clone(), Some((room, friendship.clone())))?;
    let pending = redeem(&mut agent_guest, &agent, &invite)?;
    assert!(sync(&mut agent_guest, &agent, 0)?.rooms.is_empty());
    assert!(
        send(
            &mut agent_guest,
            &agent,
            draft(room, "Onaysız mesaj", Some(before.seq))
        )
        .is_err()
    );
    assert_eq!(
        decide(&mut agent_guest, &agent, &pending, false, None)?.status,
        Status::Rejected
    );
    assert!(sync(&mut agent_guest, &agent, 0)?.rooms.is_empty());
    let (_, invite) = human_host.offer(&human, agent.card.clone(), Some((room, friendship)))?;
    let pending = redeem(&mut agent_guest, &agent, &invite)?;
    decide(&mut agent_guest, &agent, &pending, true, None)?;
    let after = send(
        &mut human_host,
        &human,
        draft(
            room,
            "Poyraz, kurulumu kontrol eder misin?",
            Some(before.seq),
        ),
    )?;
    let state = sync(&mut agent_guest, &agent, 0)?;
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].seq, after.seq);
    assert!(state.messages[0].reply.is_none());
    assert!(
        send(
            &mut agent_guest,
            &agent,
            draft(room, "Yasak alıntı", Some(before.seq))
        )
        .is_err()
    );
    let reply = send(
        &mut agent_guest,
        &agent,
        draft(room, "Kurulum tamam.", Some(after.seq)),
    )?;
    assert_eq!(reply.root, Some(after.seq));
    let _: Value = human_host.call(
        &human,
        Action::Remove {
            room,
            member: agent.id().into(),
        },
    )?;
    send(&mut human_host, &human, draft(room, "Üyelik sonrası", None))?;
    let state = sync(&mut agent_guest, &agent, 0)?;
    assert!(!state.rooms[0].active);
    assert!(state.rooms[0].members.is_empty());
    assert_eq!(state.messages.len(), 2);
    assert!(
        send(
            &mut agent_guest,
            &agent,
            draft(room, "Yetkisiz mesaj", Some(after.seq))
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn group_agent_answers_have_a_shared_finite_chain() -> Result<()> {
    let human = human()?;
    let (_directory, running, mut server) = host(&human)?;
    let room = Uuid::new_v4();
    let _: Uuid = server.call(
        &human,
        Action::Group {
            id: room,
            name: "Ajanlar".into(),
        },
    )?;
    let mut agents = Vec::new();
    for handle in ["poyraz", "kivilcim", "denizci", "yildiz"] {
        let member = Identity::generate(Role::Agent, handle, handle)?;
        let mut client = Connection::new(running.address.clone())?;
        let (_, invite) = server.offer(&human, member.card.clone(), None)?;
        let friend = redeem(&mut client, &member, &invite)?;
        let proof = decide(&mut client, &member, &friend, true, None)?
            .friendship()
            .unwrap();
        let (_, invite) = server.offer(&human, member.card.clone(), Some((room, proof)))?;
        let pending = redeem(&mut client, &member, &invite)?;
        decide(&mut client, &member, &pending, true, None)?;
        agents.push((member, client));
    }
    let root = send(
        &mut server,
        &human,
        draft(room, "@poyraz başlayabilir misin?", None),
    )?;
    let state = sync(&mut server, &human, 0)?;
    let group = state.rooms.iter().find(|r| r.id == room).unwrap();
    assert_eq!(
        agent::addressed(group, &root, &agents[0].0.card.data),
        Some(true)
    );
    assert_eq!(agent::addressed(group, &root, &agents[1].0.card.data), None);
    let mut parent = root.seq;
    for (index, next) in [
        "@kivilcim devam et",
        "@denizci devam et",
        "@yildiz devam et",
    ]
    .iter()
    .enumerate()
    {
        let (actor, client) = &mut agents[index];
        let reply = send(client, actor, draft(room, next, Some(parent)))?;
        assert_eq!(reply.root, Some(root.seq));
        parent = reply.seq;
    }
    let (actor, client) = &mut agents[3];
    assert!(send(client, actor, draft(room, "Dördüncü yanıt", Some(parent))).is_err());
    let state = sync(&mut server, &human, root.seq)?;
    assert_eq!(
        state
            .messages
            .iter()
            .filter(|m| m.root == Some(root.seq))
            .count(),
        3
    );
    Ok(())
}

#[test]
#[ignore = "requires an authenticated local Codex CLI and provider access"]
fn real_codex_natural_address_and_workspace_read() -> Result<()> {
    let human = human()?;
    let agent = agent_identity()?;
    let (_directory, running, mut client) = host(&human)?;
    let mut recipient = Connection::new(running.address.clone())?;
    let (_, ticket) = client.offer(&human, agent.card.clone(), None)?;
    let request = redeem(&mut recipient, &agent, &ticket)?;
    let friend = decide(&mut recipient, &agent, &request, true, None)?
        .friendship()
        .unwrap();
    let room = Uuid::new_v4();
    let _: Uuid = client.call(
        &human,
        Action::Group {
            id: room,
            name: "Kurulum ekibi".into(),
        },
    )?;
    let (_, invite) = client.offer(&human, agent.card.clone(), Some((room, friend)))?;
    let request = redeem(&mut recipient, &agent, &invite)?;
    decide(&mut recipient, &agent, &request, true, None)?;
    let incoming = send(
        &mut client,
        &human,
        draft(
            room,
            "Poyraz, lütfen README.md dosyasını incele ve oradaki kurulum kodunu yaz.",
            None,
        ),
    )?;
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("README.md"),
        "Windows kurulum kodu: TURKUAZ-6183",
    )?;
    let settings = Settings {
        workspace: directory.path().to_string_lossy().into_owned(),
        permits: vec![FilePermit {
            actor: human.id().into(),
            host: running.address.pin.clone(),
            room,
        }],
        ..Settings::default()
    };
    let state = sync(&mut recipient, &agent, 0)?;
    let group = state.rooms.iter().find(|r| r.id == room).unwrap();
    agent::check(&settings)?;
    let answer = agent::response(
        &settings,
        &running.address.pin,
        &agent.card.data,
        group,
        &state.messages,
        &incoming,
    )?
    .expect("the agent should answer the direct request");
    assert!(
        answer.contains("TURKUAZ-6183"),
        "The answer must include the value only visible in the permitted file"
    );
    let reply = send(
        &mut recipient,
        &agent,
        draft(room, &answer, Some(incoming.seq)),
    )?;
    assert_eq!(reply.reply.as_ref().unwrap().id, incoming.seq);
    Ok(())
}
