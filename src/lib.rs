use std::{
    borrow::Cow,
    collections::HashSet,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use blocking::unblock;
use futures::{
    future::{ready, Either},
    ready,
    stream::once,
    FutureExt, Stream, StreamExt, TryStreamExt,
};
use google_translate_tts::url;
use mdns::RecordKind;
use pin_project::pin_project;
use rust_cast::{
    channels::{
        media::{Media, StreamType},
        receiver::CastDeviceApp,
    },
    CastDevice,
};
use thiserror::Error;

const DEFAULT_DESTINATION_ID: &str = "receiver-0";
const SERVICE_NAME: &'static str = "_googlecast._tcp.local";

pub struct Target {
    name: String,
    addr: SocketAddr,
}

pub struct Connection {
    device: Arc<CastDevice<'static>>,
}

impl Connection {
    pub async fn say<'a, T: Into<Cow<'a, str>>>(&self, message: T) -> Result<(), Error> {
        let device = self.device.clone();
        let message = message.into().into_owned();
        unblock(move || {
            let app: CastDeviceApp = "CC1AD845".parse().unwrap();
            let app = device.receiver.launch_app(&app)?;
            device.connection.connect(&app.transport_id)?;
            device.media.load(
                &app.transport_id,
                &app.session_id,
                &Media {
                    stream_type: StreamType::Buffered,
                    duration: None,
                    metadata: None,
                    content_type: "audio/mp3".into(),
                    content_id: url(&message, "en"),
                },
            )?;
            Ok(())
        })
        .await
    }
}

impl Target {
    pub async fn connect(self) -> Result<Connection, Error> {
        Ok(Connection {
            device: Arc::new(
                unblock(move || {
                    let device = CastDevice::connect_without_host_verification(
                        self.addr.ip().to_string(),
                        self.addr.port(),
                    )?;
                    device
                        .connection
                        .connect(DEFAULT_DESTINATION_ID.to_string())?;
                    device.heartbeat.ping()?;
                    Ok::<_, Error>(device)
                })
                .await?,
            ),
        })
    }
}

impl Target {
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("mdns error: {0}")]
    Mdns(#[from] mdns::Error),
    #[error("chromecast error: {0}")]
    Cast(#[from] rust_cast::errors::Error),
}

#[pin_project]
struct Unique<T: Stream<Item = Result<Target, Error>>> {
    #[pin]
    stream: T,
    seen: HashSet<SocketAddr>,
}

impl<T: Stream<Item = Result<Target, Error>>> Unique<T> {
    fn new(stream: T) -> Self {
        Unique {
            stream,
            seen: HashSet::new(),
        }
    }
}

impl<T: Stream<Item = Result<Target, Error>>> Stream for Unique<T> {
    type Item = T::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        let mut stream = this.stream;
        let seen = this.seen;
        loop {
            let item = match ready!(stream.as_mut().poll_next(cx)) {
                Some(item) => item,
                None => return Poll::Ready(None),
            }?;
            if seen.insert(item.addr.clone()) {
                return Poll::Ready(Some(Ok(item)));
            }
        }
    }
}

pub fn discover() -> impl Stream<Item = Result<Target, Error>> {
    Unique::new(
        async move {
            match mdns::discover::all(SERVICE_NAME, Duration::from_secs(5)) {
                Ok(stream) => Either::Left(stream.listen().map_err(Error::Mdns)),
                Err(e) => Either::Right(once(ready(Err(e.into())))),
            }
        }
        .into_stream()
        .flatten()
        .try_filter_map(|response| async move {
            response
                .additional
                .iter()
                .filter_map(|record| {
                    if let RecordKind::TXT(data) = &record.kind {
                        data.into_iter()
                            .filter_map(|item| {
                                if let "fn" = item.split('=').next()? {
                                    Some(item)
                                } else {
                                    None
                                }
                            })
                            .map(|item| item.split('=').skip(1).next().map(String::from))
                            .next()
                            .flatten()
                    } else {
                        None
                    }
                })
                .next()
                .map(|name| {
                    Some(Ok(Target {
                        name,
                        addr: SocketAddr::from((
                            response
                                .additional
                                .iter()
                                .filter_map(|item| {
                                    if let RecordKind::A(ip) = item.kind {
                                        Some(ip)
                                    } else {
                                        None
                                    }
                                })
                                .next()?,
                            response
                                .additional
                                .into_iter()
                                .filter_map(|item| {
                                    if let RecordKind::SRV { port, .. } = item.kind {
                                        Some(port)
                                    } else {
                                        None
                                    }
                                })
                                .next()?,
                        )),
                    }))
                })
                .flatten()
                .transpose()
        }),
    )
}
