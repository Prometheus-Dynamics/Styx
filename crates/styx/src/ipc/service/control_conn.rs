//! A control connection of a camera service: a client sets, reads or lists a camera's controls,
//! or subscribes to their changes, one request at a time, until it closes the connection.
//!
//! The connection is not a frame client: whatever it asks, it never joins a camera's plan, so
//! it does not count among the camera's clients (planning, restarts, idling, metrics). A
//! control client without frames ([`ControlClient`](crate::ipc::ControlClient)) has only
//! these connections.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::camera::Camera;
use super::{Connection, Service};
use crate::ipc::metrics::memfd;
use crate::ipc::wire::{self, ClientMessage, ControlOp, ControlReply, ControlRequest};

/// Answer `first` and the requests after it; a subscription lasts while the connection does.
pub(super) fn serve(service: &Service, conn: &mut Connection, first: ControlRequest) {
    let mut subscribed: Vec<(Arc<Camera>, u64)> = Vec::new();
    let mut named = Named::default();
    if answer(service, conn, first, &mut subscribed, &mut named).is_ok() {
        'serve: while !service.stopping.load(Ordering::Acquire) {
            let Ok(messages) = conn.poll(Duration::from_millis(100)) else {
                break;
            };
            for message in messages {
                if let ClientMessage::Control(request) = message
                    && answer(service, conn, *request, &mut subscribed, &mut named).is_err()
                {
                    break 'serve;
                }
            }
        }
    }
    for (camera, key) in subscribed {
        camera.unsubscribe(key);
    }
}

/// The camera a request without a token named last on this connection, so a control client's
/// requests do not look the camera up (probing, for a service of all cameras) every time.
#[derive(Default)]
struct Named(Option<(Option<String>, Arc<Camera>)>);

fn answer(
    service: &Service,
    conn: &Connection,
    request: ControlRequest,
    subscribed: &mut Vec<(Arc<Camera>, u64)>,
    named: &mut Named,
) -> std::io::Result<()> {
    let seq = request.seq;
    let refused = |why: String| ControlReply::Refused(crate::ipc::ControlRefusal::Unsupported(why));
    let found = match &named.0 {
        Some((selector, camera)) if request.token.is_none() && *selector == request.camera => {
            Ok((camera.clone(), service.control_caller(conn.peer())))
        }
        _ => service.control_camera(&request, conn.peer()),
    };
    if request.token.is_none()
        && let Ok((camera, _)) = &found
    {
        named.0 = Some((request.camera.clone(), camera.clone()));
    }
    let (camera, caller) = match found {
        Ok(found) => found,
        Err(why) => {
            return conn
                .send(&wire::encode_control_reply(seq, &refused(why)))
                .map(drop);
        }
    };
    let policy = &service.config.controls;
    let reply = match request.op {
        ControlOp::Set(target, value) => {
            match camera.set_control(target, value, &caller, &service.config, &service.counters) {
                Ok(applied) => ControlReply::Applied(applied),
                Err(refusal) => ControlReply::Refused(refusal),
            }
        }
        ControlOp::Get(target) => match camera.get_control(target) {
            Ok((id, value)) => ControlReply::Value(id, value),
            Err(refusal) => ControlReply::Refused(refusal),
        },
        ControlOp::List => {
            let list = camera.list_controls(&caller, policy);
            let body = wire::encode_control_list(&list);
            return match memfd(&body) {
                Ok(fd) => conn
                    .send_with_fds(
                        &wire::encode_control_reply(
                            seq,
                            &ControlReply::List(list.len().min(u16::MAX.into()) as u16, body.len()),
                        ),
                        &[fd.as_raw_fd()],
                    )
                    .map(drop),
                Err(err) => conn
                    .send(&wire::encode_control_reply(
                        seq,
                        &refused(format!("control list: {err}")),
                    ))
                    .map(drop),
            };
        }
        ControlOp::Subscribe => {
            if !subscribed.iter().any(|(c, _)| Arc::ptr_eq(c, &camera)) {
                let key = camera.subscribe(conn.try_clone_socket()?);
                subscribed.push((camera, key));
            }
            ControlReply::Subscribed
        }
    };
    conn.send(&wire::encode_control_reply(seq, &reply))
        .map(drop)
}
