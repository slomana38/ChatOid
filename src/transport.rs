use std::{
    fmt, fs,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener},
    path::Path,
    sync::{Arc, Once},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum_server::{Handle, tls_rustls::RustlsConfig};
use reqwest::{
    StatusCode,
    blocking::{Client, Response},
};
use rustls::{
    ClientConfig, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::{
    host::{Host, digest},
    identity::{Identity, encode, write_new},
    local_file::{open_regular, read_bounded},
    protocol::*,
};

fn provider() -> Arc<CryptoProvider> {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    Arc::new(rustls::crypto::ring::default_provider())
}

fn pin(certificate: &[u8]) -> Result<String> {
    let (_, cert) = X509Certificate::from_der(certificate)
        .map_err(|_| anyhow::anyhow!("Host sertifikası okunamadı"))?;
    ensure!(
        cert.validity().is_valid(),
        "Host sertifikasının süresi geçersiz"
    );
    Ok(digest(cert.public_key().raw))
}

#[derive(Serialize, Deserialize)]
struct Material {
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl Material {
    fn open(path: &Path) -> Result<Self> {
        const MAX_MATERIAL_BYTES: usize = 24_000;

        match open_regular(path, true) {
            Ok(file) => {
                return Ok(serde_json::from_slice(&read_bounded(
                    &file,
                    MAX_MATERIAL_BYTES,
                )?)?);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let generated = rcgen::generate_simple_self_signed(vec!["chatoid.local".to_owned()])?;
        let material = Self {
            cert: generated.cert.der().to_vec(),
            key: generated.signing_key.serialize_der(),
        };
        match write_new(path, &serde_json::to_vec(&material)?) {
            Ok(()) => Ok(material),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::AlreadyExists) =>
            {
                let file = open_regular(path, true)?;
                Ok(serde_json::from_slice(&read_bounded(
                    &file,
                    MAX_MATERIAL_BYTES,
                )?)?)
            }
            Err(error) => Err(error),
        }
    }
}

pub struct RunningHost {
    pub address: HostAddress,
    handle: Handle<SocketAddr>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for RunningHost {
    fn drop(&mut self) {
        self.handle.graceful_shutdown(Some(Duration::from_secs(2)));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn start(directory: &Path, bind: SocketAddr, owner: &Identity) -> Result<RunningHost> {
    ensure!(
        allowed_ip(bind.ip()),
        "Yerel ağ veya VPN üzerindeki belirli bir IP adresini seç"
    );
    provider();
    fs::create_dir_all(directory)?;
    let material = Material::open(&directory.join("host.key"))?;
    let listener = TcpListener::bind(bind).context("Host adresine bağlanılamadı")?;
    listener.set_nonblocking(true)?;
    let address = HostAddress {
        socket: listener.local_addr()?,
        pin: pin(&material.cert)?,
    };
    let host = Arc::new(Host::open(
        &directory.join("host.sqlite3"),
        &owner.card,
        &address.pin,
    )?);
    let handle = Handle::new();
    let server_handle = handle.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let tls = runtime.block_on(RustlsConfig::from_der(vec![material.cert], material.key))?;
    let server = {
        let _runtime = runtime.enter();
        axum_server::from_tcp_rustls(listener, tls)?
    };
    let thread = thread::Builder::new()
        .name("chatoid-host".into())
        .spawn(move || {
            if let Err(error) = runtime.block_on(
                server
                    .handle(server_handle)
                    .serve(host.router().into_make_service()),
            ) {
                eprintln!("ChatOid host: {error:#}");
            }
        })?;
    Ok(RunningHost {
        address,
        handle,
        thread: Some(thread),
    })
}

#[derive(Debug)]
struct Pinned {
    pin: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if pin(end.as_ref()).ok().as_ref() == Some(&self.pin) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            msg,
            cert,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            msg,
            cert,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
pub struct RemoteError {
    pub status: StatusCode,
    pub message: String,
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RemoteError {}

fn parse<T: DeserializeOwned>(response: Response) -> Result<T> {
    let status = response.status();
    let max = 16 * 1024 * 1024;
    let mut body = Vec::new();
    response.take(max + 1).read_to_end(&mut body)?;
    ensure!((body.len() as u64) <= max, "Host yanıtı çok büyük");
    if status.is_success() {
        return Ok(serde_json::from_slice(&body)?);
    }
    let message = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("Host isteği reddetti ({status})"));
    Err(RemoteError { status, message }.into())
}

pub struct Connection {
    http: Client,
    pub address: HostAddress,
    token: Option<(String, String)>,
}

impl Connection {
    pub fn new(address: HostAddress) -> Result<Self> {
        address.check()?;
        let provider = provider();
        let tls = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned {
                pin: address.pin.clone(),
                provider,
            }))
            .with_no_client_auth();
        let http = Client::builder()
            .use_preconfigured_tls(tls)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            http,
            address,
            token: None,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("https://{}/v1/{path}", self.address.socket)
    }

    fn login(&mut self, identity: &Identity) -> Result<()> {
        let nonce: String = parse(
            self.http
                .post(self.url("challenge"))
                .send()
                .context("Host çevrimdışı veya sertifika doğrulanamadı")?,
        )?;
        let proof = identity.sign(
            "login",
            Login {
                host: self.address.pin.clone(),
                nonce,
                actor: identity.id().into(),
            },
        )?;
        let token = parse(
            self.http
                .post(self.url("login"))
                .json(&SignIn {
                    card: identity.card.clone(),
                    proof,
                })
                .send()?,
        )?;
        self.token = Some((identity.id().into(), token));
        Ok(())
    }

    pub fn call<T: DeserializeOwned>(&mut self, identity: &Identity, action: Action) -> Result<T> {
        if self
            .token
            .as_ref()
            .is_none_or(|(actor, _)| actor != identity.id())
        {
            self.login(identity)?;
        }
        let call = |this: &Self| {
            this.http
                .post(this.url("action"))
                .bearer_auth(
                    this.token
                        .as_ref()
                        .map(|(_, token)| token.as_str())
                        .unwrap_or_default(),
                )
                .json(&action)
                .send()
        };
        let mut response = call(self).context("Host bağlantısı kesildi")?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.login(identity)?;
            response = call(self)?;
        }
        parse(response)
    }

    pub fn offer(
        &mut self,
        actor: &Identity,
        target: crate::identity::Card,
        room: Option<(uuid::Uuid, Friendship)>,
    ) -> Result<(Request, Ticket)> {
        let authority = room.as_ref().and_then(|(_, proof)| {
            (proof.terms.data.host == self.address.pin).then(|| self.address.clone())
        });
        self.offer_with_authority(actor, target, room, authority)
    }

    pub fn offer_with_authority(
        &mut self,
        actor: &Identity,
        target: crate::identity::Card,
        room: Option<(uuid::Uuid, Friendship)>,
        authority: Option<HostAddress>,
    ) -> Result<(Request, Ticket)> {
        let code = encode(crate::identity::random::<32>()?);
        let id = uuid::Uuid::new_v4();
        let ticket = Ticket {
            host: self.address.clone(),
            id,
            code,
            grant: None,
        };
        let terms = actor.sign(
            "offer",
            Terms {
                id,
                host: self.address.pin.clone(),
                from: actor.id().into(),
                to: target.data.key.clone(),
                room: room.as_ref().map(|(id, _)| *id),
                expires: now() + INVITE_LIFETIME,
            },
        )?;
        let request = self.call(
            actor,
            Action::Offer {
                terms,
                target,
                friendship: room.map(|(_, proof)| Box::new(proof)),
                code_hash: digest(&ticket.code),
                authority,
            },
        )?;
        Ok((request, ticket))
    }

    pub fn friendship_active(&self, proof: &Friendship) -> Result<bool> {
        parse(
            self.http
                .post(self.url("friendship"))
                .json(proof)
                .send()
                .context("Arkadaşlık hostundan güncel doğrulama bekleniyor")?,
        )
    }

    fn binary_request(
        &mut self,
        identity: &Identity,
        request: impl Fn(&Self) -> reqwest::blocking::RequestBuilder,
    ) -> Result<Response> {
        if self
            .token
            .as_ref()
            .is_none_or(|(actor, _)| actor != identity.id())
        {
            self.login(identity)?;
        }
        let send = |this: &Self| {
            request(this)
                .bearer_auth(&this.token.as_ref().expect("authenticated").1)
                .send()
        };
        let mut response = send(self)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.login(identity)?;
            response = send(self)?;
        }
        Ok(response)
    }

    pub fn upload_chunk(
        &mut self,
        identity: &Identity,
        id: uuid::Uuid,
        offset: usize,
        chunk: &[u8],
    ) -> Result<()> {
        let path = format!("upload/{id}/{offset}");
        parse(self.binary_request(identity, |this| {
            this.http
                .post(this.url(&path))
                .header("content-type", "application/octet-stream")
                .body(chunk.to_vec())
        })?)
    }

    pub fn download_chunk(
        &mut self,
        identity: &Identity,
        seq: i64,
        offset: usize,
    ) -> Result<Vec<u8>> {
        let path = format!("file/{seq}/{offset}");
        let response = self.binary_request(identity, |this| this.http.get(this.url(&path)))?;
        if !response.status().is_success() {
            return parse(response);
        }
        use std::io::Read;
        let mut bytes = Vec::new();
        response
            .take((crate::protocol::FILE_CHUNK + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= crate::protocol::FILE_CHUNK,
            "Geçersiz dosya parçası"
        );
        Ok(bytes)
    }

    pub fn download_attachment(
        &mut self,
        identity: &Identity,
        message: &Message,
        output: &mut impl Write,
        check: impl Fn() -> Result<()>,
    ) -> Result<()> {
        let info = message
            .attachment
            .as_ref()
            .context("Dosya bilgisi bulunamadı")?;
        ensure!(
            (1..=MAX_BINARY_ATTACHMENT).contains(&info.bytes),
            "Geçersiz dosya boyutu"
        );
        check()?;
        if info.binary {
            for offset in (0..info.bytes).step_by(FILE_CHUNK) {
                check()?;
                let chunk = self.download_chunk(identity, message.seq, offset)?;
                ensure!(
                    chunk.len() == (info.bytes - offset).min(FILE_CHUNK),
                    "Dosya eksik"
                );
                output.write_all(&chunk)?;
            }
        } else {
            let file: Attachment = self.call(
                identity,
                Action::Download {
                    room: message.room,
                    seq: message.seq,
                },
            )?;
            let bytes = file.bytes()?;
            ensure!(bytes.len() == info.bytes, "Dosya boyutu değişti");
            output.write_all(&bytes)?;
        }
        check()
    }
}
