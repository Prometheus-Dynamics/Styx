use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsFd;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use super::sys::{self, EventFd};
use super::*;

const LONG: Duration = Duration::from_secs(5);

fn nonblocking_pipe() -> (AsyncFd<std::io::PipeReader>, std::io::PipeWriter) {
    let (reader, writer) = std::io::pipe().unwrap();
    set_nonblocking(reader.as_fd()).unwrap();
    (AsyncFd::new(reader).unwrap(), writer)
}

fn poll_once<F: Future>(future: F) -> Poll<F::Output> {
    let mut future = pin!(future);
    future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn readable_wakes_when_another_thread_writes() {
    let (reader, mut writer) = nonblocking_pipe();
    let started = Instant::now();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        writer.write_all(b"x").unwrap();
        writer
    });
    let ready = block_on(timeout(LONG, reader.readable())).unwrap().unwrap();
    assert!(ready.is_readable());
    assert!(started.elapsed() >= Duration::from_millis(25));
    drop(t.join().unwrap());
}

#[test]
fn read_with_retries_until_data_arrives() {
    let (reader, mut writer) = nonblocking_pipe();
    let t = thread::spawn(move || {
        for chunk in [b"ab", b"cd"] {
            thread::sleep(Duration::from_millis(10));
            writer.write_all(chunk).unwrap();
        }
    });
    let mut got = Vec::new();
    while got.len() < 4 {
        let mut buf = [0u8; 4];
        let n = block_on(reader.read_with(|mut r| r.read(&mut buf))).unwrap();
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, b"abcd");
    t.join().unwrap();
}

#[test]
fn writable_waits_for_room() {
    let (mut reader, writer) = std::io::pipe().unwrap();
    set_nonblocking(writer.as_fd()).unwrap();
    let writer = AsyncFd::new(writer).unwrap();
    let chunk = [0u8; 4096];
    let mut filled = 0usize;
    loop {
        match writer.get_ref().write(&chunk) {
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert!(
        poll_once(async { timeout(Duration::from_millis(20), writer.writable()).await })
            .is_pending()
    );
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        let mut buf = vec![0u8; filled];
        reader.read_exact(&mut buf).unwrap();
        reader
    });
    let ready = block_on(timeout(LONG, writer.writable())).unwrap().unwrap();
    assert!(ready.is_writable());
    drop(t.join().unwrap());
}

#[test]
fn priority_sees_out_of_band_data() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    server.set_nonblocking(true).unwrap();
    let server = AsyncFd::new(server).unwrap();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        sys::send_oob(client.as_fd()).unwrap();
        client
    });
    let ready = block_on(timeout(LONG, server.priority())).unwrap().unwrap();
    assert!(ready.is_priority(), "{ready:?}");
    drop(t.join().unwrap());
}

#[test]
fn free_functions_work_on_borrowed_descriptors() {
    let efd = Arc::new(EventFd::new().unwrap());
    let notifier = efd.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        notifier.notify().unwrap();
    });
    let ready = block_on(timeout(LONG, readable(efd.as_fd())))
        .unwrap()
        .unwrap();
    assert!(ready.is_readable());
    t.join().unwrap();
    // Also while the same descriptor is registered through an AsyncFd.
    let (reader, mut writer) = nonblocking_pipe();
    writer.write_all(b"z").unwrap();
    let ready = block_on(timeout(LONG, readable(reader.get_ref().as_fd())));
    assert!(ready.unwrap().unwrap().is_readable());
    assert!(block_on(timeout(LONG, writable(writer.as_fd()))).is_ok());
    let never = block_on(timeout(Duration::from_millis(10), priority(efd.as_fd())));
    assert_eq!(never.unwrap_err(), Elapsed);
}

#[test]
fn hangup_is_reported() {
    let (reader, writer) = nonblocking_pipe();
    drop(writer);
    let ready = block_on(timeout(LONG, reader.readable())).unwrap().unwrap();
    assert!(ready.is_hangup(), "{ready:?}");
}

#[test]
fn several_waiters_on_one_descriptor_all_wake() {
    let (reader, mut writer) = nonblocking_pipe();
    let reader = Arc::new(reader);
    let waiters: Vec<_> = (0..3)
        .map(|_| {
            let reader = reader.clone();
            thread::spawn(move || block_on(timeout(LONG, reader.readable())))
        })
        .collect();
    thread::sleep(Duration::from_millis(30));
    writer.write_all(b"x").unwrap();
    for w in waiters {
        assert!(w.join().unwrap().unwrap().unwrap().is_readable());
    }
}

#[test]
fn dropped_waiters_do_not_disturb_others() {
    let (reader, mut writer) = nonblocking_pipe();
    assert!(poll_once(reader.readable()).is_pending());
    assert!(poll_once(reader.ready(Interest::READABLE | Interest::PRIORITY)).is_pending());
    writer.write_all(b"x").unwrap();
    let ready = block_on(timeout(LONG, reader.readable())).unwrap().unwrap();
    assert!(ready.is_readable());
}

