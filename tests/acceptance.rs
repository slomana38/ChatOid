use anyhow::Result;
use chatoid::{
    agent::{self, FilePermit, Settings},
    identity::{Identity, Role, encode},
    protocol::{
        AVATAR_PX, Action, Attachment, AvatarPhoto, AvatarUpdate, Decision, Draft, Grant, Message,
        Request, Status, Sync, Ticket,
    },
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
            read: Default::default(),
            avatar: None,
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
        attachment: None,
    }
}

fn send(connection: &mut Connection, actor: &Identity, draft: Draft) -> Result<Message> {
    connection.call(actor, Action::Send { draft })
}

#[test]
fn full_size_binary_file_is_atomic_private_and_idempotent() -> Result<()> {
    use chatoid::{
        host::digest,
        protocol::{FILE_CHUNK, MAX_BINARY_ATTACHMENT},
    };
    let sender = human()?;
    let receiver = Identity::generate(Role::Human, "alici", "Alıcı")?;
    let outsider = Identity::generate(Role::Human, "yabanci", "Yabancı")?;
    let (_dir, runtime, mut author) = host(&sender)?;
    let mut guest = Connection::new(runtime.address.clone())?;
    let mut stranger = Connection::new(runtime.address.clone())?;
    let (_, ticket) = author.offer(&sender, receiver.card.clone(), None)?;
    let request = redeem(&mut guest, &receiver, &ticket)?;
    decide(&mut guest, &receiver, &request, true, None)?;
    let room = request.terms.data.id;
    let draft = draft(room, "", None);
    let chunk = vec![27; FILE_CHUNK];
    let bytes = MAX_BINARY_ATTACHMENT;
    let hash = digest(vec![27; bytes]);
    let commit = || Action::SendBinary {
        draft: draft.clone(),
        name: "dosya.bin".into(),
        bytes,
        hash: hash.clone(),
    };
    author.upload_chunk(&sender, draft.id, 0, &chunk)?;
    assert!(author.call::<Message>(&sender, commit()).is_err());
    assert!(sync(&mut guest, &receiver, 0)?.messages.is_empty());
    for offset in (0..bytes).step_by(FILE_CHUNK) {
        author.upload_chunk(&sender, draft.id, offset, &chunk)?;
    }
    let message: Message = author.call(&sender, commit())?;
    assert!(message.attachment.as_ref().unwrap().binary);
    assert_eq!(author.call::<Message>(&sender, commit())?.seq, message.seq);
    assert!(stranger.download_chunk(&outsider, message.seq, 0).is_err());
    for offset in (0..bytes).step_by(FILE_CHUNK) {
        assert_eq!(guest.download_chunk(&receiver, message.seq, offset)?, chunk);
    }
    Ok(())
}

#[test]
fn single_contact_link_delivers_and_ending_friendship_invalidates_old_proof() -> Result<()> {
    use chatoid::protocol::{ContactRoute, INVITE_LIFETIME, now};
    let sender = human()?;
    let recipient = Identity::generate(Role::Human, "duru", "Duru")?;
    let (_authority_dir, authority, mut source) = host(&sender)?;
    let (_target_dir, target, mut target_owner) = host(&recipient)?;
    let route = recipient.sign(
        "contact-route",
        ContactRoute {
            card: recipient.card.clone(),
            host: target.address.clone(),
            expires: now() + INVITE_LIFETIME,
        },
    )?;
    let (_, ticket) = source.offer(&sender, recipient.card.clone(), None)?;
    let mut relay = Connection::new(target.address.clone())?;
    for _ in 0..2 {
        relay.call::<Value>(
            &sender,
            Action::DeliverIntroduction {
                route: Box::new(route.clone()),
                ticket: ticket.clone(),
            },
        )?;
    }
    let inbox = sync(&mut target_owner, &recipient, 0)?.introductions;
    assert_eq!(inbox.len(), 1);
    let mut guest = Connection::new(authority.address.clone())?;
    let requested = redeem(&mut guest, &recipient, &inbox[0])?;
    let proof = decide(&mut guest, &recipient, &requested, true, None)?
        .friendship()
        .unwrap();
    let dm = requested.terms.data.id;
    send(&mut source, &sender, draft(dm, "Geçmiş kalır", None))?;
    let group = Uuid::new_v4();
    target_owner.call::<Uuid>(
        &recipient,
        Action::Group {
            id: group,
            name: "Ekip".into(),
        },
    )?;
    let (_, invite) = target_owner.offer_with_authority(
        &recipient,
        sender.card.clone(),
        Some((group, proof.clone())),
        Some(authority.address.clone()),
    )?;
    let mut group_guest = Connection::new(target.address.clone())?;
    let group_pending = redeem(&mut group_guest, &sender, &invite)?;
    guest.call::<Value>(&recipient, Action::EndFriendship { room: dm })?;
    assert!(
        sync(&mut source, &sender, 0)?
            .rooms
            .iter()
            .any(|room| room.id == dm && room.closed)
    );
    assert_eq!(sync(&mut guest, &recipient, 0)?.messages.len(), 1);
    assert!(send(&mut source, &sender, draft(dm, "Reddedilir", None)).is_err());
    assert!(decide(&mut group_guest, &sender, &group_pending, true, None).is_err());
    assert!(!source.friendship_active(&proof)?);
    let (_, again) = source.offer(&sender, recipient.card.clone(), None)?;
    assert_ne!(again.id, dm);
    Ok(())
}

