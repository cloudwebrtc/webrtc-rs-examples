use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Extension,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{get, get_service},
    AddExtensionLayer, Router,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::time::Duration;
use tower_http::services::ServeDir;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS, MIME_TYPE_VP8};
use webrtc::api::APIBuilder;
use webrtc::api::API;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::io::ivf_reader::IVFReader;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;
use webrtc::Error;

struct AppState {
    api: API,
    peer_set: Mutex<HashMap<String, Arc<RTCPeerConnection>>>,
    local_tracks: Mutex<HashMap<String, Arc<TrackLocalStaticRTP>>>,
}

macro_rules! defer {
    {$($body:stmt;)+} => {
        let _guard = {
            pub struct Guard<F: FnOnce()>(Option<F>);
            impl<F: FnOnce()> Drop for Guard<F> {
                fn drop(&mut self) {
                    if let Some(f) = (self.0).take() {
                        f()
                    }
                }
            }
            Guard(Some(||{
                $($body)+
            }))
        };
    };
}

#[tokio::main]
async fn main() -> Result<()> {
    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();

    m.register_default_codecs()?;

    // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
    // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
    // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
    // for each PeerConnection.
    let mut registry = Registry::new();

    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut m).await?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let peer_map: HashMap<String, Arc<RTCPeerConnection>> = HashMap::new();
    let peer_set = Mutex::new(peer_map);
    let local_tracks_map: HashMap<String, Arc<TrackLocalStaticRTP>> = HashMap::new();
    let local_tracks = Mutex::new(local_tracks_map);
    let app_state = Arc::new(AppState {
        api,
        peer_set,
        local_tracks,
    });

    let app_state_clone = app_state.clone();

    let app = Router::new()
        .fallback(
            get_service(
                ServeDir::new("examples/sfu-ws/assets").append_index_html_on_directories(true),
            )
            .handle_error(|error: std::io::Error| async move {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Unhandled internal error: {}", error),
                )
            }),
        )
        .route("/websocket", get(ws_handler))
        .layer(AddExtensionLayer::new(app_state));

    tokio::spawn(async move {
        let result = Result::<usize>::Ok(0);
        while result.is_ok() {
            let timeout = tokio::time::sleep(Duration::from_secs(1));
            tokio::pin!(timeout);
            tokio::select! {
                _ = timeout.as_mut() => {
                    app_state_clone.peer_set.lock().unwrap().iter().for_each(|(_, item)| {
                        let pc = item.clone();
                        tokio::spawn(async move {
                            let receivers  = pc.get_receivers().await;
                            for receiver in receivers {
                                let track = receiver.track().await;
                                if let Some(track) = track {
                                    let media_ssrc = track.ssrc();
                                    pc.write_rtcp(&[Box::new(PictureLossIndication{
                                        sender_ssrc: 0,
                                        media_ssrc,
                                    })]).await;
                                }
                            }
                        });
                        println!("connection_state: {:?}", item.connection_state());
                    });
                }
            };
        }
    });
    // run it with hyper
    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
    println!("listening on http://{}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Extension(state): Extension<Arc<AppState>>,
) -> impl IntoResponse {
    println!("client connected");
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    // Prepare the configuration
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };
    let peer_id = &format!("pc-{}", rand::random::<u32>());
    let pc = Arc::new(state.api.new_peer_connection(config).await.unwrap());
    let state_clone = state.clone();
    pc.on_track(Box::new(
        move |track: Option<Arc<TrackRemote>>, _receiver: Option<Arc<RTCRtpReceiver>>| {
            if let Some(track) = track {
                let state_clone = state.clone();
                println!("add track");
                add_track(state_clone, track);
            }
            Box::pin(async {})
        },
    ))
    .await;

    state_clone
        .peer_set
        .lock()
        .unwrap()
        .insert(peer_id.to_string(), pc);

    if let Some(msg) = socket.recv().await {
        if let Ok(msg) = msg {
            println!("client says: {:?}", msg);
        } else {
            println!("client disconnected");
            return;
        }
    }

    loop {
        if socket
            .send(Message::Text(String::from("Hi!")))
            .await
            .is_err()
        {
            println!("client disconnected");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    let pc = state_clone.peer_set.lock().unwrap().remove(peer_id);
    println!("pc.close");
    pc.unwrap().close().await;
}

async fn add_track(state: Arc<AppState>, remote_track: Arc<TrackRemote>) {
    let track_id = remote_track.id().await;
    let stream_id = remote_track.stream_id().await;
    let local_track = Arc::new(TrackLocalStaticRTP::new(
        remote_track.codec().await.capability,
        track_id.clone(),
        stream_id,
    ));
    state
        .local_tracks
        .lock()
        .unwrap()
        .insert(track_id.clone(), local_track.clone());


    // Read RTP packets being sent to webrtc-rs
    while let Ok((rtp, _)) = remote_track.read_rtp().await {
        if let Err(err) = local_track.write_rtp(&rtp).await {
            if Error::ErrClosedPipe != err {
                print!("output track write_rtp got error: {} and break", err);
                break;
            } else {
                print!("output track write_rtp got error: {}", err);
            }
        }
    }

    state.local_tracks.lock().unwrap().remove(&track_id);
}
