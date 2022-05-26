use futures::{FutureExt, StreamExt};
use getopt::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::env;
use std::sync::Arc;
use std::vec::Vec;
use tokio::sync::{mpsc, watch, watch::Receiver, RwLock};
use tokio::time::*;
use uuid::Uuid;
use warp::ws::{Message, WebSocket};
use warp::{
    http::{Method, StatusCode},
    reply::json,
    Filter, Rejection, Reply,
};

use curio_lib::auth::*;
use curio_lib::consumer_service::ConsumerService;
use curio_lib::types::consumer::ConsumerTypes;
use curio_lib::types::messages::AuctionOutbid;
use curio_lib::types::messages::NotificationMessage;

#[derive(Debug, Clone)]
pub struct Client {
    pub user_id: usize,
    pub user_login: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IdResponse {
    id: String,
}

#[derive(Clone)]
pub struct AppState {
    pub jwt_secret: String,
}

type Clients = Arc<RwLock<HashMap<String, Client>>>;

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let mut opts = Parser::new(&args, "sc:");

    let mut server_string: String = String::from("127.0.0.1:6789");
    let mut client_string: String = String::from("127.0.0.1:5678");
    loop {
        match opts
            .next()
            .transpose()
            .expect("Failed to read command line argument.")
        {
            None => break,
            Some(opt) => match opt {
                Opt('s', Some(string)) => server_string = string,
                Opt('c', Some(string)) => client_string = string,
                _ => unreachable!(),
            },
        }
    }

    let jwt_secret = env::var("JWT_SECRET").expect("JWT_SECRET must be set");
    let app_state = AppState { jwt_secret };

    let cloned_client_string = client_string.clone();
    let (tx, rx) = watch::channel(NotificationMessage::Init);    
    tokio::spawn(async move {
        let subscriptions = vec![NotificationMessage::AuctionOutbid(
            AuctionOutbid::default(),
        )];
        
        let mut consumer_service = ConsumerService::new(
            server_string, 
            cloned_client_string,
            ConsumerTypes::WebsocketDispatch,
            subscriptions,
            tx
        );

        consumer_service.establish_connection().await;
    });
       
    let clients: Clients = Arc::new(RwLock::new(HashMap::new()));
    let health_route = warp::path!("health").and_then(health_handler);
    let auth_route = warp::path("auth")
        .and(warp::post())
        .and(warp::header("Authorization"))
        .and(with_clients(clients.clone()))
        .and(with_state(app_state.clone()))
        .and_then(auth_handler);

    let ws_route = warp::path("wss")
        .and(warp::ws())
        .and(warp::path::param())
        .and(with_clients(clients.clone()))
        .and(with_rx(rx))
        .and_then(ws_handler);

    let routes = health_route.or(auth_route).or(ws_route).with(
        warp::cors()
            .allow_credentials(true)
            .allow_any_origin()
            .allow_methods(&[Method::POST, Method::OPTIONS])
            .allow_headers(vec!["authorization", "Authorization"]),
    );

    let client_socket_addr: std::net::SocketAddr = client_string
        .parse()
        .expect("Failed to parse client string.");
    warp::serve(routes).run(client_socket_addr).await;
}

fn with_state(
    state: AppState,
) -> impl Filter<Extract = (AppState,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || state.clone())
}

fn with_clients(clients: Clients) -> impl Filter<Extract = (Clients,), Error = Infallible> + Clone {
    warp::any().map(move || clients.clone())
}

fn with_rx(
    rx: Receiver<NotificationMessage>,
) -> impl Filter<Extract = (Receiver<NotificationMessage>,), Error = Infallible> + Clone {
    warp::any().map(move || rx.clone())
}

pub async fn auth_handler(
    jwt_token: String,
    clients: Clients,
    state: AppState,
) -> std::result::Result<warp::http::Response<warp::hyper::Body>, Rejection> {
    match decode_and_validate_token(state.jwt_secret.as_ref(), jwt_token.as_ref()) {
        Ok(data) => {
            let my_id = Uuid::new_v4().to_string();
            clients.write().await.insert(
                my_id.clone(),
                Client {
                    user_id: data.nameid.parse().expect("Token id is not valid."),
                    user_login: data.unique_name,
                },
            );

            println!("Client token validated for id: {}", my_id);

            let response = IdResponse { id: my_id };
            Ok(json(&response).into_response())
        }
        Err(e) => {
            eprintln!(
                "Failed to decode or validate JWT token: {}, Error: {}",
                jwt_token, e
            );
            Ok(StatusCode::UNAUTHORIZED.into_response())
        }
    }
}

pub async fn health_handler() -> std::result::Result<impl Reply, Rejection> {
    Ok(StatusCode::OK)
}

pub async fn ws_handler(
    ws: warp::ws::Ws,
    id: String,
    clients: Clients,
    rx: Receiver<NotificationMessage>,
) -> std::result::Result<impl Reply, Rejection> {
    let client = clients.read().await.get(&id).cloned();
    match client {
        Some(c) => Ok(ws.on_upgrade(move |socket| client_connection(socket, id, clients, c, rx))),
        None => Err(warp::reject::not_found()),
    }
}

pub async fn client_connection(
    ws: WebSocket,
    id: String,
    clients: Clients,
    mut _client: Client,
    mut _rx: Receiver<NotificationMessage>,
) {
    let (user_ws_tx, mut _user_ws_rx) = ws.split();
    let (channel_tx, channel_rx) = mpsc::unbounded_channel();
    tokio::task::spawn(channel_rx.forward(user_ws_tx).map(|result| {
        if let Err(e) = result {
            eprintln!("websocket send error: {}", e);
        }
    }));

    loop {
        if let Err(_disconnected) = channel_tx.send(Ok(Message::text("testing"))) {
            break;
        }

        delay_for(Duration::from_secs(2)).await;
    }

    /*while let Some(msg) = rx.recv().await {

    }*/

    println!("Client {} disconnected.", id);
    clients.write().await.remove(&id);
}