#[test]
fn agent_device_summary_is_returned_only_to_active_friends() -> Result<()> {
    use chatoid::protocol::{DeviceSummary, ProfilePublication, PublicProfile};
    let human = human()?;
    let agent = agent_identity()?;
    let outsider = Identity::generate(Role::Human, "konuk", "Konuk")?;
    let (_dir, running, mut owner) = host(&human)?;
    let mut guest = Connection::new(running.address.clone())?;
    let mut unrelated = Connection::new(running.address.clone())?;
    let (profile, device) = (
        agent.sign(
            "shared-profile",
            PublicProfile {
                display_name: "Poyraz".into(),
                agent: None,
            },
        )?,
        agent.sign(
            "device-summary",
            DeviceSummary {
                system: "Windows 11".into(),
                architecture: "x86_64".into(),
                processor: "8 mantıksal işlemci".into(),
                memory: "16 GiB".into(),
            },
        )?,
    );
    guest.call::<Value>(
        &agent,
        Action::PublishProfile {
            publication: Box::new(ProfilePublication {
                profile,
                device: Some(device),
            }),
        },
    )?;
    let load = || Action::LoadProfile {
        actor: agent.id().into(),
    };
    assert!(
        unrelated
            .call::<Option<ProfilePublication>>(&outsider, load())
            .is_err()
    );
    let (_, invite) = owner.offer(&human, agent.card.clone(), None)?;
    let request = redeem(&mut guest, &agent, &invite)?;
    let proof = decide(&mut guest, &agent, &request, true, None)?
        .friendship()
        .unwrap();
    let info = owner
        .call::<Option<ProfilePublication>>(&human, load())?
        .unwrap();
    assert_eq!(info.device.unwrap().data.memory, "16 GiB");
    let group = Uuid::new_v4();
    owner.call::<Uuid>(
        &human,
        Action::Group {
            id: group,
            name: "Ortak grup".into(),
        },
    )?;
    let (_, ticket) = owner.offer(&human, agent.card.clone(), Some((group, proof)))?;
    let pending = redeem(&mut guest, &agent, &ticket)?;
    decide(&mut guest, &agent, &pending, true, None)?;
    owner.call::<Value>(
        &human,
        Action::EndFriendship {
            room: request.terms.data.id,
        },
    )?;
    let info = owner
        .call::<Option<ProfilePublication>>(&human, load())?
        .unwrap();
    assert!(info.device.is_none());
    Ok(())
}

