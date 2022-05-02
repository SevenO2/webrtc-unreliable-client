
use crate::webrtc::dtls_transport::RTCDtlsTransport;
use crate::webrtc::error::{flatten_errs, Error, Result};
use crate::webrtc::peer_connection::sdp::TrackDetails;
use crate::webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters,
    RTPCodecType,
};
use crate::webrtc::rtp_transceiver::{
    create_stream_info, RTCRtpDecodingParameters, RTCRtpReceiveParameters, SSRC,
};
use crate::webrtc::track::track_remote::TrackRemote;
use crate::webrtc::track::{TrackStream, TrackStreams};

use interceptor::{Attributes, Interceptor};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify, RwLock};

pub struct RTPReceiverInternal {
    // removing these seems to cause a compiler panic
    #[allow(dead_code)]
    kind: bool,
    #[allow(dead_code)]
    transport: bool,
    #[allow(dead_code)]
    media_engine: bool,

    tracks: RwLock<Vec<TrackStreams>>,
    closed_rx: Arc<Notify>,
    received_rx: Mutex<mpsc::Receiver<()>>,
    transceiver_codecs: Mutex<Option<Arc<Mutex<Vec<RTCRtpCodecParameters>>>>>,
    interceptor: Arc<dyn Interceptor + Send + Sync>,
}

impl RTPReceiverInternal {
    /// read reads incoming RTCP for this RTPReceiver
    async fn read(&self, b: &mut [u8]) -> Result<(usize, Attributes)> {
        {
            let mut received_rx = self.received_rx.lock().await;
            tokio::select! {
                _ = received_rx.recv() =>{
                    // Drop into the below code and don't hold this lock any longer
                }
                _ = self.closed_rx.notified() => {
                    return Err(Error::ErrClosedPipe);
                }
            }
        }

        let tracks = self.tracks.read().await;
        if let Some(t) = tracks.first() {
            if let Some(rtcp_interceptor) = &t.stream.rtcp_interceptor {
                let a = Attributes::new();
                tokio::select! {
                    _ = self.closed_rx.notified() => {
                        Err(Error::ErrClosedPipe)
                    }
                    result = rtcp_interceptor.read(b, &a) => {
                        Ok(result?)
                    }
                }
            } else {
                Err(Error::ErrInterceptorNotBind)
            }
        } else {
            Err(Error::ErrExistingTrack)
        }
    }

    /// read_simulcast reads incoming RTCP for this RTPReceiver for given rid
    async fn read_simulcast(&self, b: &mut [u8], rid: &str) -> Result<(usize, Attributes)> {
        {
            let mut received_rx = self.received_rx.lock().await;
            tokio::select! {
                _ = received_rx.recv() =>{
                    // Drop into the below code and don't hold this lock any longer
                }
                _ = self.closed_rx.notified() => {
                    return Err(Error::ErrClosedPipe);
                }
            }
        }

        let tracks = self.tracks.read().await;
        for t in &*tracks {
            if t.track.rid() == rid {
                if let Some(rtcp_interceptor) = &t.stream.rtcp_interceptor {
                    let a = Attributes::new();
                    tokio::select! {
                        _ = self.closed_rx.notified() => {
                            return Err(Error::ErrClosedPipe);
                        }
                        result = rtcp_interceptor.read(b, &a) => {
                            return Ok(result?);
                        }
                    }
                } else {
                    return Err(Error::ErrInterceptorNotBind);
                }
            }
        }
        Err(Error::ErrRTPReceiverForRIDTrackStreamNotFound)
    }

    /// read_rtcp is a convenience method that wraps Read and unmarshal for you.
    /// It also runs any configured interceptors.
    async fn read_rtcp(
        &self,
        receive_mtu: usize,
    ) -> Result<(Vec<Box<dyn rtcp::packet::Packet + Send + Sync>>, Attributes)> {
        let mut b = vec![0u8; receive_mtu];
        let (n, attributes) = self.read(&mut b).await?;

        let mut buf = &b[..n];
        let pkts = rtcp::packet::unmarshal(&mut buf)?;

        Ok((pkts, attributes))
    }

    /// read_simulcast_rtcp is a convenience method that wraps ReadSimulcast and unmarshal for you
    async fn read_simulcast_rtcp(
        &self,
        rid: &str,
        receive_mtu: usize,
    ) -> Result<(Vec<Box<dyn rtcp::packet::Packet + Send + Sync>>, Attributes)> {
        let mut b = vec![0u8; receive_mtu];
        let (n, attributes) = self.read_simulcast(&mut b, rid).await?;

        let mut buf = &b[..n];
        let pkts = rtcp::packet::unmarshal(&mut buf)?;

        Ok((pkts, attributes))
    }
}

