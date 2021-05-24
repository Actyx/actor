use crate::{FutureBox, FutureResultBox, Mailbox, Receiver, Spawner};
use std::{future::Future, pin::Pin, task::Poll};
use tokio::{runtime::Runtime, sync::mpsc};

/// Spawner that uses the current tokio context
///
/// Will fail when used on a non-tokio thread.
pub struct TokioSpawner;

impl Spawner for TokioSpawner {
    fn spawn(&self, fut: FutureBox) -> FutureResultBox {
        let fut = tokio::spawn(fut);
        Box::pin(async move {
            match fut.await {
                Ok(result) => Ok(result),
                Err(err) => Err(err.into()),
            }
        })
    }
}

/// Spawner that uses the given tokio runtime
pub struct TokioRuntimeSpawner(pub Runtime);

impl Spawner for TokioRuntimeSpawner {
    fn spawn(&self, fut: FutureBox) -> FutureResultBox {
        let fut = self.0.spawn(fut);
        Box::pin(async move {
            match fut.await {
                Ok(result) => Ok(result),
                Err(err) => Err(err.into()),
            }
        })
    }
}

pub struct TokioMailbox;

impl Mailbox for TokioMailbox {
    fn make_mailbox<M: Send + 'static>(&self) -> (super::ActorRef<M>, Box<dyn Receiver<M>>) {
        let (tx, rx) = mpsc::unbounded_channel::<M>();
        let aref = super::ActorRef::new(Box::new(move |msg| {
            let _ = tx.send(msg);
        }));
        (aref, Box::new(TokioReceiver(rx)))
    }
}

pub struct TokioReceiver<M>(mpsc::UnboundedReceiver<M>);

impl<M: Send + 'static> super::Receiver<M> for TokioReceiver<M> {
    fn receive(&mut self) -> &mut (dyn Future<Output = anyhow::Result<M>> + Send + Unpin + '_) {
        self
    }
}

impl<M: Send + 'static> Future for TokioReceiver<M> {
    type Output = anyhow::Result<M>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.as_mut().0.poll_recv(cx) {
            Poll::Ready(Some(msg)) => Poll::Ready(Ok(msg)),
            Poll::Ready(None) => Poll::Ready(Err(anyhow::anyhow!("channel closed"))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActorRef, Context, NoActorRef};
    use anyhow::Result;
    use futures::poll;
    use std::{task::Poll, thread::sleep, time::Duration};
    use tokio::sync::oneshot;

    async fn actor(mut ctx: Context<(String, ActorRef<String>)>) -> Result<()> {
        loop {
            let (name, sender) = ctx.receive().await?;
            let (responder, handle) = actor!(TokioMailbox, |ctx| {
                let m = ctx.receive().await?;
                sender.tell(format!("Hello {}!", m));
                Ok(())
            });
            responder.tell(name);
            let _ = handle.await;
        }
    }

    #[tokio::test]
    async fn smoke() {
        let (aref, join_handle) = actor!(TokioMailbox, TokioSpawner, fn actor(ctx));

        let (tx, rx) = oneshot::channel();
        let (receiver, jr) = actor!(TokioMailbox, TokioSpawner, |ctx| {
            let msg = ctx.receive().await?;
            let _ = tx.send(msg);
            Ok("buh")
        });
        aref.tell(("Fred".to_owned(), receiver));
        assert_eq!(rx.await.unwrap(), "Hello Fred!");
        assert_eq!(jr.await.unwrap().unwrap(), "buh");

        let (tx, rx) = oneshot::channel();
        let (receiver, jr) = actor!(TokioMailbox, TokioSpawner, |ctx| {
            let msg = ctx.receive().await?;
            let _ = tx.send(msg);
            Ok(42)
        });
        aref.tell(("Barney".to_owned(), receiver));
        assert_eq!(rx.await.unwrap(), "Hello Barney!");
        assert_eq!(jr.await.unwrap().unwrap(), 42);

        drop(aref);
        join_handle
            .await
            .unwrap()
            .unwrap_err()
            .downcast::<NoActorRef>()
            .unwrap();
    }

    #[tokio::test]
    async fn dropped() {
        let (tx, mut rx) = oneshot::channel();
        let (aref, handle) = actor!(TokioMailbox, TokioSpawner, |ctx| {
            let result: Result<()> = ctx.receive().await;
            let _ = tx.send(result);
            Ok(())
        });

        sleep(Duration::from_millis(200));
        match poll!(&mut rx) {
            Poll::Pending => {}
            x => panic!("unexpected result: {:?}", x),
        }

        drop(aref);
        handle.await.unwrap().unwrap();
        let err = match poll!(rx) {
            Poll::Ready(Ok(e)) => e.unwrap_err(),
            x => panic!("unexpected poll result: {:?}", x),
        };
        err.downcast::<NoActorRef>()
            .unwrap_or_else(|e| panic!("unexpected error type: {}", e));
    }
}
