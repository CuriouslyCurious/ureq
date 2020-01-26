use crate::Error;
use crate::Stream;
use futures_util::future::poll_fn;
use std::future::Future;
use std::task::Poll;
use std::time::Duration;

// TODO does this cause memory leaks?
pub async fn never() {
    poll_fn::<(), _>(|_| Poll::Pending).await;
    unreachable!()
}

#[cfg(feature = "async-std")]
pub mod exec {
    use super::*;
    use async_std_lib::net::TcpStream;
    use async_std_lib::task;

    pub struct AsyncImpl;

    impl Stream for TcpStream {}

    impl AsyncImpl {
        pub async fn connect_tcp(addr: &str) -> Result<impl Stream, Error> {
            Ok(TcpStream::connect(addr).await?)
        }

        pub fn spawn<T>(task: T)
        where
            T: Future + Send + 'static,
        {
            async_std_lib::task::spawn(async move {
                task.await;
            });
        }

        pub async fn timeout(duration: Duration) {
            async_std_lib::future::timeout(duration, never()).await.ok();
        }

        pub fn block_on<F: Future>(future: F) -> F::Output {
            task::block_on(future)
        }
    }
}

#[cfg(all(feature = "tokio", not(feature = "async-std")))]
pub mod exec {
    use super::*;
    use crate::tokio::from_tokio;
    use once_cell::sync::OnceCell;
    use std::sync::Mutex;
    use tokio_lib::net::TcpStream;
    use tokio_lib::runtime::{Builder, Handle, Runtime};

    static RUNTIME: OnceCell<Mutex<Runtime>> = OnceCell::new();
    static HANDLE: OnceCell<Handle> = OnceCell::new();

    pub struct AsyncImpl;

    impl AsyncImpl {
        pub async fn connect_tcp(addr: &str) -> Result<impl Stream, Error> {
            Ok(from_tokio(TcpStream::connect(addr).await?))
        }

        pub async fn timeout(duration: Duration) {
            tokio_lib::time::delay_for(duration).await;
        }

        pub fn spawn<T>(task: T)
        where
            T: Future + Send + 'static,
        {
            with_handle(|h| {
                h.spawn(async move {
                    task.await;
                });
            });
        }

        pub fn block_on<F: Future>(future: F) -> F::Output {
            with_runtime(|rt| rt.block_on(future))
        }
    }

    fn create_default_runtime() -> (Handle, Runtime) {
        let runtime = Builder::new()
            .basic_scheduler()
            .enable_io()
            .enable_time()
            .build()
            .expect("Failed to build tokio runtime");
        let handle = runtime.handle().clone();
        (handle, runtime)
    }

    fn with_runtime<F: FnOnce(&mut tokio_lib::runtime::Runtime) -> R, R>(f: F) -> R {
        let mut rt = RUNTIME
            .get_or_init(|| {
                let (h, rt) = create_default_runtime();
                HANDLE.set(h).expect("Failed to set HANDLE");
                Mutex::new(rt)
            })
            .lock()
            .unwrap();
        f(&mut rt)
    }

    fn with_handle<F: FnOnce(tokio_lib::runtime::Handle)>(f: F) {
        let h = {
            HANDLE
                .get_or_init(|| {
                    let (h, rt) = create_default_runtime();
                    RUNTIME.set(Mutex::new(rt)).expect("Failed to set RUNTIME");
                    h
                })
                .clone()
        };
        f(h)
    }
}
