use super::{RecvOutcome, SendOutcome, bounded};
use tokio::runtime::Builder;

#[test]
fn recv_async_observes_send_that_happened_before_wait_registration() {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(async {
        let (tx, rx) = bounded(1);
        assert_eq!(tx.send(7), SendOutcome::Ok);

        match rx.recv_async().await {
            RecvOutcome::Data(value) => assert_eq!(value, 7),
            other => panic!("expected queued value, got {other:?}"),
        }
    });
}

#[test]
fn send_async_observes_capacity_that_happened_before_wait_registration() {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(async {
        let (tx, rx) = bounded(1);
        assert_eq!(tx.send(1), SendOutcome::Ok);
        assert!(matches!(rx.recv(), RecvOutcome::Data(1)));

        assert_eq!(tx.send_async(2).await, SendOutcome::Ok);
        assert!(matches!(rx.recv(), RecvOutcome::Data(2)));
    });
}

#[test]
fn recv_async_observes_close_that_happened_before_wait_registration() {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(async {
        let (_tx, rx) = bounded::<u8>(1);
        rx.close();

        assert!(matches!(rx.recv_async().await, RecvOutcome::Closed));
    });
}

#[test]
fn send_async_observes_close_that_happened_before_wait_registration() {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(async {
        let (tx, rx) = bounded::<u8>(1);
        rx.close();

        assert_eq!(tx.send_async(7).await, SendOutcome::Closed);
    });
}

#[test]
fn recv_async_wakes_on_close() {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(async {
        let (_tx, rx) = bounded::<u8>(1);
        let waiter = tokio::spawn({
            let rx = rx.clone();
            async move { rx.recv_async().await }
        });
        tokio::task::yield_now().await;
        rx.close();

        assert!(matches!(waiter.await.expect("waiter"), RecvOutcome::Closed));
    });
}

#[test]
fn send_async_wakes_on_close() {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(async {
        let (tx, rx) = bounded(1);
        assert_eq!(tx.send(1), SendOutcome::Ok);
        let waiter = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send_async(2).await }
        });
        tokio::task::yield_now().await;
        rx.close();

        assert_eq!(waiter.await.expect("waiter"), SendOutcome::Closed);
    });
}

#[test]
fn async_waits_and_wakes_are_counted() {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(async {
        let (tx, rx) = bounded(1);
        let waiter = tokio::spawn({
            let rx = rx.clone();
            async move { rx.recv_async().await }
        });
        tokio::task::yield_now().await;
        assert_eq!(tx.send(3), SendOutcome::Ok);
        assert!(matches!(
            waiter.await.expect("waiter"),
            RecvOutcome::Data(3)
        ));

        let stats = rx.stats();
        assert_eq!(stats.async_recv_waits, 1);
        assert!(stats.async_recv_wakes >= 1);
    });
}

#[test]
fn cancelled_async_recv_waiter_does_not_consume_future_send() {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(async {
        let (tx, rx) = bounded(1);
        let waiter = tokio::spawn({
            let rx = rx.clone();
            async move { rx.recv_async().await }
        });
        tokio::task::yield_now().await;
        waiter.abort();
        assert!(
            waiter
                .await
                .expect_err("waiter should be cancelled")
                .is_cancelled()
        );

        assert_eq!(tx.send_async(9).await, SendOutcome::Ok);
        assert!(matches!(rx.recv_async().await, RecvOutcome::Data(9)));
    });
}

#[test]
fn cancelled_async_send_waiter_does_not_block_future_capacity() {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(async {
        let (tx, rx) = bounded(1);
        assert_eq!(tx.send(1), SendOutcome::Ok);
        let waiter = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send_async(2).await }
        });
        tokio::task::yield_now().await;
        waiter.abort();
        assert!(
            waiter
                .await
                .expect_err("waiter should be cancelled")
                .is_cancelled()
        );

        assert!(matches!(rx.recv_async().await, RecvOutcome::Data(1)));
        assert_eq!(tx.send_async(3).await, SendOutcome::Ok);
        assert!(matches!(rx.recv_async().await, RecvOutcome::Data(3)));
    });
}
