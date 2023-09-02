#![feature(waker_getters)]

use std::{
    future::Future,
    io::Write,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    thread::spawn,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::oneshot,
};

extern "C" {
    fn extern_clone_waker(data: u64) -> u64;
    fn extern_wake_waker(data: u64);
    fn extern_wake_by_ref_waker(data: u64);
    fn extern_drop_waker(data: u64);

    fn extern_spawn_task(future: *mut ());

    fn tcp_connect(address: u32, port: u16) -> u64;
    fn poll_tcp_connect(future: u64, waker: u64, result_location: *mut TcpConnectFuture) -> bool;
    fn tcp_read_poll(stream: u64, but: *mut u8, buf_length: u32, waker: u64) -> u64;
    fn tcp_write_poll(stream: u64, buf: *const u8, buf_length: u32, waker: u64) -> u64;
    fn tcp_flush_poll(stream: u64, waker: u64) -> u8;
    fn tcp_shutdown_poll(stream: u64, waker: u64) -> u8;
}

/// # Safety
/// result_location must point to a valid TcpConnectFuture
#[no_mangle]
pub unsafe extern "C" fn write_tcp_open_results(
    result_location: *mut TcpConnectFuture,
    handle: u64,
    error: u32,
    is_error: bool,
) {
    unsafe {
        let result_location = &mut *result_location;
        result_location.results = Some(if is_error {
            Err(std::mem::transmute::<_, std::io::ErrorKind>(error as u8).into())
        } else {
            Ok(TcpWrapper(handle))
        })
    }
}

pub struct TcpConnectFuture {
    future: u64,
    results: Option<Result<TcpWrapper, std::io::Error>>,
}

impl Future for TcpConnectFuture {
    type Output = Result<TcpWrapper, std::io::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let is_done = poll_tcp_connect(
                self.future,
                unwrap_waker(cx.waker().clone()),
                self.as_mut().get_mut(),
            );
            if is_done {
                Poll::Ready(self.results.take().unwrap())
            } else {
                Poll::Pending
            }
        }
    }
}

fn open_tcp(address: std::net::SocketAddrV4) -> TcpConnectFuture {
    let future = unsafe { tcp_connect((*address.ip()).into(), address.port()) };
    TcpConnectFuture {
        future,
        results: None,
    }
}

/// # Safety
/// `waker` must be an instance of a double boxed host waker, and the resulting `u64` must eventually be passed to the host, either through `extern_wake_waker`, `extern_drop_waker`, or another host function which ensures it is cleaned up
unsafe fn unwrap_waker(waker: Waker) -> u64 {
    let underlying_waker = std::mem::transmute::<Waker, RawWaker>(waker);

    *Box::from_raw(underlying_waker.data() as *mut u64)
}

pub struct TcpWrapper(u64);

impl AsyncRead for TcpWrapper {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        unsafe {
            let result = tcp_read_poll(
                self.0,
                buf.unfilled_mut().as_mut_ptr() as *mut u8,
                buf.remaining() as u32,
                unwrap_waker(cx.waker().clone()),
            );

            if result & 0xffffffff == 0 {
                Poll::Pending
            } else if result & 0xffffffff == 1 {
                let read_amount = result >> 32;

                buf.assume_init(read_amount as _);
                buf.advance(read_amount as _);

                Poll::Ready(Ok(()))
            } else {
                let error_kind = (result & 0xffffffff) - 2;

                Poll::Ready(Err(std::mem::transmute::<u8, std::io::ErrorKind>(
                    error_kind as _,
                )
                .into()))
            }
        }
    }
}

impl AsyncWrite for TcpWrapper {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let result = unsafe {
            tcp_write_poll(
                self.0,
                buf.as_ptr(),
                buf.len() as _,
                unwrap_waker(cx.waker().clone()),
            )
        };
        if (result & 0xffffffff) == 0 {
            Poll::Pending
        } else if (result & 0xffffffff) == 1 {
            Poll::Ready(Ok((result >> 32) as _))
        } else {
            Poll::Ready(Err(unsafe {
                std::mem::transmute::<u8, std::io::ErrorKind>(((result & 0xffffffff) - 2) as _)
            }
            .into()))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let result = unsafe { tcp_flush_poll(self.0, unwrap_waker(cx.waker().clone())) };
        if result == 0 {
            Poll::Pending
        } else if result == 1 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(unsafe {
                std::mem::transmute::<_, std::io::ErrorKind>(result - 2)
            }
            .into()))
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let result = unsafe { tcp_shutdown_poll(self.0, unwrap_waker(cx.waker().clone())) };
        if result == 0 {
            Poll::Pending
        } else if result == 1 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(unsafe {
                std::mem::transmute::<_, std::io::ErrorKind>(result - 2)
            }
            .into()))
        }
    }
}

