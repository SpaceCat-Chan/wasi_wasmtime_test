use std::{
    collections::HashSet,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use tokio::io::{AsyncRead, AsyncWrite};

use colony::{Colony, FlaggedColony};
use futures::Future;
use tokio::net::TcpStream;
use wasi_common::WasiCtx;
use wasmtime::{Caller, Engine, Linker, Module, Store};
use wasmtime_wasi::WasiCtxBuilder;

struct WasmResources {
    wasi: WasiCtx,
    // an option so we can do .take to solve some lifetime shenanigans
    // will never actually be None long term
    tcp_streams: FlaggedColony<Option<Pin<Box<TcpStream>>>>,
    in_progress_connects: FlaggedColony<Pin<Box<dyn Future<Output = std::io::Result<TcpStream>>>>>,

    task_sender: std::sync::mpsc::Sender<u32>,
    waker_reciever: tokio::sync::watch::Receiver<Option<Waker>>,
}

impl WasmResources {
    fn new(
        wasi: WasiCtx,
        task_sender: std::sync::mpsc::Sender<u32>,
        waker_reciever: tokio::sync::watch::Receiver<Option<Waker>>,
    ) -> Self {
        Self {
            wasi,
            tcp_streams: Colony::flagged(),
            in_progress_connects: Colony::flagged(),
            task_sender,
            waker_reciever,
        }
    }
}

fn main() {
    let engine = Engine::default();
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::add_to_linker(&mut linker, |s: &mut WasmResources| &mut s.wasi).unwrap();
    let wasi = WasiCtxBuilder::new()
        .inherit_args()
        .unwrap()
        .inherit_env()
        .unwrap()
        .inherit_stdio()
        .build();
    wasi.push_dir(
        Box::new(wasmtime_wasi::dir::Dir::from_cap_std(
            wasmtime_wasi::Dir::open_ambient_dir("..", wasmtime_wasi::ambient_authority()).unwrap(),
        )),
        ".".into(),
    )
    .unwrap();

    let (task_sender, task_reciever) = std::sync::mpsc::channel();
    let (waker_sender, waker_reciever) = tokio::sync::watch::channel(None);

    let mut store = Store::new(
        &engine,
        WasmResources::new(wasi, task_sender, waker_reciever),
    );

    linker
        .func_wrap("env", "extern_clone_waker", |waker: u64| {
            let waker = unsafe { Box::from_raw(waker as *mut Waker) };
            let new_waker = waker.clone();
            Box::into_raw(waker);
            Box::into_raw(new_waker) as u64
        })
        .unwrap();

    linker
        .func_wrap("env", "extern_wake_waker", |waker: u64| {
            let waker = unsafe { Box::from_raw(waker as *mut Waker) };
            waker.wake();
        })
        .unwrap();

    linker
        .func_wrap("env", "extern_wake_by_ref_waker", |waker: u64| {
            let waker = unsafe { Box::from_raw(waker as *mut Waker) };
            waker.wake_by_ref();
            Box::into_raw(waker);
        })
        .unwrap();

    linker
        .func_wrap("env", "extern_drop_waker", |waker: u64| {
            unsafe { Box::from_raw(waker as *mut Waker) };
        })
        .unwrap();

    linker
        .func_wrap(
            "env",
            "extern_spawn_task",
            |mut caller: Caller<WasmResources>, task: u32| {
                println!("host: spawning task {}", task);
                if let Some(ref waker) = *caller.data_mut().waker_reciever.borrow() {
                    waker.wake_by_ref();
                }
                let _ = caller.data_mut().task_sender.send(task);
            },
        )
        .unwrap();

    linker
        .func_wrap(
            "env",
            "tcp_connect",
            |mut caller: Caller<WasmResources>, address: u32, port: u32| {
                println!("host: tcp_connect called with addr {address} and port {port}");
                let in_progress_futures = &mut caller.data_mut().in_progress_connects;
                let fut = Box::pin(TcpStream::connect(std::net::SocketAddrV4::new(
                    address.into(),
                    port as u16,
                )));
                in_progress_futures.insert(fut) as u64
            },
        )
        .unwrap();

    linker
        .func_wrap(
            "env",
            "poll_tcp_connect",
            |mut caller: Caller<WasmResources>, future: u64, waker: u64, res_loc: u32| {
                println!("host: poll_tcp_connect called");
                let in_progress_futures = caller
                    .data_mut()
                    .in_progress_connects
                    .get_mut(future as usize)
                    .unwrap();
                let waker = unsafe { Box::from_raw(waker as *mut Waker) };
                let mut cx = Context::from_waker(&waker);
                let result = in_progress_futures.as_mut().poll(&mut cx);

                match result {
                    Poll::Pending => {
                        println!("host: still pending");
                        0
                    }
                    Poll::Ready(res) => {
                        caller.data_mut().in_progress_connects.remove(future as _);
                        let write_result = caller
                            .get_export("write_tcp_open_results")
                            .unwrap()
                            .into_func()
                            .unwrap()
                            .typed::<(u32, u64, u32, u32), ()>(&caller)
                            .unwrap();
                        match res {
                            Ok(stream) => {
                                println!("host: connected");
                                let handle =
                                    caller.data_mut().tcp_streams.insert(Some(Box::pin(stream)));
                                write_result
                                    .call(&mut caller, (res_loc, handle as u64, 0, 0))
                                    .unwrap();
                            }
                            Err(e) => {
                                println!("host: error connecting");
                                write_result
                                    .call(&mut caller, (res_loc, 0, e.kind() as u32, 1))
                                    .unwrap();
                            }
                        }
                        1
                    }
                }
            },
        )
        .unwrap();

    linker
        .func_wrap(
            "env",
            "tcp_read_poll",
            |mut caller: Caller<WasmResources>,
             handle: u64,
             buf: u32,
             buf_length: u32,
             waker: u64| {
                println!("host: tcp read poll");
                let mut stream = caller
                    .data_mut()
                    .tcp_streams
                    .get_mut(handle as _)
                    .unwrap()
                    .take()
                    .unwrap();

                let waker = unsafe { Box::from_raw(waker as *mut Waker) };
                let mut cx = Context::from_waker(&waker);

                let memory = caller.get_export("memory").unwrap().into_memory().unwrap();
                let buffer =
                    &mut memory.data_mut(&mut caller)[(buf as _)..((buf + buf_length) as usize)];

                let mut buffer = tokio::io::ReadBuf::new(buffer);

                let result = stream.as_mut().poll_read(&mut cx, &mut buffer);

                println!("host: {:?}", result);

                let result = match result {
                    Poll::Pending => 0,
                    Poll::Ready(Ok(())) => 1u64 | ((buffer.filled().len() as u64) << 32),
                    Poll::Ready(Err(e)) => e.kind() as u64 + 2,
                };
                *caller.data_mut().tcp_streams.get_mut(handle as _).unwrap() = Some(stream);
                result
            },
        )
        .unwrap();

    linker
        .func_wrap(
            "env",
            "tcp_write_poll",
            |mut caller: Caller<WasmResources>,
             handle: u64,
             buf: u32,
             buf_length: u32,
             waker: u64| {
                println!("host: tcp write poll");
                let mut stream = caller
                    .data_mut()
                    .tcp_streams
                    .get_mut(handle as _)
                    .unwrap()
                    .take()
                    .unwrap();

                let waker = unsafe { Box::from_raw(waker as *mut Waker) };
                let mut cx = Context::from_waker(&waker);

                let memory = &caller
                    .get_export("memory")
                    .unwrap()
                    .into_memory()
                    .unwrap()
                    .data(&caller)[(buf as _)..((buf + buf_length) as _)];

                let result = stream.as_mut().poll_write(&mut cx, memory);
                *caller.data_mut().tcp_streams.get_mut(handle as _).unwrap() = Some(stream);

                println!("host: {:?}", result);
                match result {
                    Poll::Pending => 0,
                    Poll::Ready(Ok(len)) => 1 | ((len as u64) << 32),
                    Poll::Ready(Err(e)) => e.kind() as u64 + 2,
                }
            },
        )
        .unwrap();

    linker
        .func_wrap(
            "env",
            "tcp_flush_poll",
            |mut caller: Caller<WasmResources>, handle: u64, waker: u64| {
                println!("host: tcp flush poll");
                let mut stream = caller
                    .data_mut()
                    .tcp_streams
                    .get_mut(handle as _)
                    .unwrap()
                    .take()
                    .unwrap();

                let waker = unsafe { Box::from_raw(waker as *mut Waker) };
                let mut cx = Context::from_waker(&waker);

                let result = stream.as_mut().poll_flush(&mut cx);
                *caller.data_mut().tcp_streams.get_mut(handle as _).unwrap() = Some(stream);

                println!("host: {:?}", result);
                match result {
                    Poll::Pending => 0,
                    Poll::Ready(Ok(())) => 1,
                    Poll::Ready(Err(e)) => e.kind() as u32 + 2,
                }
            },
        )
        .unwrap();

    linker
        .func_wrap(
            "env",
            "tcp_shutdown_poll",
            |mut caller: Caller<WasmResources>, handle: u64, waker: u64| {
                println!("host: tcp shutdown poll");
                let mut stream = caller
                    .data_mut()
                    .tcp_streams
                    .get_mut(handle as _)
                    .unwrap()
                    .take()
                    .unwrap();

                let waker = unsafe { Box::from_raw(waker as *mut Waker) };
                let mut cx = Context::from_waker(&waker);

                let result = stream.as_mut().poll_shutdown(&mut cx);
                *caller.data_mut().tcp_streams.get_mut(handle as _).unwrap() = Some(stream);

                println!("host: {:?}", result);
                match result {
                    Poll::Pending => 0,
                    Poll::Ready(Ok(())) => 1,
                    Poll::Ready(Err(e)) => e.kind() as u32 + 2,
                }
            },
        )
        .unwrap();

    let module = Module::new(
        &engine,
        std::fs::read("../wasi/target/wasm32-wasi/release/wasi.wasm").unwrap(),
    )
    .unwrap();
    linker.module(&mut store, "main", &module).unwrap();

    linker
        .get(&mut store, "main", "start")
        .unwrap()
        .into_func()
        .unwrap()
        .typed::<(), ()>(&store)
        .unwrap()
        .call(&mut store, ())
        .unwrap();

    let future = WasmFutures {
        futures: Colony::new(),
        task_reciever,
        waker_sender,
        store,
        linker,
    };

    let exec = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    exec.block_on(future);
}

struct WasmFutures {
    futures: Colony<u32>,
    task_reciever: std::sync::mpsc::Receiver<u32>,
    waker_sender: tokio::sync::watch::Sender<Option<Waker>>,
    store: Store<WasmResources>,
    linker: Linker<WasmResources>,
}

impl Future for WasmFutures {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.waker_sender.send(Some(cx.waker().clone())).unwrap();
        while let Ok(task) = self.task_reciever.try_recv() {
            self.futures.insert(task);
        }

        if self.futures.is_empty() {
            return Poll::Ready(());
        }
        let this = self.get_mut();

        let poll_func = this
            .linker
            .get(&mut this.store, "main", "poll")
            .unwrap()
            .into_func()
            .unwrap()
            .typed::<(u32, u64), u32>(&this.store)
            .unwrap();

        let mut to_remove = HashSet::new();

        for future in this.futures.iter() {
            let waker = Box::into_raw(Box::new(cx.waker().clone())) as u64;
            println!("host: polling {}", future.1);
            let res = poll_func.call(&mut this.store, (*future.1, waker)).unwrap();
            if res == 1 {
                to_remove.insert(future.0);
                println!("host: {} finished", future.1);
            }
        }

        for remove in to_remove.into_iter() {
            this.futures.remove(remove);
        }

        if this.futures.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}