/// RTPReceiver allows an application to inspect the receipt of a TrackRemote
pub struct RTCRtpReceiver {
    receive_mtu: usize,
    kind: RTPCodecType,
    transport: Arc<RTCDtlsTransport>,
    closed_tx: Arc<Notify>,
    received_tx: Mutex<Option<mpsc::Sender<()>>>,

    pub internal: Arc<RTPReceiverInternal>,
}

impl std::fmt::Debug for RTCRtpReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RTCRtpReceiver")
            .field("kind", &self.kind)
            .finish()
    }
}

impl RTCRtpReceiver {
    pub fn new(
        receive_mtu: usize,
        kind: RTPCodecType,
        transport: Arc<RTCDtlsTransport>,
        interceptor: Arc<dyn Interceptor + Send + Sync>,
    ) -> Self {
        let closed_tx = Arc::new(Notify::new());
        let closed_rx = closed_tx.clone();
        let (received_tx, received_rx) = mpsc::channel(1);

        RTCRtpReceiver {
            receive_mtu,
            kind,
            transport: Arc::clone(&transport),
            closed_tx,
            received_tx: Mutex::new(Some(received_tx)),

            internal: Arc::new(RTPReceiverInternal {
                kind: true,
                transport: true,
                media_engine: true,

                tracks: RwLock::new(vec![]),
                interceptor,
                closed_rx,
                received_rx: Mutex::new(received_rx),
                transceiver_codecs: Mutex::new(None),
            }),
        }
    }

    pub fn kind(&self) -> RTPCodecType {
        self.kind
    }

    pub(crate) async fn set_transceiver_codecs(
        &self,
        codecs: Option<Arc<Mutex<Vec<RTCRtpCodecParameters>>>>,
    ) {
        let mut transceiver_codecs = self.internal.transceiver_codecs.lock().await;
        *transceiver_codecs = codecs;
    }

    /// track returns the RtpTransceiver TrackRemote
    pub async fn track(&self) -> Option<Arc<TrackRemote>> {
        let tracks = self.internal.tracks.read().await;
        if tracks.len() != 1 {
            None
        } else {
            tracks.first().map(|t| Arc::clone(&t.track))
        }
    }

    /// tracks returns the RtpTransceiver traclockks
    /// A RTPReceiver to support Simulcast may now have multiple tracks
    pub async fn tracks(&self) -> Vec<Arc<TrackRemote>> {
        let tracks = self.internal.tracks.read().await;
        tracks.iter().map(|t| Arc::clone(&t.track)).collect()
    }

    /// receive initialize the track and starts all the transports
    pub async fn receive(&self, parameters: &RTCRtpReceiveParameters) -> Result<()> {
        let receiver = Arc::downgrade(&self.internal);

        let _d = {
            let mut received_tx = self.received_tx.lock().await;
            if received_tx.is_none() {
                return Err(Error::ErrRTPReceiverReceiveAlreadyCalled);
            }
            received_tx.take()
        };

        let interceptor= Arc::clone(&self.internal.interceptor);

        let codec = RTCRtpCodecCapability::default();

        for encoding in &parameters.encodings {
            let (stream_info, rtp_read_stream, rtp_interceptor, rtcp_read_stream, rtcp_interceptor) =
                if encoding.ssrc != 0 {
                    let stream_info = create_stream_info(
                        "".to_owned(),
                        encoding.ssrc,
                        0,
                        codec.clone(),
                        &vec![],
                    );
                    let (rtp_read_stream, rtp_interceptor, rtcp_read_stream, rtcp_interceptor) =
                        self.transport
                            .streams_for_ssrc(encoding.ssrc, &stream_info, &interceptor)
                            .await?;

                    (
                        Some(stream_info),
                        rtp_read_stream,
                        rtp_interceptor,
                        rtcp_read_stream,
                        rtcp_interceptor,
                    )
                } else {
                    (None, None, None, None, None)
                };

            let t = TrackStreams {
                track: Arc::new(TrackRemote::new(
                    self.receive_mtu,
                    self.kind,
                    encoding.ssrc,
                    encoding.rid.clone(),
                    receiver.clone(),
                )),
                stream: TrackStream {
                    stream_info,
                    rtp_read_stream,
                    rtp_interceptor,
                    rtcp_read_stream,
                    rtcp_interceptor,
                },

                repair_stream: TrackStream {
                    stream_info: None,
                    rtp_read_stream: None,
                    rtp_interceptor: None,
                    rtcp_read_stream: None,
                    rtcp_interceptor: None,
                },
            };

            {
                let mut tracks = self.internal.tracks.write().await;
                tracks.push(t);
            };

            let rtx_ssrc = encoding.rtx.ssrc;
            if rtx_ssrc != 0 {
                let stream_info = create_stream_info(
                    "".to_owned(),
                    rtx_ssrc,
                    0,
                    codec.clone(),
                    &vec![],
                );
                let (rtp_read_stream, rtp_interceptor, rtcp_read_stream, rtcp_interceptor) = self
                    .transport
                    .streams_for_ssrc(rtx_ssrc, &stream_info, &interceptor)
                    .await?;

                self.receive_for_rtx(
                    rtx_ssrc,
                    "".to_owned(),
                    TrackStream {
                        stream_info: Some(stream_info),
                        rtp_read_stream,
                        rtp_interceptor,
                        rtcp_read_stream,
                        rtcp_interceptor,
                    },
                )
                .await?;
            }
        }

        Ok(())
    }