#[no_mangle]
pub extern "C" fn start() {
    // returns a handle to the main future
    spawn_task(a_main());
}

#[no_mangle]
pub extern "C" fn poll(main: *mut (), waker: u64) -> u8 {
    let waker = RawWaker::new(Box::into_raw(Box::new(waker)) as _, &VTABLE);
    let waker = unsafe { Waker::from_raw(waker) };
    let mut cx = Context::from_waker(&waker);
    let main = unsafe { &mut *(main as *mut FutureWrapper) };

    match main.fut.as_mut().poll(&mut cx) {
        Poll::Pending => 0,
        Poll::Ready(()) => 1,
    }
}

#[no_mangle]
pub extern "C" fn drop_future(future: *mut ()) {
    drop(unsafe { Box::from_raw(future as *mut FutureWrapper) });
}

const VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake_waker, wake_by_ref_waker, drop_waker);

unsafe fn clone_waker(data: *const ()) -> RawWaker {
    let data = Box::from_raw(data as *mut u64);
    let new_data = Box::new(extern_clone_waker(*data));
    Box::into_raw(data);
    RawWaker::new(Box::into_raw(new_data) as *const (), &VTABLE)
}
unsafe fn wake_waker(data: *const ()) {
    let data = Box::from_raw(data as *mut u64);
    extern_wake_waker(*data);
}
unsafe fn wake_by_ref_waker(data: *const ()) {
    let data = Box::from_raw(data as *mut u64);
    extern_wake_by_ref_waker(*data);
    Box::into_raw(data);
}
unsafe fn drop_waker(data: *const ()) {
    let data = Box::from_raw(data as *mut u64);
    extern_drop_waker(*data);
}

struct FutureWrapper {
    fut: Pin<Box<dyn Future<Output = ()>>>,
}

impl FutureWrapper {
    fn new(fut: impl Future<Output = ()> + 'static) -> Self {
        FutureWrapper { fut: Box::pin(fut) }
    }
}

struct Task<T> {
    result: oneshot::Receiver<T>,
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe { self.map_unchecked_mut(|s| &mut s.result) }
            .poll(cx)
            .map(|r| r.unwrap())
    }
}

fn spawn_task<T: 'static>(fut: impl Future<Output = T> + 'static) -> Task<T> {
    let (tx, rx) = oneshot::channel();
    let future = Box::new(FutureWrapper::new(async {
        let res = fut.await;
        let _ = tx.send(res);
    }));

    unsafe { extern_spawn_task(Box::into_raw(future) as *mut ()) }
    // awoo

    Task { result: rx }
}

// APPLICATION SHIT FROM HERE DOWN:

async fn a_main() {
    let client: hyper::Client<_, hyper::Body> = hyper::Client::builder()
        .executor(TaskSpawner)
        .build(TcpConnector);

    let mut response = client
        .get("http://google.com/file12".parse().unwrap())
        .await
        .unwrap();

    println!("got the response! {:?}", response);

    let body = hyper::body::to_bytes(response.body_mut()).await.unwrap();

    println!("with body:\n{}", String::from_utf8_lossy(&body[..]));
}

struct TaskSpawner;

impl<F> hyper::rt::Executor<F> for TaskSpawner
where
    F: Future + 'static,
{
    fn execute(&self, fut: F) {
        spawn_task(fut);
    }
}

#[derive(Clone)]
struct TcpConnector;

impl tower::Service<hyper::Uri> for TcpConnector {
    type Response = TcpWrapper;

    type Error = std::io::Error;

    type Future = TcpConnectFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: hyper::Uri) -> Self::Future {
        open_tcp(std::net::SocketAddrV4::new(
            [10, 0, 0, 10].into(),
            req.port().map(|p| p.as_u16()).unwrap_or(4444),
        ))
    }
}

impl hyper::client::connect::Connection for TcpWrapper {
    fn connected(&self) -> hyper::client::connect::Connected {
        hyper::client::connect::Connected::new()
    }
}
