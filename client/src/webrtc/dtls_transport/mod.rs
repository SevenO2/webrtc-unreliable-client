
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use crate::webrtc::dtls::config::ClientAuthType;
use crate::webrtc::dtls::conn::DTLSConn;
use tokio::sync::Mutex;
use crate::webrtc::util::Conn;

use dtls_role::*;

use crate::webrtc::dtls_transport::dtls_parameters::DTLSParameters;
use crate::webrtc::dtls_transport::dtls_transport_state::RTCDtlsTransportState;
use crate::webrtc::error::{Error, Result};
use crate::webrtc::ice_transport::ice_transport_state::RTCIceTransportState;
use crate::webrtc::ice_transport::RTCIceTransport;
use crate::webrtc::mux::mux_func::match_dtls;
use crate::webrtc::peer_connection::certificate::RTCCertificate;

pub mod dtls_fingerprint;
pub mod dtls_parameters;
pub mod dtls_role;
pub mod dtls_transport_state;

pub type OnDTLSTransportStateChangeHdlrFn = Box<
    dyn (FnMut(RTCDtlsTransportState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

/// DTLSTransport allows an application access to information about the DTLS
/// transport over which RTP and RTCP packets are sent and received by
/// RTPSender and RTPReceiver, as well other data such as SCTP packets sent
/// and received by data channels.
#[derive(Default)]
pub struct RTCDtlsTransport {
    pub ice_transport: Arc<RTCIceTransport>,
    pub certificates: Vec<RTCCertificate>,

    pub remote_parameters: Mutex<DTLSParameters>,
    pub remote_certificate: Mutex<Bytes>,
    pub state: AtomicU8, //DTLSTransportState,
    pub on_state_change_handler: Arc<Mutex<Option<OnDTLSTransportStateChangeHdlrFn>>>,
    pub conn: Mutex<Option<Arc<DTLSConn>>>,
}

impl RTCDtlsTransport {
    pub fn new(
        ice_transport: Arc<RTCIceTransport>,
        certificates: Vec<RTCCertificate>,
    ) -> Self {
        RTCDtlsTransport {
            ice_transport,
            certificates,
            state: AtomicU8::new(RTCDtlsTransportState::New as u8),
            ..Default::default()
        }
    }

    pub async fn conn(&self) -> Option<Arc<DTLSConn>> {
        let conn = self.conn.lock().await;
        conn.clone()
    }

    /// returns the currently-configured ICETransport or None
    /// if one has not been configured
    pub fn ice_transport(&self) -> &RTCIceTransport {
        &self.ice_transport
    }

    /// state_change requires the caller holds the lock
    async fn state_change(&self, state: RTCDtlsTransportState) {
        self.state.store(state as u8, Ordering::SeqCst);
        let mut handler = self.on_state_change_handler.lock().await;
        if let Some(f) = &mut *handler {
            f(state).await;
        }
    }

    /// on_state_change sets a handler that is fired when the DTLS
    /// connection state changes.
    pub async fn on_state_change(&self, f: OnDTLSTransportStateChangeHdlrFn) {
        let mut on_state_change_handler = self.on_state_change_handler.lock().await;
        *on_state_change_handler = Some(f);
    }

    /// state returns the current dtls_transport transport state.
    pub fn state(&self) -> RTCDtlsTransportState {
        self.state.load(Ordering::SeqCst).into()
    }

    /// get_local_parameters returns the DTLS parameters of the local DTLSTransport upon construction.
    pub fn get_local_parameters(&self) -> Result<DTLSParameters> {
        let mut fingerprints = vec![];

        for c in &self.certificates {
            fingerprints.extend(c.get_fingerprints()?);
        }

        Ok(DTLSParameters {
            role: DTLSRole::Auto, // always returns the default role
            fingerprints,
        })
    }

    /// get_remote_certificate returns the certificate chain in use by the remote side
    /// returns an empty list prior to selection of the remote certificate
    pub async fn get_remote_certificate(&self) -> Bytes {
        let remote_certificate = self.remote_certificate.lock().await;
        remote_certificate.clone()
    }

    async fn prepare_transport(
        &self,
        remote_parameters: DTLSParameters,
    ) -> Result<(DTLSRole, crate::webrtc::dtls::config::Config)> {
        self.ensure_ice_conn()?;

        if self.state() != RTCDtlsTransportState::New {
            return Err(Error::ErrInvalidDTLSStart);
        }

        {
            let mut rp = self.remote_parameters.lock().await;
            *rp = remote_parameters;
        }

        let certificate = if let Some(cert) = self.certificates.first() {
            cert.certificate.clone()
        } else {
            return Err(Error::ErrNonCertificate);
        };
        self.state_change(RTCDtlsTransportState::Connecting).await;

        Ok((
            DTLSRole::Client,
            crate::webrtc::dtls::config::Config {
                certificates: vec![certificate],
                srtp_protection_profiles: vec![],
                client_auth: ClientAuthType::RequireAnyClientCert,
                insecure_skip_verify: true,
                ..Default::default()
            },
        ))
    }

    /// start DTLS transport negotiation with the parameters of the remote DTLS transport
    pub async fn start(&self, remote_parameters: DTLSParameters) -> Result<()> {
        let dtls_conn_result = if let Some(dtls_endpoint) =
            self.ice_transport.new_endpoint(Box::new(match_dtls)).await
        {
            let (_, dtls_config) = self.prepare_transport(remote_parameters).await?;

            // Connect as DTLS Client/Server, function is blocking and we
            // must not hold the DTLSTransport lock
            crate::webrtc::dtls::conn::DTLSConn::new(
                dtls_endpoint as Arc<dyn Conn + Send + Sync>,
                dtls_config,
                true,
                None,
            )
                .await
        } else {
            Err(crate::webrtc::dtls::Error::Other(
                "ice_transport.new_endpoint failed".to_owned(),
            ))
        };

        let dtls_conn = match dtls_conn_result {
            Ok(dtls_conn) => dtls_conn,
            Err(err) => {
                self.state_change(RTCDtlsTransportState::Failed).await;
                return Err(err.into());
            }
        };

        {
            let mut conn = self.conn.lock().await;
            *conn = Some(Arc::new(dtls_conn));
        }
        self.state_change(RTCDtlsTransportState::Connected).await;

        Ok(())
    }

    pub fn ensure_ice_conn(&self) -> Result<()> {
        if self.ice_transport.state() == RTCIceTransportState::New {
            Err(Error::ErrICEConnectionNotStarted)
        } else {
            Ok(())
        }
    }
}
