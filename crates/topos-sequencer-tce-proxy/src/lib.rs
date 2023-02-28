//!
//! The module to handle incoming events from the friendly TCE node
//!

use crate::Error::InvalidChannelError;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;
use tonic::transport::channel;
use topos_core::api::checkpoints::{TargetCheckpoint, TargetStreamPosition};
use topos_core::api::tce::v1::GetSourceHeadRequest;
use topos_core::{
    api::tce::v1::{
        api_service_client::ApiServiceClient, watch_certificates_request,
        watch_certificates_response, SubmitCertificateRequest, WatchCertificatesRequest,
        WatchCertificatesResponse,
    },
    uci::{Certificate, SubnetId},
};
use topos_sequencer_types::*;
use tracing::{error, info, warn};

const CERTIFICATE_OUTBOUND_CHANNEL_SIZE: usize = 100;
const CERTIFICATE_INBOUND_CHANNEL_SIZE: usize = 100;
const TCE_PROXY_COMMAND_CHANNEL_SIZE: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("tonic transport error")]
    TonicTransportError {
        #[from]
        source: tonic::transport::Error,
    },
    #[error("tonic error")]
    TonicStatusError {
        #[from]
        source: tonic::Status,
    },
    #[error("invalid channel error")]
    InvalidChannelError,
    #[error("invalid tce endpoint error")]
    InvalidTceEndpoint,
    #[error("invalid subnet id error")]
    InvalidSubnetId,
    #[error("invalid certificate error")]
    InvalidCertificate,
    #[error("hex conversion error {source}")]
    HexConversionError {
        #[from]
        source: hex::FromHexError,
    },
    #[error(
        "Unable to get source head certificate for subnet id {subnet_id:?}, details: {details}"
    )]
    UnableToGetSourceHeadCertificate {
        subnet_id: SubnetId,
        details: String,
    },

    #[error("Certificate source head empty for subnet id {subnet_id:?}")]
    SourceHeadEmpty { subnet_id: SubnetId },
}

pub struct TceProxyConfig {
    pub subnet_id: SubnetId,
    pub base_tce_api_url: String,
    pub positions: Vec<TargetStreamPosition>,
}

/// Proxy with the TCE
///
/// 1) Fetch the delivered certificates from the TCE
/// 2) Submit the new certificate to the TCE
pub struct TceProxyWorker {
    pub config: TceProxyConfig,
    commands: mpsc::UnboundedSender<TceProxyCommand>,
    events: mpsc::UnboundedReceiver<TceProxyEvent>,
}

enum TceClientCommand {
    // Get head certificate that was sent to the TCE node for this subnet
    GetSourceHead {
        subnet_id: topos_core::uci::SubnetId,
        sender: oneshot::Sender<Result<Certificate, Error>>,
    },
    // Open the stream to the TCE node
    // Mark the position from which TCE node certificates should be retrieved
    OpenStream {
        target_checkpoint: TargetCheckpoint,
    },
    // Send generated certificate to the TCE node
    SendCertificate {
        cert: Box<Certificate>,
    },
    Shutdown,
}

pub struct TceClient {
    subnet_id: topos_core::uci::SubnetId,
    tce_endpoint: String,
    command_sender: mpsc::Sender<TceClientCommand>,
}

impl TceClient {
    pub async fn open_stream(&self, positions: Vec<TargetStreamPosition>) -> Result<(), Error> {
        self.command_sender
            .send(TceClientCommand::OpenStream {
                target_checkpoint: TargetCheckpoint {
                    target_subnet_ids: vec![self.subnet_id],
                    positions,
                },
            })
            .await
            .map_err(|_| InvalidChannelError)?;
        Ok(())
    }
    pub async fn send_certificate(&mut self, cert: Certificate) -> Result<(), Error> {
        self.command_sender
            .send(TceClientCommand::SendCertificate {
                cert: Box::new(cert),
            })
            .await
            .map_err(|_| InvalidChannelError)?;
        Ok(())
    }
    pub async fn close(&mut self) -> Result<(), Error> {
        self.command_sender
            .send(TceClientCommand::Shutdown)
            .await
            .map_err(|_| InvalidChannelError)?;
        Ok(())
    }

    pub async fn get_source_head(&mut self) -> Result<Certificate, Error> {
        #[allow(clippy::type_complexity)]
        let (sender, receiver): (
            oneshot::Sender<Result<Certificate, Error>>,
            oneshot::Receiver<Result<Certificate, Error>>,
        ) = oneshot::channel();
        self.command_sender
            .send(TceClientCommand::GetSourceHead {
                subnet_id: self.subnet_id,
                sender,
            })
            .await
            .map_err(|_| InvalidChannelError)?;

        receiver.await.map_err(|_| InvalidChannelError)?
    }
}