#[test]
fn into_inner_deregisters() {
    let (reader, mut writer) = nonblocking_pipe();
    let raw = reader.into_inner();
    // Registering again would fail with EEXIST had the first registration stayed.
    let reader = AsyncFd::new(raw).unwrap();
    writer.write_all(b"x").unwrap();
    assert!(block_on(timeout(LONG, reader.readable())).is_ok());
    let _ = format!("{reader:?}");
}

#[test]
fn private_reactor_works_and_stops() {
    let reactor = Reactor::new().unwrap();
    let (reader, mut writer) = std::io::pipe().unwrap();
    let reader = AsyncFd::with_reactor(reader, &reactor).unwrap();
    drop(reactor);
    writer.write_all(b"x").unwrap();
    assert!(block_on(timeout(LONG, reader.readable())).is_ok());
    drop(reader);
}

#[test]
fn sleep_waits_about_the_right_time() {
    let started = Instant::now();
    block_on(sleep(Duration::from_millis(30)));
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(30), "{took:?}");
    assert!(took < Duration::from_secs(1), "{took:?}");
    block_on(sleep(Duration::ZERO));
}

#[test]
fn timers_fire_in_deadline_order_across_threads() {
    let start = Instant::now();
    let handles: Vec<_> = [60u64, 10, 35]
        .into_iter()
        .map(|ms| {
            thread::spawn(move || {
                block_on(sleep_until(start + Duration::from_millis(ms)));
                (ms, start.elapsed())
            })
        })
        .collect();
    for h in handles {
        let (ms, took) = h.join().unwrap();
        assert!(took >= Duration::from_millis(ms), "{ms}: {took:?}");
        assert!(took < Duration::from_millis(ms + 500), "{ms}: {took:?}");
    }
}

#[test]
fn sleep_reset_and_cancel() {
    let mut tick = sleep(Duration::from_secs(60));
    assert!(poll_once(&mut tick).is_pending());
    assert!(!tick.is_elapsed());
    let next = Instant::now() + Duration::from_millis(10);
    tick.reset(next);
    assert_eq!(tick.deadline(), next);
    block_on(&mut tick);
    assert!(tick.is_elapsed());
    // A short sleep started after a long one still fires on time.
    let long = sleep(Duration::from_secs(60));
    assert!(poll_once(long).is_pending());
    let started = Instant::now();
    block_on(sleep(Duration::from_millis(10)));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn timeout_passes_results_through() {
    assert_eq!(block_on(timeout(LONG, async { 7 })), Ok(7));
    let (reader, _writer) = nonblocking_pipe();
    let result = block_on(timeout(Duration::from_millis(10), reader.readable()));
    assert_eq!(result.unwrap_err(), Elapsed);
    let io: std::io::Error = Elapsed.into();
    assert_eq!(io.kind(), std::io::ErrorKind::TimedOut);
}

#[test]
fn ready_and_interest_format() {
    assert_eq!(
        format!("{:?}", Ready::READABLE | Ready::HANGUP),
        "READABLE | HANGUP"
    );
    assert_eq!(
        format!("{:?}", Interest::READABLE | Interest::PRIORITY),
        "Interest(READABLE | PRIORITY)"
    );
    assert!(Ready::EMPTY.is_empty());
    assert!(Ready::ERROR.is_error());
    let r = Ready::from_epoll(sys::EPOLLIN | sys::EPOLLPRI);
    assert_eq!(r.matching(Interest::PRIORITY), Ready::PRIORITY);
    assert_eq!(r.matching(Interest::WRITABLE), Ready::EMPTY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn works_under_tokio_multi_thread() {
    let (reader, mut writer) = nonblocking_pipe();
    tokio::spawn(async move {
        sleep(Duration::from_millis(10)).await;
        writer.write_all(b"tokio").unwrap();
    });
    let mut buf = [0u8; 8];
    let n = timeout(LONG, reader.read_with(|mut r| r.read(&mut buf)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"tokio");
}

#[tokio::test(flavor = "current_thread")]
async fn works_under_tokio_current_thread() {
    let efd = EventFd::new().unwrap();
    let efd = AsyncFd::new(efd).unwrap();
    let waiting = async { efd.readable().await.unwrap() };
    let notify = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        efd.get_ref().notify().unwrap();
    };
    let (ready, ()) = tokio::join!(waiting, notify);
    assert!(ready.is_readable());
    assert!(
        timeout(Duration::from_millis(5), std::future::pending::<()>())
            .await
            .is_err()
    );
}

#[test]
fn poll_ready_wakes_the_task_that_polled_last() {
    let (reader, mut writer) = nonblocking_pipe();
    let poll = |reader: &AsyncFd<std::io::PipeReader>| {
        reader.poll_read_ready(&mut Context::from_waker(Waker::noop()))
    };
    assert!(poll(&reader).is_pending());
    assert!(poll(&reader).is_pending());
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        writer.write_all(b"x").unwrap();
        writer
    });
    let ready = block_on(timeout(
        LONG,
        std::future::poll_fn(|cx| reader.poll_read_ready(cx)),
    ))
    .unwrap()
    .unwrap();
    assert!(ready.is_readable());
    drop(t.join().unwrap());
}