    pub(crate) async fn have_received(&self) -> bool {
        let received_tx = self.received_tx.lock().await;
        received_tx.is_none()
    }

    pub(crate) async fn start(&self, incoming: &TrackDetails) {
        let mut encoding_size = incoming.ssrcs.len();
        if incoming.rids.len() >= encoding_size {
            encoding_size = incoming.rids.len();
        };

        let mut encodings = vec![RTCRtpDecodingParameters::default(); encoding_size];
        for (i, encoding) in encodings.iter_mut().enumerate() {
            if incoming.rids.len() > i {
                encoding.rid = incoming.rids[i].clone();
            }
            if incoming.ssrcs.len() > i {
                encoding.ssrc = incoming.ssrcs[i];
            }

            encoding.rtx.ssrc = incoming.repair_ssrc;
        }

        if let Err(err) = self.receive(&RTCRtpReceiveParameters { encodings }).await {
            log::warn!("RTPReceiver Receive failed {}", err);
            return;
        }

        // set track id and label early so they can be set as new track information
        // is received from the SDP.
        for track_remote in &self.tracks().await {
            track_remote.set_id(incoming.id.clone()).await;
            track_remote.set_stream_id(incoming.stream_id.clone()).await;
        }
    }

    /// Stop irreversibly stops the RTPReceiver
    pub async fn stop(&self) -> Result<()> {
        self.closed_tx.notify_waiters();

        let received_tx_is_none = {
            let received_tx = self.received_tx.lock().await;
            received_tx.is_none()
        };

        let mut errs = vec![];
        if received_tx_is_none {
            let tracks = self.internal.tracks.write().await;
            for t in &*tracks {
                if let Some(rtcp_read_stream) = &t.stream.rtcp_read_stream {
                    if let Err(err) = rtcp_read_stream.close().await {
                        errs.push(err);
                    }
                }

                if let Some(rtp_read_stream) = &t.stream.rtp_read_stream {
                    if let Err(err) = rtp_read_stream.close().await {
                        errs.push(err);
                    }
                }

                if let Some(repair_rtcp_read_stream) = &t.repair_stream.rtcp_read_stream {
                    if let Err(err) = repair_rtcp_read_stream.close().await {
                        errs.push(err);
                    }
                }

                if let Some(repair_rtp_read_stream) = &t.repair_stream.rtp_read_stream {
                    if let Err(err) = repair_rtp_read_stream.close().await {
                        errs.push(err);
                    }
                }

                if let Some(stream_info) = &t.stream.stream_info {
                    self.internal
                        .interceptor
                        .unbind_remote_stream(stream_info)
                        .await;
                }

                if let Some(repair_stream_info) = &t.repair_stream.stream_info {
                    self.internal
                        .interceptor
                        .unbind_remote_stream(repair_stream_info)
                        .await;
                }
            }
        }

        flatten_errs(errs)
    }

    /// receiveForRtx starts a routine that processes the repair stream
    /// These packets aren't exposed to the user yet, but we need to process them for
    /// TWCC
    pub(crate) async fn receive_for_rtx(
        &self,
        ssrc: SSRC,
        rsid: String,
        repair_stream: TrackStream,
    ) -> Result<()> {
        let mut tracks = self.internal.tracks.write().await;
        let l = tracks.len();
        for t in &mut *tracks {
            if (ssrc != 0 && l == 1) || t.track.rid() == rsid {
                t.repair_stream = repair_stream;

                let receive_mtu = self.receive_mtu;
                let track = t.clone();
                tokio::spawn(async move {
                    let a = Attributes::new();
                    let mut b = vec![0u8; receive_mtu];
                    while let Some(repair_rtp_interceptor) = &track.repair_stream.rtp_interceptor {
                        //TODO: cancel repair_rtp_interceptor.read gracefully
                        //println!("repair_rtp_interceptor read begin with ssrc={}", ssrc);
                        if repair_rtp_interceptor.read(&mut b, &a).await.is_err() {
                            break;
                        }
                    }
                });

                return Ok(());
            }
        }

        Err(Error::ErrRTPReceiverForRIDTrackStreamNotFound)
    }
}