#[derive(Default)]
pub struct TceClientBuilder {
    tce_endpoint: Option<String>,
    subnet_id: Option<SubnetId>,
    tce_proxy_event_sender: Option<mpsc::UnboundedSender<TceProxyEvent>>,
}

impl TceClientBuilder {
    pub fn set_tce_endpoint<T: ToString>(mut self, endpoint: T) -> Self {
        self.tce_endpoint = Some(endpoint.to_string());
        self
    }

    pub fn set_subnet_id(mut self, subnet_id: SubnetId) -> Self {
        self.subnet_id = Some(subnet_id);
        self
    }

    pub fn set_proxy_event_sender(
        mut self,
        tce_proxy_event_sender: mpsc::UnboundedSender<TceProxyEvent>,
    ) -> Self {
        self.tce_proxy_event_sender = Some(tce_proxy_event_sender);
        self
    }

    pub async fn build_and_launch(
        self,
    ) -> Result<(TceClient, impl futures::stream::Stream<Item = Certificate>), Error> {
        // Channel used to pass received certificates (certificates pushed TCE node) from the TCE client to the application
        let (inbound_certificate_sender, inbound_certificate_receiver) =
            mpsc::channel(CERTIFICATE_INBOUND_CHANNEL_SIZE);

        let tce_endpoint = self
            .tce_endpoint
            .as_ref()
            .ok_or(Error::InvalidTceEndpoint)?
            .clone();
        // Connect to tce node service using backoff strategy
        let mut tce_grpc_client =
            match connect_to_tce_service_with_retry(tce_endpoint.clone()).await {
                Ok(client) => {
                    info!("Connected to tce service on endpoint {}", &tce_endpoint);
                    client
                }
                Err(e) => {
                    error!("Unable to connect to tce client, error details: {}", e);
                    return Err(e);
                }
            };

        // Channel used to initiate watch_certificates_request::Command that will be sent to the TCE through stream
        let (outbound_stream_command_sender, mut outbound_stream_command_receiver) =
            mpsc::channel::<WatchCertificatesRequest>(CERTIFICATE_OUTBOUND_CHANNEL_SIZE);

        // Outbound stream used to send watch_certificates_request::Command to the TCE node service
        let outbound_watch_certificates_stream = async_stream::stream! {
            loop {
                while let Some(request) = outbound_stream_command_receiver.recv().await {
                    yield request;
                }
            }
        };

        // Call TCE service watch certificates, get inbound response stream
        let mut inbound_watch_certificates_stream: tonic::Streaming<WatchCertificatesResponse> =
            tce_grpc_client
                .watch_certificates(outbound_watch_certificates_stream)
                .await
                .map(|r| r.into_inner())?;

        // Channel used to shut down task for inbound stream responses processing
        let (inbound_shutdown_sender, mut inbound_shutdown_receiver) =
            mpsc::unbounded_channel::<()>();

        let subnet_id = *self.subnet_id.as_ref().ok_or(Error::InvalidSubnetId)?;

        let tce_proxy_event_sender = self.tce_proxy_event_sender.clone();

        // Run task and process inbound watch certificate stream responses
        tokio::spawn(async move {
            // Listen for feedback from TCE service (WatchCertificatesResponse)
            info!(
                "Entering watch certificate response loop for tce node {} for subnet id {:?}",
                &tce_endpoint, &subnet_id
            );
            loop {
                tokio::select! {
                    Some(response) = inbound_watch_certificates_stream.next() => {
                        match response {
                            Ok(watch_certificate_response) => match watch_certificate_response.event {
                                // Received CertificatePushed event from TCE (new certificate has been received from TCE)
                                Some(watch_certificates_response::Event::CertificatePushed(
                                    certificate_pushed,
                                )) => {
                                    info!("Certificate {:?} received from tce", &certificate_pushed);
                                    if let Some(certificate) = certificate_pushed.certificate {
                                        let cert = match certificate.clone().try_into() {
                                            Ok(c) => c,
                                            Err(_) => {
                                                error!("invalid certificate conversion for {certificate:?}");
                                                continue;
                                            }
                                        };
                                        if let Err(e) = inbound_certificate_sender
                                            .send(cert)
                                            .await
                                        {
                                            error!(
                                                "unable to pass received certificate to application, error details: {}",
                                                e.to_string()
                                            )
                                        }
                                    }
                                }
                                // Confirmation from TCE that stream has been opened
                                Some(watch_certificates_response::Event::StreamOpened(stream_opened)) => {
                                    info!(
                                        "Watch certificate stream opened for for tce node {} subnet ids {:?}",
                                         &tce_endpoint, stream_opened.subnet_ids
                                    );
                                }
                                None => {
                                    warn!(
                                        "Watch certificate stream received None object from tce node {}", &tce_endpoint
                                    );
                                }
                            },
                            Err(e) => {
                                error!(
                                    "Watch certificates response error for tce node {} subnet_id {:?}, error details: {}",
                                    &tce_endpoint, &subnet_id, e.to_string()
                                );
                                // Send warning to restart TCE proxy
                                if let Some(tce_proxy_event_sender) = tce_proxy_event_sender.clone() {
                                    if let Err(e) = tce_proxy_event_sender.send(TceProxyEvent::WatchCertificatesChannelFailed) {
                                          error!("Unable to send watch certificates channel failed signal: {e}");
                                    }
                                }
                            }
                        }
                    }
                    Some(_) = inbound_shutdown_receiver.recv() => {
                        info!("Finishing watch certificates task...");
                        // Finish this task listener
                        break;
                    }
                }
            }
            info!(
                "Finishing watch certificate task for tce node {} subnet_id {:?}",
                &tce_endpoint, &subnet_id
            );
        });

        // Channel used to pass commands from the application to the TCE proxy
        // To close to chanel worker task, send None as Certificate
        let (tce_command_sender, mut tce_command_receiver) =
            mpsc::channel::<TceClientCommand>(TCE_PROXY_COMMAND_CHANNEL_SIZE);

        // Run task for sending certificates to the TCE stream
        let tce_endpoint = self
            .tce_endpoint
            .as_ref()
            .ok_or(Error::InvalidTceEndpoint)?
            .clone();
        tokio::spawn(async move {
            info!(
                "Entering tce proxy command loop for stream {}",
                &tce_endpoint
            );
            loop {
                tokio::select! {
                   command = tce_command_receiver.recv() => {
                        match command {
                           Some(TceClientCommand::SendCertificate {cert}) =>  {
                                let cert_id = cert.id;
                                let previous_cert_id = cert.prev_id;
                                match tce_grpc_client
                                .submit_certificate(SubmitCertificateRequest {
                                    certificate: Some(topos_core::api::uci::v1::Certificate::from(*cert)),
                                })
                                .await
                                .map(|r| r.into_inner()) {
                                    Ok(_response)=> {
                                        info!("Certificate cert_id: {:?} previous_cert_id: {:?} successfully sent to tce {}", &cert_id, &previous_cert_id, &tce_endpoint);
                                    }
                                    Err(e) => {
                                        error!("Certificate submit failed, error details: {}", e);
                                    }
                                }
                            }
                            Some(TceClientCommand::OpenStream {target_checkpoint}) =>  {
                                // Send command to TCE to open stream with my subnet id
                                info!(
                                    "Sending OpenStream command to tce node {} for subnet id {:?}",
                                    &tce_endpoint, &subnet_id
                                );
                                if let Err(e) = outbound_stream_command_sender
                                    .send(
                                            watch_certificates_request::OpenStream {
                                                target_checkpoint:
                                                    Some(target_checkpoint.into()),
                                                source_checkpoint: None
                                            }.into(),
                                    )
                                    .await
                                    {
                                        error!(
                                            "Unable to send OpenStream command, error details: {}",
                                            e.to_string()
                                        )
                                    }
                            }
                            Some(TceClientCommand::Shutdown) =>  {
                                inbound_shutdown_sender.send(()).expect("valid channel for shutting down task");
                                break;
                            }
                            Some(TceClientCommand::GetSourceHead {subnet_id, sender}) =>  {
                                    let result: Result<Certificate, Error> = match tce_grpc_client
                                    .get_source_head(GetSourceHeadRequest {
                                        subnet_id: Some(subnet_id.into())
                                    })
                                    .await
                                    .map(|r| r.into_inner().certificate) {
                                        Ok(Some(certificate)) => Ok(certificate.try_into().map_err(|_| Error::InvalidCertificate)?),
                                        Ok(None) => {
                                            Err(Error::SourceHeadEmpty{subnet_id})
                                        },
                                        Err(e) => {
                                            Err(Error::UnableToGetSourceHeadCertificate{subnet_id, details: e.to_string()})
                                    },
                                };

                                if sender.send(result).is_err() {
                                    error!("Unable to pass result of the source head, channel failed");
                                };
                            }
                            None => {
                                panic!("None should not be possible");
                            }
                        }
                    }
                }
            }
            info!(
                "Finished submit certificate loop for stream {}",
                &tce_endpoint
            );
            Result::<(), Error>::Ok(())
        });

        Ok((
            TceClient {
                subnet_id: self.subnet_id.ok_or(Error::InvalidSubnetId)?,
                tce_endpoint: self.tce_endpoint.ok_or(Error::InvalidTceEndpoint)?,
                command_sender: tce_command_sender,
            },
            tokio_stream::wrappers::ReceiverStream::new(inbound_certificate_receiver),
        ))
    }
}