#[test]
fn files_photos_and_receipts_are_visible_only_in_shared_chats() -> Result<()> {
    let sender = human()?;
    let receiver = Identity::generate(Role::Human, "yagmur", "Yağmur")?;
    let outsider = Identity::generate(Role::Human, "misafir", "Misafir")?;
    let (_directory, running, mut author) = host(&sender)?;
    let mut guest = Connection::new(running.address.clone())?;
    let mut stranger = Connection::new(running.address.clone())?;
    let (_, invite) = author.offer(&sender, receiver.card.clone(), None)?;
    let request = redeem(&mut guest, &receiver, &invite)?;
    decide(&mut guest, &receiver, &request, true, None)?;
    let room = request.terms.data.id;

    let image = image::RgbImage::from_pixel(AVATAR_PX, AVATAR_PX, image::Rgb([55, 142, 140]));
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg).encode_image(&image)?;
    let data = encode(&jpeg);
    let digest = AvatarPhoto::digest(&data);
    author.call::<Sync>(
        &sender,
        Action::Sync {
            after: 0,
            grants: vec![],
            read: Default::default(),
            avatar: Some(AvatarUpdate {
                data: Some(data.clone()),
            }),
        },
    )?;
    assert!(sync(&mut stranger, &outsider, 0)?.avatar_index.is_empty());
    assert_eq!(
        sync(&mut guest, &receiver, 0)?.avatar_index[sender.id()],
        digest
    );
    let request_photo = || Action::Avatar {
        actor: sender.id().into(),
        digest: digest.clone(),
    };
    assert!(
        stranger
            .call::<AvatarPhoto>(&outsider, request_photo())
            .is_err()
    );
    assert_eq!(
        guest.call::<AvatarPhoto>(&receiver, request_photo())?.data,
        data
    );

    let mut outgoing = draft(room, "", None);
    outgoing.attachment = Some(Attachment {
        name: "örnek.jpg".into(),
        data,
    });
    let message = send(&mut author, &sender, outgoing.clone())?;
    assert_eq!(
        send(&mut author, &sender, outgoing.clone())?.seq,
        message.seq
    );
    outgoing.attachment.as_mut().unwrap().name = "başka.jpg".into();
    assert!(send(&mut author, &sender, outgoing).is_err());
    let received = sync(&mut guest, &receiver, 0)?;
    let file = received.messages[0].attachment.as_ref().unwrap();
    assert_eq!(file.name, "örnek.jpg");
    assert_eq!(file.bytes, jpeg.len());
    assert_eq!(file.preview.as_ref().unwrap().decode()?.width(), AVATAR_PX);
    let fetch = || Action::Download {
        room,
        seq: message.seq,
    };
    assert!(stranger.call::<Attachment>(&outsider, fetch()).is_err());
    assert!(
        guest
            .call::<Attachment>(
                &receiver,
                Action::Download {
                    room: Uuid::new_v4(),
                    seq: message.seq,
                }
            )
            .is_err()
    );
    assert_eq!(guest.call::<Attachment>(&receiver, fetch())?.bytes()?, jpeg);

    assert_eq!(sync(&mut author, &sender, 0)?.receipts[0].delivered, 0);
    sync(&mut guest, &receiver, received.cursor)?;
    let delivered = sync(&mut author, &sender, 0)?;
    assert_eq!(delivered.receipts[0].delivered, message.seq);
    assert_eq!(delivered.receipts[0].read, 0);
    guest.call::<Sync>(
        &receiver,
        Action::Sync {
            after: received.cursor,
            grants: vec![],
            read: [(room, message.seq)].into(),
            avatar: None,
        },
    )?;
    assert_eq!(sync(&mut author, &sender, 0)?.receipts[0].read, message.seq);
    assert!(sync(&mut stranger, &outsider, 0)?.receipts.is_empty());
    Ok(())
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
    let authority = first.address.clone();

    let (second_dir, second, mut human_host) = host(&human)?;
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
    let (_, invite) = human_host.offer_with_authority(
        &human,
        agent.card.clone(),
        Some((room, friendship.clone())),
        Some(authority.clone()),
    )?;
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
    let (_, invite) = human_host.offer_with_authority(
        &human,
        agent.card.clone(),
        Some((room, friendship)),
        Some(authority),
    )?;
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
    assert_eq!(state.rooms[0].owner, human.id());
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
    let address = second.address.clone();
    drop(second);
    let restarted = start(second_dir.path(), address.socket, &human)?;
    let mut host_client = Connection::new(restarted.address.clone())?;
    let persisted = sync(&mut host_client, &human, 0)?;
    assert_eq!(persisted.rooms[0].id, room);
    assert_eq!(persisted.rooms[0].owner, human.id());
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
