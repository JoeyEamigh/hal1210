use std::time::Duration;

use daemoncomm::{MessageToClient, MessageToClientData, MessageToServerData, client::Hal1210Client};
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(2);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_command_roundtrip() {
  crate::monitoring::init_logger();

  let server_cancel = tokio_util::sync::CancellationToken::new();
  let (client_req_tx, mut client_req_rx) = tokio::sync::mpsc::unbounded_channel();
  let (server_res_tx, server_res_rx) = tokio::sync::mpsc::unbounded_channel();

  let client_man = crate::client::ClientMan::init(client_req_tx, server_res_rx, server_cancel.clone())
    .await
    .expect("could not initialize communication manager");
  let com_handle = client_man.spawn();

  let (incoming_tx, mut incoming_rx) = tokio::sync::mpsc::unbounded_channel();
  let client = Hal1210Client::connect(incoming_tx, tokio_util::sync::CancellationToken::new())
    .await
    .expect("could not connect test client");

  let request_id = client
    .send(MessageToServerData::SetManualMode { enabled: true })
    .expect("failed to send manual mode command");

  let client_req = timeout(TIMEOUT, client_req_rx.recv())
    .await
    .expect("timed out waiting for client manager to forward request")
    .expect("client manager channel closed unexpectedly");

  let addr = client_req.addr;
  let message = match client_req.data {
    crate::client::ClientReqData::Message(message) => message,
    other => panic!("unexpected client manager event: {other:?}"),
  };

  assert_eq!(message.id, request_id, "client manager rewrote message id");
  match message.data {
    MessageToServerData::SetManualMode { enabled } => {
      assert!(enabled, "manual mode request should enable manual mode");
    }
    other => panic!("unexpected message payload forwarded to bridge: {other:?}"),
  }

  let reply = MessageToClient {
    id: request_id,
    data: MessageToClientData::ManualMode { enabled: true },
  };
  server_res_tx
    .send(crate::client::ServerRes::new(addr, reply))
    .expect("failed to send response to client manager");

  let response = timeout(TIMEOUT, incoming_rx.recv())
    .await
    .expect("timed out waiting for client response")
    .expect("test client dropped before receiving response");

  assert_eq!(response.id, request_id, "response id should match request");
  match response.data {
    MessageToClientData::ManualMode { enabled } => {
      assert!(enabled, "manual mode response should report enabled");
    }
    other => panic!("unexpected client response payload: {other:?}"),
  }

  client.cancel();
  let disconnect = timeout(TIMEOUT, client_req_rx.recv())
    .await
    .expect("timed out waiting for disconnect event")
    .expect("client manager closed channel before disconnect event");

  assert!(
    matches!(disconnect.data, crate::client::ClientReqData::Disconnected),
    "expected disconnect event, got {:?}",
    disconnect.data
  );

  server_cancel.cancel();
  com_handle
    .await
    .expect("communication manager task panicked before shutdown");
}