async fn connect_to_tce_service_with_retry(
    endpoint: String,
) -> Result<ApiServiceClient<tonic::transport::channel::Channel>, Error> {
    info!(
        "Connecting to tce service endpoint {} using backoff strategy...",
        endpoint
    );
    let op = || async {
        let channel = channel::Endpoint::from_shared(endpoint.clone())?
            .connect()
            .await
            .map_err(|e| {
                error!(
                    "Unable to connect to tce service {}, error details: {}",
                    &endpoint,
                    e.to_string()
                );
                e
            })?;
        Ok(ApiServiceClient::new(channel))
    };
    backoff::future::retry(backoff::ExponentialBackoff::default(), op)
        .await
        .map_err(|e| {
            error!("Error connecting to  service api {} ...", e.to_string());
            Error::TonicTransportError { source: e }
        })
}

impl TceProxyWorker {
    pub async fn new(config: TceProxyConfig) -> Result<(Self, Option<Certificate>), Error> {
        let (command_sender, mut command_rcv) = mpsc::unbounded_channel::<TceProxyCommand>();
        let (evt_sender, evt_rcv) = mpsc::unbounded_channel::<TceProxyEvent>();

        let (mut tce_client, mut receiving_certificate_stream) = TceClientBuilder::default()
            .set_subnet_id(config.subnet_id)
            .set_tce_endpoint(&config.base_tce_api_url)
            .set_proxy_event_sender(evt_sender.clone())
            .build_and_launch()
            .await?;

        // TODO: retrieve target stream position from the subnet node
        let target_stream_positions = config.positions.clone();

        tce_client.open_stream(target_stream_positions).await?;

        // Retrieve source head from TCE node, so that
        // we know from where to start creating certificates
        let source_head_certificate = match tce_client.get_source_head().await {
            Ok(certificate) => Some(certificate),
            Err(Error::SourceHeadEmpty { subnet_id: _ }) => {
                //This is also OK, TCE node does not have any data about certificates
                //We should start certificate production from scratch
                None
            }
            Err(e) => {
                return Err(e);
            }
        };

        tokio::spawn(async move {
            info!(
                "Entering tce proxy worker loop to handle app commands for tce endpoint {}",
                &tce_client.tce_endpoint
            );
            loop {
                tokio::select! {
                    // process TCE proxy commands received from application
                    Some(cmd) = command_rcv.recv() => {
                        match cmd {
                            TceProxyCommand::SubmitCertificate(cert) => {
                                info!(
                                    "Submitting new certificate to the TCE network: {:?}",
                                    &cert
                                );
                                if let Err(e) = tce_client.send_certificate(*cert).await {
                                    error!("failed to pass certificate to tce client, error details: {}", e);
                                }
                            }
                            TceProxyCommand::Shutdown(sender) => {
                                info!("Received TceProxyCommand::Shutdown command, closing tce client...");
                                if let Err(e) = tce_client.close().await {
                                    error!("Unable to shutdown tce client, error details: {}", e);
                                }
                                 _ = sender.send(());
                                break;
                            }
                        }
                    }
                     // process certificates received from the TCE node
                    Some(cert) = receiving_certificate_stream.next() => {
                        info!("Received certificate from TCE {:?}", cert);
                        evt_sender.send(TceProxyEvent::NewDeliveredCerts(vec![cert])).expect("send");
                    }
                }
            }
            info!(
                "Exiting tce proxy worker handle loop for endpoint {}",
                &tce_client.tce_endpoint
            );
        });

        // Save channels and handles, return latest tce known certificate
        Ok((
            Self {
                commands: command_sender,
                events: evt_rcv,
                config,
            },
            source_head_certificate,
        ))
    }

    /// Send commands to TCE
    pub fn send_command(&self, cmd: TceProxyCommand) -> Result<(), String> {
        match self.commands.send(cmd) {
            Ok(_) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Pollable (in select!) event listener
    pub async fn next_event(&mut self) -> Result<TceProxyEvent, String> {
        let event = self.events.recv().await;
        Ok(event.unwrap())
    }

    /// Shut down TCE proxy
    pub async fn shutdown(&self) -> Result<(), String> {
        info!("Shutting down TCE proxy worker...");
        let (sender, receiver) = oneshot::channel();
        match self.commands.send(TceProxyCommand::Shutdown(sender)) {
            Ok(_) => {}
            Err(e) => {
                error!("Error sending shutdown signal to TCE worker {e}");
                return Err(e.to_string());
            }
        };
        receiver.await.map_err(|e| e.to_string())
    }
}
