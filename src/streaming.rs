use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::OwnedSemaphorePermit,
};

pub struct BodyReader {
    rx: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    pending: std::io::Cursor<Vec<u8>>,
}
impl AsyncRead for BodyReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pending.position() as usize == self.pending.get_ref().len() {
            match self.rx.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e)),
                Poll::Ready(Some(Ok(chunk))) => self.pending = std::io::Cursor::new(chunk),
            }
        }
        let start = self.pending.position() as usize;
        let n = buf.remaining().min(self.pending.get_ref().len() - start);
        buf.put_slice(&self.pending.get_ref()[start..start + n]);
        self.pending.set_position((start + n) as u64);
        Poll::Ready(Ok(()))
    }
}
pub fn idle_stream(
    mut reader: Pin<Box<dyn AsyncRead + Send>>,
    permit: Arc<OwnedSemaphorePermit>,
    idle: Duration,
) -> tokio_util::io::ReaderStream<BodyReader> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        let _permit = permit;
        loop {
            let progress = async {
                let mut buf = vec![0; 64 * 1024];
                let mut read_buf = ReadBuf::new(&mut buf);
                std::future::poll_fn(|cx| reader.as_mut().poll_read(cx, &mut read_buf)).await?;
                let n = read_buf.filled().len();
                if n == 0 {
                    return Ok(false);
                }
                buf.truncate(n);
                Ok::<_, std::io::Error>(tx.send(Ok(buf)).await.is_ok())
            };
            tokio::select! {
                _ = tx.closed() => break,
                result = tokio::time::timeout(idle, progress) => match result {
                    Ok(Ok(true)) => {},
                    Ok(Ok(false)) => break,
                    Ok(Err(e)) => { let _ = tx.try_send(Err(e)); break; },
                    Err(_) => { let _ = tx.try_send(Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "response body idle timeout"))); break; }
                }
            }
        }
    });
    tokio_util::io::ReaderStream::new(BodyReader {
        rx,
        pending: std::io::Cursor::new(Vec::new()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn unpolled_body_releases_slot_on_timeout_and_drop() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::new(slots.clone().acquire_owned().await.unwrap());
        let stream = idle_stream(
            Box::pin(std::io::Cursor::new(vec![1; 256 * 1024])),
            permit,
            Duration::from_millis(20),
        );
        let permit = tokio::time::timeout(Duration::from_secs(1), slots.clone().acquire_owned())
            .await
            .unwrap()
            .unwrap();
        drop(stream);
        drop(permit);
        let permit = Arc::new(slots.clone().acquire_owned().await.unwrap());
        let stream = idle_stream(
            Box::pin(std::io::Cursor::new(vec![1; 256 * 1024])),
            permit,
            Duration::from_secs(60),
        );
        drop(stream);
        let _permit = tokio::time::timeout(Duration::from_secs(1), slots.acquire())
            .await
            .unwrap()
            .unwrap();
    }
}
